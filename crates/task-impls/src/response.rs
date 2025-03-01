use std::{sync::Arc, time::Duration};

use async_broadcast::Receiver;
use async_compatibility_layer::art::{async_sleep, async_spawn};
#[cfg(async_executor_impl = "async-std")]
use async_std::task::JoinHandle;
use futures::{channel::mpsc, FutureExt, StreamExt};
use hotshot_task::dependency::{Dependency, EventDependency};
use hotshot_types::{
    consensus::{Consensus, LockedConsensusState},
    data::VidDisperseShare,
    message::{
        DaConsensusMessage, DataMessage, GeneralConsensusMessage, Message, MessageKind, Proposal,
        SequencingMessage,
    },
    traits::{
        election::Membership,
        network::{DataRequest, RequestKind, ResponseChannel, ResponseMessage},
        node_implementation::NodeType,
        signature_key::SignatureKey,
    },
};
use sha2::{Digest, Sha256};
#[cfg(async_executor_impl = "tokio")]
use tokio::task::JoinHandle;

use crate::events::HotShotEvent;

/// Type alias for the channel that we receive requests from the network on.
pub type RequestReceiver = mpsc::Receiver<(Vec<u8>, ResponseChannel<Vec<u8>>)>;

/// Time to wait for txns before sending `ResponseMessage::NotFound`
const TXNS_TIMEOUT: Duration = Duration::from_millis(100);

/// Task state for the Network Request Task. The task is responsible for handling
/// requests sent to this node by the network.  It will validate the sender,
/// parse the request, and try to find the data request in the consensus stores.
pub struct NetworkResponseState<TYPES: NodeType> {
    /// Locked consensus state
    consensus: LockedConsensusState<TYPES>,
    /// Receiver for requests
    receiver: RequestReceiver,
    /// Quorum membership for checking if requesters have state
    quorum: Arc<TYPES::Membership>,
    /// This replicas public key
    pub_key: TYPES::SignatureKey,
    /// This replicas private key
    private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
}

impl<TYPES: NodeType> NetworkResponseState<TYPES> {
    /// Create the network request state with the info it needs
    pub fn new(
        consensus: LockedConsensusState<TYPES>,
        receiver: RequestReceiver,
        quorum: Arc<TYPES::Membership>,
        pub_key: TYPES::SignatureKey,
        private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    ) -> Self {
        Self {
            consensus,
            receiver,
            quorum,
            pub_key,
            private_key,
        }
    }

    /// Run the request response loop until a `HotShotEvent::Shutdown` is received.
    /// Or the stream is closed.
    async fn run_loop(mut self, shutdown: EventDependency<Arc<HotShotEvent<TYPES>>>) {
        let mut shutdown = Box::pin(shutdown.completed().fuse());
        loop {
            futures::select! {
                req = self.receiver.next() => {
                    match req {
                        Some((msg, chan)) => self.handle_message(msg, chan).await,
                        None => return,
                    }
                },
                _ = shutdown => {
                    return;
                }
            }
        }
    }

    /// Handle an incoming message.  First validates the sender, then handles the contained request.
    /// Sends the response via `chan`
    async fn handle_message(&self, raw_req: Vec<u8>, chan: ResponseChannel<Vec<u8>>) {
        let req: Message<TYPES> = match bincode::deserialize(&raw_req) {
            Ok(deserialized) => deserialized,
            Err(e) => {
                tracing::error!("Failed to deserialize message! Error: {e}");
                return;
            }
        };
        let sender = req.sender.clone();

        match req.kind {
            MessageKind::Data(DataMessage::RequestData(request)) => {
                if !self.valid_sender(&sender) || !valid_signature::<TYPES>(&request, &sender) {
                    let serialized_msg = match bincode::serialize(
                        &self.make_msg(ResponseMessage::Denied),
                    ) {
                        Ok(serialized) => serialized,
                        Err(e) => {
                            tracing::error!("Failed to serialize outgoing message: this should never happen. Error: {e}");
                            return;
                        }
                    };
                    let _ = chan.sender.send(serialized_msg);
                    return;
                }

                let response = self.handle_request(request).await;
                let serialized_response = match bincode::serialize(&response) {
                    Ok(serialized) => serialized,
                    Err(e) => {
                        tracing::error!("Failed to serialize outgoing message: this should never happen. Error: {e}");
                        return;
                    }
                };
                let _ = chan.sender.send(serialized_response);
            }
            msg => tracing::error!(
                "Received message that wasn't a DataRequest in the request task.  Message: {:?}",
                msg
            ),
        }
    }

    /// Get the VID share from consensus storage, or calculate it from the payload for
    /// the view, if we have the payload.  Stores all the shares calculated from the payload
    /// if the calculation was done
    async fn get_or_calc_vid_share(
        &self,
        view: TYPES::Time,
        key: &TYPES::SignatureKey,
    ) -> Option<Proposal<TYPES, VidDisperseShare<TYPES>>> {
        let contained = self
            .consensus
            .read()
            .await
            .vid_shares()
            .get(&view)
            .is_some_and(|m| m.contains_key(key));
        if !contained {
            if Consensus::calculate_and_update_vid(
                Arc::clone(&self.consensus),
                view,
                Arc::clone(&self.quorum),
                &self.private_key,
            )
            .await
            .is_none()
            {
                // Sleep in hope we receive txns in the meantime
                async_sleep(TXNS_TIMEOUT).await;
                Consensus::calculate_and_update_vid(
                    Arc::clone(&self.consensus),
                    view,
                    Arc::clone(&self.quorum),
                    &self.private_key,
                )
                .await?;
            }
            return self
                .consensus
                .read()
                .await
                .vid_shares()
                .get(&view)?
                .get(key)
                .cloned();
        }
        self.consensus
            .read()
            .await
            .vid_shares()
            .get(&view)?
            .get(key)
            .cloned()
    }

    /// Handle the request contained in the message. Returns the response we should send
    /// First parses the kind and passes to the appropriate handler for the specific type
    /// of the request.
    async fn handle_request(&self, req: DataRequest<TYPES>) -> Message<TYPES> {
        match req.request {
            RequestKind::Vid(view, pub_key) => {
                let Some(share) = self.get_or_calc_vid_share(view, &pub_key).await else {
                    return self.make_msg(ResponseMessage::NotFound);
                };
                let seq_msg = SequencingMessage::Da(DaConsensusMessage::VidDisperseMsg(share));
                self.make_msg(ResponseMessage::Found(seq_msg))
            }
            // TODO impl for DA Proposal: https://github.com/EspressoSystems/HotShot/issues/2651
            RequestKind::DaProposal(_view) => self.make_msg(ResponseMessage::NotFound),
            RequestKind::Proposal(view) => self.make_msg(self.respond_with_proposal(view).await),
        }
    }

    /// Helper to turn a `ResponseMessage` into a `Message` by filling
    /// in the surrounding feilds and creating the `MessageKind`
    fn make_msg(&self, msg: ResponseMessage<TYPES>) -> Message<TYPES> {
        Message {
            sender: self.pub_key.clone(),
            kind: MessageKind::Data(DataMessage::DataResponse(msg)),
        }
    }
    /// Makes sure the sender is allowed to send a request.
    fn valid_sender(&self, sender: &TYPES::SignatureKey) -> bool {
        self.quorum.has_stake(sender)
    }
    /// Lookup the proposal for the view and respond if it's found/not found
    async fn respond_with_proposal(&self, view: TYPES::Time) -> ResponseMessage<TYPES> {
        match self.consensus.read().await.last_proposals().get(&view) {
            Some(prop) => ResponseMessage::Found(SequencingMessage::General(
                GeneralConsensusMessage::Proposal(prop.clone()),
            )),
            None => ResponseMessage::NotFound,
        }
    }
}

/// Check the signature
fn valid_signature<TYPES: NodeType>(
    req: &DataRequest<TYPES>,
    sender: &TYPES::SignatureKey,
) -> bool {
    let Ok(data) = bincode::serialize(&req.request) else {
        return false;
    };
    sender.validate(&req.signature, &Sha256::digest(data))
}

/// Spawn the network response task to handle incoming request for data
/// from other nodes.  It will shutdown when it gets `HotshotEvent::Shutdown`
/// on the `event_stream` arg.
pub fn run_response_task<TYPES: NodeType>(
    task_state: NetworkResponseState<TYPES>,
    event_stream: Receiver<Arc<HotShotEvent<TYPES>>>,
) -> JoinHandle<()> {
    let dep = EventDependency::new(
        event_stream,
        Box::new(|e| matches!(e.as_ref(), HotShotEvent::Shutdown)),
    );
    async_spawn(task_state.run_loop(dep))
}
