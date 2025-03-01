// TODO: Remove after integration
#![allow(unused_imports)]

use futures::StreamExt;
use hotshot::tasks::task_state::CreateTaskState;
use hotshot_example_types::{
    node_types::{MemoryImpl, TestTypes},
    state_types::TestValidatedState,
};
use hotshot_macros::{run_test, test_scripts};
use hotshot_task_impls::{
    events::HotShotEvent::*, quorum_proposal_recv::QuorumProposalRecvTaskState,
};
use hotshot_testing::{
    helpers::{build_fake_view_with_leaf_and_state, build_system_handle},
    predicates::event::{all_predicates, quorum_proposal_missing, exact, vote_now},
    script::InputOrder,
    serial,
    view_generator::TestViewGenerator,
};
use hotshot_types::{data::ViewNumber, traits::{node_implementation::ConsensusTime,ValidatedState}};

#[cfg(test)]
#[cfg(feature = "dependency-tasks")]
#[cfg_attr(async_executor_impl = "tokio", tokio::test(flavor = "multi_thread"))]
#[cfg_attr(async_executor_impl = "async-std", async_std::test)]
async fn test_quorum_proposal_recv_task() {
    use std::time::Duration;

    use hotshot_testing::{
        helpers::build_fake_view_with_leaf,
        script::{Expectations, TaskScript},
    };
    use hotshot_types::data::Leaf;

    async_compatibility_layer::logging::setup_logging();
    async_compatibility_layer::logging::setup_backtrace();

    let handle = build_system_handle(2).await.0;
    let quorum_membership = handle.hotshot.memberships.quorum_membership.clone();
    let da_membership = handle.hotshot.memberships.da_membership.clone();
    let consensus = handle.hotshot.consensus();
    let mut consensus_writer = consensus.write().await;

    let mut generator = TestViewGenerator::generate(quorum_membership.clone(), da_membership);
    let mut proposals = Vec::new();
    let mut leaders = Vec::new();
    let mut votes = Vec::new();
    let mut dacs = Vec::new();
    let mut vids = Vec::new();
    let mut leaves = Vec::new();
    for view in (&mut generator).take(2).collect::<Vec<_>>().await {
        proposals.push(view.quorum_proposal.clone());
        leaders.push(view.leader_public_key);
        votes.push(view.create_quorum_vote(&handle));
        dacs.push(view.da_certificate.clone());
        vids.push(view.vid_proposal.clone());
        leaves.push(view.leaf.clone());

        // These are both updated when we vote. Since we don't have access
        // to that, we'll just put them in here.
        consensus_writer
            .update_saved_leaves(Leaf::from_quorum_proposal(&view.quorum_proposal.data));
        consensus_writer.update_validated_state_map(
            view.quorum_proposal.data.view_number,
            build_fake_view_with_leaf(view.leaf.clone()),
        ).unwrap();
    }
    drop(consensus_writer);

    let inputs = vec![serial![QuorumProposalRecv(
        proposals[1].clone(),
        leaders[1]
    )]];

    let expectations = vec![Expectations::from_outputs(vec![
        exact(ViewChange(ViewNumber::new(2))),
        exact(UpdateHighQc(proposals[1].data.justify_qc.clone())),
        exact(ValidatedStateUpdated(
            ViewNumber::new(2),
            build_fake_view_with_leaf_and_state(
                leaves[1].clone(),
                <TestValidatedState as ValidatedState<TestTypes>>::from_header(
                    &proposals[1].data.block_header,
                ),
            ),
        )),
        exact(QuorumProposalValidated(
            proposals[1].data.clone(),
            leaves[0].clone(),
        )),
    ])];

    let state = QuorumProposalRecvTaskState::<TestTypes, MemoryImpl>::create_from(&handle).await;
    let mut script = TaskScript {
        timeout: Duration::from_millis(35),
        state,
        expectations,
    };
    run_test![inputs, script].await;
}

#[cfg(test)]
#[cfg(feature = "dependency-tasks")]
#[cfg_attr(async_executor_impl = "tokio", tokio::test(flavor = "multi_thread"))]
#[cfg_attr(async_executor_impl = "async-std", async_std::test)]
async fn test_quorum_proposal_recv_task_liveness_check() {
    use std::time::Duration;

    use hotshot::traits::ValidatedState;
    use hotshot_example_types::state_types::TestValidatedState;
    use hotshot_testing::{
        all_predicates,
        helpers::{build_fake_view_with_leaf, build_fake_view_with_leaf_and_state},
        script::{Expectations, TaskScript},
    };
    use hotshot_types::{
        data::Leaf,
        vote::{HasViewNumber, VoteDependencyData},
    };

    async_compatibility_layer::logging::setup_logging();
    async_compatibility_layer::logging::setup_backtrace();

    let handle = build_system_handle(4).await.0;
    let quorum_membership = handle.hotshot.memberships.quorum_membership.clone();
    let da_membership = handle.hotshot.memberships.da_membership.clone();
    let consensus = handle.hotshot.consensus();
    let mut consensus_writer = consensus.write().await;

    let mut generator = TestViewGenerator::generate(quorum_membership.clone(), da_membership);
    let mut proposals = Vec::new();
    let mut leaders = Vec::new();
    let mut votes = Vec::new();
    let mut dacs = Vec::new();
    let mut vids = Vec::new();
    let mut leaves = Vec::new();
    for view in (&mut generator).take(4).collect::<Vec<_>>().await {
        proposals.push(view.quorum_proposal.clone());
        leaders.push(view.leader_public_key);
        votes.push(view.create_quorum_vote(&handle));
        dacs.push(view.da_certificate.clone());
        vids.push(view.vid_proposal.clone());
        leaves.push(view.leaf.clone());

        // It's not explicitly required to insert an entry for every generated view, but
        // there's no reason not to.
        let inserted_view_number = view.quorum_proposal.data.view_number();

        // These are both updated when we'd have voted previously. However, since
        // we don't have access to that, we'll just put them in here. We
        // specifically ignore writing the saved leaves so that way
        // the parent lookup fails and we trigger a view liveness check.
        consensus_writer.update_validated_state_map(
            inserted_view_number,
            build_fake_view_with_leaf(view.leaf.clone()),
        ).unwrap();

        // The index here is important. Since we're proposing for view 4, we need the
        // value from entry 2 to align the public key from the shares map.
        consensus_writer.update_vid_shares(inserted_view_number, view.vid_proposal.0[2].clone());

        // We need there to be a DA certificate for us to be able to vote, so we grab
        // this from the generator as well since we don't have the running task that'd
        // insert the value ordinarily.
        consensus_writer.update_saved_da_certs(inserted_view_number, view.da_certificate.clone());
    }

    // We can only propose if we've seen a QcFormed event already, so we just insert it
    // ourselves here instead. This is a bit cheesy, but it'll work as we expect for the
    // purposes of the test.
    consensus_writer
        .update_high_qc(proposals[3].data.justify_qc.clone())
        .unwrap();

    drop(consensus_writer);

    let inputs = vec![serial![QuorumProposalRecv(
        proposals[2].clone(),
        leaders[2]
    )]];

    let expectations = vec![Expectations::from_outputs(all_predicates![
        exact(ViewChange(ViewNumber::new(3))),
        exact(ValidatedStateUpdated(
            ViewNumber::new(3),
            build_fake_view_with_leaf_and_state(
                leaves[2].clone(),
                <TestValidatedState as ValidatedState<TestTypes>>::from_header(
                    &proposals[2].data.block_header,
                ),
            ),
        )),
        quorum_proposal_missing(),
        exact(UpdateHighQc(proposals[2].data.justify_qc.clone())),
        vote_now(),
    ])];

    let state = QuorumProposalRecvTaskState::<TestTypes, MemoryImpl>::create_from(&handle).await;
    let mut script = TaskScript {
        timeout: Duration::from_millis(35),
        state,
        expectations,
    };
    run_test![inputs, script].await;
}
