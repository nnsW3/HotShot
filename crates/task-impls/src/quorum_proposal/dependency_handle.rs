//! This module holds the dependency task for the QuorumProposalTask. It is spawned whenever an event that could
//! initiate a proposal occurs.

use std::{marker::PhantomData, sync::Arc, time::Duration};

use anyhow::{ensure, Context, Result};
use async_broadcast::{Receiver, Sender};
use async_compatibility_layer::art::{async_sleep, async_spawn};
use async_lock::RwLock;
use committable::Committable;
use hotshot_task::{
    dependency::{Dependency, EventDependency},
    dependency_task::HandleDepOutput,
};
use hotshot_types::{
    consensus::{CommitmentAndMetadata, Consensus},
    data::{Leaf, QuorumProposal, VidDisperse, ViewChangeEvidence},
    message::Proposal,
    traits::{
        block_contents::BlockHeader, node_implementation::NodeType, signature_key::SignatureKey,
    },
};
use tracing::{debug, error};
use vbs::version::Version;

use crate::{
    consensus::helpers::{fetch_proposal, parent_leaf_and_state},
    events::HotShotEvent,
    helpers::broadcast_event,
};

/// Proposal dependency types. These types represent events that precipitate a proposal.
#[derive(PartialEq, Debug)]
pub(crate) enum ProposalDependency {
    /// For the `SendPayloadCommitmentAndMetadata` event.
    PayloadAndMetadata,

    /// For the `QcFormed` event.
    Qc,

    /// For the `ViewSyncFinalizeCertificate2Recv` event.
    ViewSyncCert,

    /// For the `QcFormed` event timeout branch.
    TimeoutCert,

    /// For the `QuroumProposalRecv` event.
    Proposal,

    /// For the `VidShareValidated` event.
    VidShare,
}

/// Handler for the proposal dependency
pub struct ProposalDependencyHandle<TYPES: NodeType> {
    /// Latest view number that has been proposed for (proxy for cur_view).
    pub latest_proposed_view: TYPES::Time,

    /// The view number to propose for.
    pub view_number: TYPES::Time,

    /// The event sender.
    pub sender: Sender<Arc<HotShotEvent<TYPES>>>,

    /// The event receiver.
    pub receiver: Receiver<Arc<HotShotEvent<TYPES>>>,

    /// Immutable instance state
    pub instance_state: Arc<TYPES::InstanceState>,

    /// Membership for Quorum Certs/votes
    pub quorum_membership: Arc<TYPES::Membership>,

    /// Our public key
    pub public_key: TYPES::SignatureKey,

    /// Our Private Key
    pub private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,

    /// Round start delay from config, in milliseconds.
    pub round_start_delay: u64,

    /// Shared consensus task state
    pub consensus: Arc<RwLock<Consensus<TYPES>>>,

    /// The current version of consensus
    pub version: Version,
}

impl<TYPES: NodeType> ProposalDependencyHandle<TYPES> {
    /// Publishes a proposal given the [`CommitmentAndMetadata`], [`VidDisperse`]
    /// and high qc [`hotshot_types::simple_certificate::QuorumCertificate`],
    /// with optional [`ViewChangeEvidence`].
    async fn publish_proposal(
        &self,
        commitment_and_metadata: CommitmentAndMetadata<TYPES>,
        vid_share: Proposal<TYPES, VidDisperse<TYPES>>,
        view_change_evidence: Option<ViewChangeEvidence<TYPES>>,
    ) -> Result<()> {
        let (parent_leaf, state) = parent_leaf_and_state(
            self.view_number,
            Arc::clone(&self.quorum_membership),
            self.public_key.clone(),
            Arc::clone(&self.consensus),
        )
        .await?;

        let proposal_certificate = view_change_evidence
            .as_ref()
            .filter(|cert| cert.is_valid_for_view(&self.view_number))
            .cloned();

        ensure!(
            commitment_and_metadata.block_view == self.view_number,
            "Cannot propose because our VID payload commitment and metadata is for an older view."
        );

        let block_header = TYPES::BlockHeader::new(
            state.as_ref(),
            self.instance_state.as_ref(),
            &parent_leaf,
            commitment_and_metadata.commitment,
            commitment_and_metadata.builder_commitment,
            commitment_and_metadata.metadata,
            commitment_and_metadata.fee,
            vid_share.data.common.clone(),
            self.version,
        )
        .await
        .context("Failed to construct block header")?;

        let proposal = QuorumProposal {
            block_header,
            view_number: self.view_number,
            justify_qc: self.consensus.read().await.high_qc().clone(),
            proposal_certificate,
            upgrade_certificate: None,
        };

        let proposed_leaf = Leaf::from_quorum_proposal(&proposal);
        ensure!(
            proposed_leaf.parent_commitment() == parent_leaf.commit(),
            "Proposed leaf parent does not equal high qc"
        );

        let signature =
            TYPES::SignatureKey::sign(&self.private_key, proposed_leaf.commit().as_ref())
                .context("Failed to compute proposed_leaf.commit()")?;

        let message = Proposal {
            data: proposal,
            signature,
            _pd: PhantomData,
        };
        debug!(
            "Sending proposal for view {:?}",
            proposed_leaf.view_number(),
        );

        self.consensus
            .write()
            .await
            .update_last_proposed_view(message.clone())?;
        async_sleep(Duration::from_millis(self.round_start_delay)).await;
        broadcast_event(
            Arc::new(HotShotEvent::QuorumProposalSend(
                message.clone(),
                self.public_key.clone(),
            )),
            &self.sender,
        )
        .await;

        Ok(())
    }
}
impl<TYPES: NodeType> HandleDepOutput for ProposalDependencyHandle<TYPES> {
    type Output = Vec<Vec<Vec<Arc<HotShotEvent<TYPES>>>>>;

    #[allow(clippy::no_effect_underscore_binding)]
    async fn handle_dep_result(self, res: Self::Output) {
        let high_qc_view_number = self.consensus.read().await.high_qc().view_number;
        if !self
            .consensus
            .read()
            .await
            .validated_state_map()
            .contains_key(&high_qc_view_number)
        {
            // The proposal for the high qc view is missing, try to get it asynchronously
            let memberhsip = Arc::clone(&self.quorum_membership);
            let sender = self.sender.clone();
            let consensus = Arc::clone(&self.consensus);
            async_spawn(async move {
                fetch_proposal(high_qc_view_number, sender, memberhsip, consensus).await
            });
            // Block on receiving the event from the event stream.
            EventDependency::new(
                self.receiver.clone(),
                Box::new(move |event| {
                    let event = event.as_ref();
                    if let HotShotEvent::ValidatedStateUpdated(view_number, _) = event {
                        *view_number == high_qc_view_number
                    } else {
                        false
                    }
                }),
            )
            .completed()
            .await;
        }

        let mut commit_and_metadata: Option<CommitmentAndMetadata<TYPES>> = None;
        let mut timeout_certificate = None;
        let mut view_sync_finalize_cert = None;
        let mut vid_share = None;
        for event in res.iter().flatten().flatten() {
            match event.as_ref() {
                HotShotEvent::SendPayloadCommitmentAndMetadata(
                    payload_commitment,
                    builder_commitment,
                    metadata,
                    view,
                    fee,
                ) => {
                    commit_and_metadata = Some(CommitmentAndMetadata {
                        commitment: *payload_commitment,
                        builder_commitment: builder_commitment.clone(),
                        metadata: metadata.clone(),
                        fee: fee.clone(),
                        block_view: *view,
                    });
                }
                HotShotEvent::QcFormed(cert) => match cert {
                    either::Right(timeout) => {
                        timeout_certificate = Some(timeout.clone());
                    }
                    either::Left(_) => {
                        // Handled by the UpdateHighQc event.
                    }
                },
                HotShotEvent::ViewSyncFinalizeCertificate2Recv(cert) => {
                    view_sync_finalize_cert = Some(cert.clone());
                }
                HotShotEvent::VidDisperseSend(share, _) => {
                    vid_share = Some(share.clone());
                }
                _ => {}
            }
        }

        if commit_and_metadata.is_none() {
            error!(
                "Somehow completed the proposal dependency task without a commitment and metadata"
            );
            return;
        }

        if vid_share.is_none() {
            error!("Somehow completed the proposal dependency task without a VID share");
            return;
        }

        let proposal_cert = if let Some(view_sync_cert) = view_sync_finalize_cert {
            Some(ViewChangeEvidence::ViewSync(view_sync_cert))
        } else {
            timeout_certificate.map(ViewChangeEvidence::Timeout)
        };

        if let Err(e) = self
            .publish_proposal(
                commit_and_metadata.unwrap(),
                vid_share.unwrap(),
                proposal_cert,
            )
            .await
        {
            error!("Failed to publish proposal; error = {e}");
        }
    }
}
