use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use async_broadcast::{Receiver, Sender};
use async_compatibility_layer::art::{async_sleep, async_spawn, async_timeout};
use async_lock::RwLock;
#[cfg(async_executor_impl = "async-std")]
use async_std::task::JoinHandle;
use async_trait::async_trait;
use hotshot_task::task::TaskState;
use hotshot_types::{
    consensus::Consensus,
    data::QuorumProposal,
    message::{
        DaConsensusMessage, DataMessage, GeneralConsensusMessage, Message, MessageKind, Proposal,
        SequencingMessage,
    },
    traits::{
        election::Membership,
        network::{ConnectedNetwork, DataRequest, RequestKind, ResponseMessage},
        node_implementation::{NodeImplementation, NodeType},
        signature_key::SignatureKey,
    },
    vote::HasViewNumber,
};
use rand::{prelude::SliceRandom, thread_rng};
use sha2::{Digest, Sha256};
#[cfg(async_executor_impl = "tokio")]
use tokio::task::JoinHandle;
use tracing::{debug, error, info, instrument, warn};

use crate::{
    events::{HotShotEvent, ProposalMissing},
    helpers::broadcast_event,
};

/// Amount of time to try for a request before timing out.
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);

/// Long running task which will request information after a proposal is received.
/// The task will wait a it's `delay` and then send a request iteratively to peers
/// for any data they don't have related to the proposal.  For now it's just requesting VID
/// shares.
pub struct NetworkRequestState<TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// Network to send requests over
    pub network: Arc<I::QuorumNetwork>,
    /// Consensus shared state so we can check if we've gotten the information
    /// before sending a request
    pub state: Arc<RwLock<Consensus<TYPES>>>,
    /// Last seen view, we won't request for proposals before older than this view
    pub view: TYPES::Time,
    /// Delay before requesting peers
    pub delay: Duration,
    /// DA Membership
    pub da_membership: TYPES::Membership,
    /// Quorum Membership
    pub quorum_membership: TYPES::Membership,
    /// This nodes public key
    pub public_key: TYPES::SignatureKey,
    /// This nodes private/signign key, used to sign requests.
    pub private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    /// The node's id
    pub id: u64,
    /// A flag indicating that `HotShotEvent::Shutdown` has been received
    pub shutdown_flag: Arc<AtomicBool>,
    /// A flag indicating that `HotShotEvent::Shutdown` has been received
    pub spawned_tasks: BTreeMap<TYPES::Time, Vec<JoinHandle<()>>>,
}

impl<TYPES: NodeType, I: NodeImplementation<TYPES>> Drop for NetworkRequestState<TYPES, I> {
    fn drop(&mut self) {
        futures::executor::block_on(async move { self.cancel_subtasks().await });
    }
}

/// Alias for a signature
type Signature<TYPES> =
    <<TYPES as NodeType>::SignatureKey as SignatureKey>::PureAssembledSignatureType;

#[async_trait]
impl<TYPES: NodeType, I: NodeImplementation<TYPES>> TaskState for NetworkRequestState<TYPES, I> {
    type Event = HotShotEvent<TYPES>;

    async fn handle_event(
        &mut self,
        event: Arc<Self::Event>,
        sender: &Sender<Arc<Self::Event>>,
        _receiver: &Receiver<Arc<Self::Event>>,
    ) -> Result<()> {
        match event.as_ref() {
            HotShotEvent::QuorumProposalValidated(proposal, _) => {
                let prop_view = proposal.view_number();
                if prop_view >= self.view {
                    self.spawn_requests(prop_view, sender.clone()).await;
                }
                Ok(())
            }
            HotShotEvent::ViewChange(view) => {
                let view = *view;
                if view > self.view {
                    self.view = view;
                }
                Ok(())
            }
            HotShotEvent::QuorumProposalRequest(missing) => {
                let ProposalMissing {
                    view,
                    response_chan: chan,
                } = missing;
                self.run_proposal(&RequestKind::Proposal(*view), chan.clone(), *view);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn cancel_subtasks(&mut self) {
        self.set_shutdown_flag();

        while !self.spawned_tasks.is_empty() {
            let Some((_, handles)) = self.spawned_tasks.pop_first() else {
                break;
            };

            for handle in handles {
                #[cfg(async_executor_impl = "async-std")]
                handle.cancel().await;
                #[cfg(async_executor_impl = "tokio")]
                handle.abort();
            }
        }
    }
}

impl<TYPES: NodeType, I: NodeImplementation<TYPES>> NetworkRequestState<TYPES, I> {
    /// Spawns tasks for a given view to retrieve any data needed.
    async fn spawn_requests(
        &mut self,
        view: TYPES::Time,
        sender: Sender<Arc<HotShotEvent<TYPES>>>,
    ) {
        let requests = self.build_requests(view).await;
        if requests.is_empty() {
            return;
        }
        requests
            .into_iter()
            .for_each(|r| self.run_delay(r, sender.clone(), view));
    }

    /// Creates the srequest structures for all types that are needed.
    async fn build_requests(&self, view: TYPES::Time) -> Vec<RequestKind<TYPES>> {
        let mut reqs = Vec::new();
        if !self.state.read().await.vid_shares().contains_key(&view) {
            reqs.push(RequestKind::Vid(view, self.public_key.clone()));
        }
        // TODO request other things
        reqs
    }

    /// Sign the serialized version of the request
    fn serialize_and_sign(
        &self,
        request: &RequestKind<TYPES>,
    ) -> Option<<TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType> {
        let Ok(data) = bincode::serialize(&request) else {
            tracing::error!("Failed to serialize request!");
            return None;
        };
        let Ok(signature) = TYPES::SignatureKey::sign(&self.private_key, &Sha256::digest(data))
        else {
            error!("Failed to sign Data Request");
            return None;
        };
        Some(signature)
    }
    /// run a delayed request task for a request.  The first response
    /// received will be sent over `sender`
    #[instrument(skip_all, fields(id = self.id, view = *self.view), name = "NetworkRequestState run_delay", level = "error")]
    fn run_delay(
        &mut self,
        request: RequestKind<TYPES>,
        sender: Sender<Arc<HotShotEvent<TYPES>>>,
        view: TYPES::Time,
    ) {
        let mut recipients: Vec<_> = self
            .da_membership
            .whole_committee(view)
            .into_iter()
            .collect();
        // Randomize the recipients so all replicas don't overload the same 1 recipients
        // and so we don't implicitly rely on the same replica all the time.
        recipients.shuffle(&mut thread_rng());
        let requester = DelayedRequester::<TYPES, I> {
            network: Arc::clone(&self.network),
            state: Arc::clone(&self.state),
            sender,
            delay: self.delay,
            recipients,
            shutdown_flag: Arc::clone(&self.shutdown_flag),
        };
        let Some(signature) = self.serialize_and_sign(&request) else {
            return;
        };
        debug!("Requesting data: {:?}", request);
        let handle = async_spawn(requester.run(request, signature));

        self.spawned_tasks.entry(view).or_default().push(handle);
    }

    /// Spawns a task to send a request for the proposal and send the response on a channel
    fn run_proposal(
        &mut self,
        request: &RequestKind<TYPES>,
        response_chan: Sender<Option<Proposal<TYPES, QuorumProposal<TYPES>>>>,
        view: TYPES::Time,
    ) {
        let leader = self.da_membership.leader(view);
        let requester = ProposalRequester::<TYPES, I> {
            network: Arc::clone(&self.network),
            sender: response_chan,
            leader,
        };
        let Some(signature) = self.serialize_and_sign(request) else {
            return;
        };

        let pub_key = self.public_key.clone();
        async_spawn(async move {
            requester.do_proposal(view, signature, pub_key).await;
        });
    }

    /// Signals delayed requesters to finish
    pub fn set_shutdown_flag(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
    }
}

/// A short lived task that waits a delay and starts trying peers until it completes
/// a request.  If at any point the requested info is seen in the data stores or
/// the view has moved beyond the view we are requesting, the task will completed.
struct DelayedRequester<TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// Network to send requests
    network: Arc<I::QuorumNetwork>,
    /// Shared state to check if the data go populated
    state: Arc<RwLock<Consensus<TYPES>>>,
    /// Channel to send the event when we receive a response
    sender: Sender<Arc<HotShotEvent<TYPES>>>,
    /// Duration to delay sending the first request
    delay: Duration,
    /// The peers we will request in a random order
    recipients: Vec<TYPES::SignatureKey>,
    /// A flag indicating that `HotShotEvent::Shutdown` has been received
    shutdown_flag: Arc<AtomicBool>,
}

/// A task the requests some data immediately from one peer

struct ProposalRequester<TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// Network to send requests
    network: Arc<I::QuorumNetwork>,
    /// Channel to send the event when we receive a response
    sender: Sender<Option<Proposal<TYPES, QuorumProposal<TYPES>>>>,
    /// Leader for the view of the request
    leader: TYPES::SignatureKey,
}

impl<TYPES: NodeType, I: NodeImplementation<TYPES>> ProposalRequester<TYPES, I> {
    /// Handle sending a request for proposal for a view, does
    /// not delay
    async fn do_proposal(
        &self,
        view: TYPES::Time,
        signature: Signature<TYPES>,
        key: TYPES::SignatureKey,
    ) {
        let response = match bincode::serialize(&make_proposal_req::<TYPES>(view, signature, key)) {
            Ok(serialized_msg) => {
                async_timeout(
                    REQUEST_TIMEOUT,
                    self.network
                        .request_data::<TYPES>(serialized_msg, &self.leader),
                )
                .await
            }
            Err(e) => {
                tracing::error!(
                    "Failed to serialize outgoing message: this should never happen. Error: {e}"
                );
                broadcast_event(None, &self.sender).await;
                return;
            }
        };
        if let Ok(Ok(serialized_response)) = response {
            if let Ok(ResponseMessage::Found(msg)) = bincode::deserialize(&serialized_response) {
                let SequencingMessage::General(GeneralConsensusMessage::Proposal(prop)) = msg
                else {
                    error!("Requested Proposal but received a non-proposal in response.  Response was {:?}", msg);
                    broadcast_event(None, &self.sender).await;
                    return;
                };
                broadcast_event(Some(prop), &self.sender).await;
            }
            broadcast_event(None, &self.sender).await;
        } else {
            broadcast_event(None, &self.sender).await;
        }
    }
}

/// Wrapper for the info in a VID request
struct VidRequest<TYPES: NodeType>(TYPES::Time, TYPES::SignatureKey);

impl<TYPES: NodeType, I: NodeImplementation<TYPES>> DelayedRequester<TYPES, I> {
    /// Wait the delay, then try to complete the request.  Iterates over peers
    /// until the request is completed, or the data is no longer needed.
    async fn run(self, request: RequestKind<TYPES>, signature: Signature<TYPES>) {
        match request {
            RequestKind::Vid(view, key) => {
                // Do the delay only if primary is up and then start sending
                if !self.network.is_primary_down() {
                    async_sleep(self.delay).await;
                }
                self.do_vid(VidRequest(view, key), signature).await;
            }
            RequestKind::Proposal(..) | RequestKind::DaProposal(..) => {}
        }
    }
    /// Handle sending a VID Share request, runs the loop until the data exists
    async fn do_vid(&self, req: VidRequest<TYPES>, signature: Signature<TYPES>) {
        let message = make_vid(&req, signature);
        let mut recipients_it = self.recipients.iter().cycle();

        let serialized_msg = match bincode::serialize(&message) {
            Ok(serialized_msg) => serialized_msg,
            Err(e) => {
                tracing::error!(
                    "Failed to serialize outgoing message: this should never happen. Error: {e}"
                );

                return;
            }
        };

        while !self.cancel_vid(&req).await {
            match async_timeout(
                REQUEST_TIMEOUT,
                self.network
                    .request_data::<TYPES>(serialized_msg.clone(), recipients_it.next().unwrap()),
            )
            .await
            {
                Ok(Ok(response)) => {
                    match bincode::deserialize(&response) {
                        Ok(ResponseMessage::Found(data)) => {
                            self.handle_response_message(data).await;
                            // keep trying, but expect the map to be populated, or view to increase
                            async_sleep(REQUEST_TIMEOUT).await;
                        }
                        Ok(ResponseMessage::NotFound) => {
                            info!("Peer Responded they did not have the data");
                        }
                        Ok(ResponseMessage::Denied) => {
                            error!("Request for data was denied by the receiver");
                        }
                        Err(e) => {
                            error!("Failed to deserialize response: {e}");
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!("Error Sending request.  Error: {:?}", e);
                    async_sleep(REQUEST_TIMEOUT).await;
                }
                Err(_) => {
                    warn!("Request to other node timed out");
                }
            }
        }
    }
    /// Returns true if we got the data we wanted, or the view has moved on.
    async fn cancel_vid(&self, req: &VidRequest<TYPES>) -> bool {
        let view = req.0;
        let state = self.state.read().await;
        self.shutdown_flag.load(Ordering::Relaxed)
            || state.vid_shares().contains_key(&view)
            || state.cur_view() > view
    }

    /// Transform a response into a `HotShotEvent`
    async fn handle_response_message(&self, message: SequencingMessage<TYPES>) {
        let event = match message {
            SequencingMessage::Da(DaConsensusMessage::VidDisperseMsg(prop)) => {
                HotShotEvent::VidShareRecv(prop)
            }
            _ => return,
        };
        broadcast_event(Arc::new(event), &self.sender).await;
    }
}

/// Make a VID Request Message to send
fn make_vid<TYPES: NodeType>(
    req: &VidRequest<TYPES>,
    signature: Signature<TYPES>,
) -> Message<TYPES> {
    let kind = RequestKind::Vid(req.0, req.1.clone());
    let data_request = DataRequest {
        view: req.0,
        request: kind,
        signature,
    };
    Message {
        sender: req.1.clone(),
        kind: MessageKind::Data(DataMessage::RequestData(data_request)),
    }
}

/// Build a request for a Proposal
fn make_proposal_req<TYPES: NodeType>(
    view: TYPES::Time,
    signature: Signature<TYPES>,
    key: TYPES::SignatureKey,
) -> Message<TYPES> {
    let kind = RequestKind::Proposal(view);
    let data_request = DataRequest {
        view,
        request: kind,
        signature,
    };
    Message {
        sender: key,
        kind: MessageKind::Data(DataMessage::RequestData(data_request)),
    }
}
