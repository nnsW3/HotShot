#![allow(dead_code)]

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_broadcast::{broadcast, Sender};
use async_lock::RwLockUpgradableReadGuard;
use committable::Committable;
use hotshot_types::{
    data::{Leaf, QuorumProposal},
    message::Proposal,
    simple_certificate::QuorumCertificate,
    traits::{
        election::Membership,
        node_implementation::{NodeImplementation, NodeType},
        storage::Storage,
        ValidatedState,
    },
    utils::{View, ViewInner},
    vote::{Certificate, HasViewNumber},
};
use tracing::{debug, error, warn};

use super::QuorumProposalRecvTaskState;
use crate::{
    consensus::{
        helpers::{
            fetch_proposal, validate_proposal_safety_and_liveness, validate_proposal_view_and_certs,
        },
        view_change::{update_view, SEND_VIEW_CHANGE_EVENT},
    },
    events::HotShotEvent,
    helpers::broadcast_event,
};

/// Whether the proposal contained in `QuorumProposalRecv` is fully validated or only the liveness
/// is checked.
pub(crate) enum QuorumProposalValidity {
    /// Fully validated.
    Fully,
    /// Not fully validated due to the parent information missing in the internal state, but the
    /// liveness is validated.
    Liveness,
}

/// Update states in the event that the parent state is not found for a given `proposal`.
async fn validate_proposal_liveness<TYPES: NodeType, I: NodeImplementation<TYPES>>(
    proposal: &Proposal<TYPES, QuorumProposal<TYPES>>,
    event_sender: &Sender<Arc<HotShotEvent<TYPES>>>,
    task_state: &mut QuorumProposalRecvTaskState<TYPES, I>,
) -> Result<QuorumProposalValidity> {
    let view_number = proposal.data.view_number();
    let mut consensus_write = task_state.consensus.write().await;

    let leaf = Leaf::from_quorum_proposal(&proposal.data);

    let state = Arc::new(
        <TYPES::ValidatedState as ValidatedState<TYPES>>::from_header(&proposal.data.block_header),
    );
    let view = View {
        view_inner: ViewInner::Leaf {
            leaf: leaf.commit(),
            state,
            delta: None, // May be updated to `Some` in the vote task.
        },
    };

    if let Err(e) = consensus_write.update_validated_state_map(view_number, view.clone()) {
        tracing::trace!("{e:?}");
    }
    consensus_write.update_saved_leaves(leaf.clone());

    if let Err(e) = task_state
        .storage
        .write()
        .await
        .update_undecided_state(
            consensus_write.saved_leaves().clone(),
            consensus_write.validated_state_map().clone(),
        )
        .await
    {
        warn!("Couldn't store undecided state.  Error: {:?}", e);
    }

    let liveness_check =
        proposal.data.justify_qc.clone().view_number() > consensus_write.locked_view();

    drop(consensus_write);

    // Broadcast that we've updated our consensus state so that other tasks know it's safe to grab.
    broadcast_event(
        HotShotEvent::ValidatedStateUpdated(view_number, view).into(),
        event_sender,
    )
    .await;

    if !liveness_check {
        bail!("Liveness invalid.");
    }

    Ok(QuorumProposalValidity::Liveness)
}

/// Handles the `QuorumProposalRecv` event by first validating the cert itself for the view, and then
/// updating the states, which runs when the proposal cannot be found in the internal state map.
///
/// This code can fail when:
/// - The justify qc is invalid.
/// - The task is internally inconsistent.
/// - The sequencer storage update fails.
#[allow(clippy::too_many_lines)]
pub(crate) async fn handle_quorum_proposal_recv<TYPES: NodeType, I: NodeImplementation<TYPES>>(
    proposal: &Proposal<TYPES, QuorumProposal<TYPES>>,
    sender: &TYPES::SignatureKey,
    event_sender: &Sender<Arc<HotShotEvent<TYPES>>>,
    task_state: &mut QuorumProposalRecvTaskState<TYPES, I>,
) -> Result<QuorumProposalValidity> {
    let sender = sender.clone();
    let cur_view = task_state.cur_view;

    validate_proposal_view_and_certs(
        proposal,
        &sender,
        task_state.cur_view,
        &task_state.quorum_membership,
        &task_state.timeout_membership,
    )
    .context("Failed to validate proposal view or attached certs")?;

    let view_number = proposal.data.view_number();
    let view_leader_key = task_state.quorum_membership.leader(view_number);
    let justify_qc = proposal.data.justify_qc.clone();

    if !justify_qc.is_valid_cert(task_state.quorum_membership.as_ref()) {
        let consensus = task_state.consensus.read().await;
        consensus.metrics.invalid_qc.update(1);
        bail!("Invalid justify_qc in proposal for view {}", *view_number);
    }

    // NOTE: We could update our view with a valid TC but invalid QC, but that is not what we do here
    if let Err(e) = update_view::<TYPES>(
        view_number,
        event_sender,
        task_state.timeout,
        Arc::clone(&task_state.consensus),
        &mut task_state.cur_view,
        &mut task_state.cur_view_time,
        &mut task_state.timeout_task,
        &task_state.output_event_stream,
        SEND_VIEW_CHANGE_EVENT,
        task_state.quorum_membership.leader(cur_view) == task_state.public_key,
    )
    .await
    {
        debug!("Failed to update view; error = {e:#}");
    }

    // Get the parent leaf and state.
    let mut parent_leaf = task_state
        .consensus
        .read()
        .await
        .saved_leaves()
        .get(&justify_qc.data.leaf_commit)
        .cloned();

    parent_leaf = match parent_leaf {
        Some(p) => Some(p),
        None => fetch_proposal(
            justify_qc.view_number(),
            event_sender.clone(),
            Arc::clone(&task_state.quorum_membership),
            Arc::clone(&task_state.consensus),
        )
        .await
        .ok(),
    };
    let consensus_read = task_state.consensus.read().await;

    let parent = match parent_leaf {
        Some(leaf) => {
            if let (Some(state), _) = consensus_read.state_and_delta(leaf.view_number()) {
                Some((leaf, Arc::clone(&state)))
            } else {
                bail!("Parent state not found! Consensus internally inconsistent");
            }
        }
        None => None,
    };

    if justify_qc.view_number() > consensus_read.high_qc().view_number {
        if let Err(e) = task_state
            .storage
            .write()
            .await
            .update_high_qc(justify_qc.clone())
            .await
        {
            bail!("Failed to store High QC, not voting; error = {:?}", e);
        }
    }
    drop(consensus_read);

    let mut consensus_write = task_state.consensus.write().await;
    if let Err(e) = consensus_write.update_high_qc(justify_qc.clone()) {
        tracing::trace!("{e:?}");
    }
    drop(consensus_write);

    broadcast_event(
        HotShotEvent::UpdateHighQc(justify_qc.clone()).into(),
        event_sender,
    )
    .await;

    let Some((parent_leaf, _parent_state)) = parent else {
        warn!(
            "Proposal's parent missing from storage with commitment: {:?}",
            justify_qc.data.leaf_commit
        );
        return validate_proposal_liveness(proposal, event_sender, task_state).await;
    };

    // Validate the proposal
    validate_proposal_safety_and_liveness(
        proposal.clone(),
        parent_leaf,
        Arc::clone(&task_state.consensus),
        None,
        Arc::clone(&task_state.quorum_membership),
        view_leader_key,
        event_sender.clone(),
        sender,
        task_state.output_event_stream.clone(),
    )
    .await?;

    Ok(QuorumProposalValidity::Fully)
}
