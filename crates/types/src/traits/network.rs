//! Network access compatibility
//!
//! Contains types and traits used by `HotShot` to abstract over network access

use async_compatibility_layer::art::async_sleep;
#[cfg(async_executor_impl = "async-std")]
use async_std::future::TimeoutError;
use derivative::Derivative;
use dyn_clone::DynClone;
use futures::{
    channel::{mpsc, oneshot},
    Future,
};
#[cfg(async_executor_impl = "tokio")]
use tokio::time::error::Elapsed as TimeoutError;

#[cfg(not(any(async_executor_impl = "async-std", async_executor_impl = "tokio")))]
compile_error! {"Either config option \"async-std\" or \"tokio\" must be enabled for this crate."}
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Debug,
    hash::Hash,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use async_compatibility_layer::channel::UnboundedSendError;
use async_trait::async_trait;
use futures::future::join_all;
use rand::{
    distributions::{Bernoulli, Uniform},
    prelude::Distribution,
};
use serde::{Deserialize, Serialize};
use snafu::Snafu;

use super::{node_implementation::NodeType, signature_key::SignatureKey};
use crate::{
    data::ViewNumber,
    message::{MessagePurpose, SequencingMessage},
    BoxSyncFuture,
};

/// for any errors we decide to add to memory network
#[derive(Debug, Snafu, Serialize, Deserialize)]
#[snafu(visibility(pub))]
pub enum MemoryNetworkError {
    /// stub
    Stub,
}

/// Centralized server specific errors
#[derive(Debug, Snafu, Serialize, Deserialize)]
#[snafu(visibility(pub))]
pub enum CentralizedServerNetworkError {
    /// The centralized server could not find a specific message.
    NoMessagesInQueue,
}

/// Centralized server specific errors
#[derive(Debug, Snafu, Serialize, Deserialize)]
#[snafu(visibility(pub))]
pub enum PushCdnNetworkError {
    /// Failed to receive a message from the server
    FailedToReceive,
    /// Failed to send a message to the server
    FailedToSend,
}

/// Web server specific errors
#[derive(Debug, Snafu, Serialize, Deserialize)]
#[snafu(visibility(pub))]
pub enum WebServerNetworkError {
    /// The injected consensus data is incorrect
    IncorrectConsensusData,
    /// The client returned an error
    ClientError,
    /// Endpoint parsed incorrectly
    EndpointError,
    /// Client disconnected
    ClientDisconnected,
}

/// the type of transmission
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransmitType<TYPES: NodeType> {
    /// directly transmit
    Direct(TYPES::SignatureKey),
    /// broadcast the message to all
    Broadcast,
    /// broadcast to DA committee
    DaCommitteeBroadcast,
}

/// Error type for networking
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum NetworkError {
    /// Libp2p specific errors
    Libp2p {
        /// source of error
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// collection of libp2p specific errors
    Libp2pMulti {
        /// sources of errors
        sources: Vec<Box<dyn std::error::Error + Send + Sync>>,
    },
    /// memory network specific errors
    MemoryNetwork {
        /// source of error
        source: MemoryNetworkError,
    },
    /// Push CDN network-specific errors
    PushCdnNetwork {
        /// source of error
        source: PushCdnNetworkError,
    },
    /// Centralized server specific errors
    CentralizedServer {
        /// source of error
        source: CentralizedServerNetworkError,
    },

    /// Web server specific errors
    WebServer {
        /// source of error
        source: WebServerNetworkError,
    },
    /// unimplemented functionality
    UnimplementedFeature,
    /// Could not deliver a message to a specified recipient
    CouldNotDeliver,
    /// Attempted to deliver a message to an unknown node
    NoSuchNode,
    /// No bootstrap nodes were specified on network creation
    NoBootstrapNodesSpecified,
    /// Failed to serialize a network message
    FailedToSerialize {
        /// Originating bincode error
        source: anyhow::Error,
    },
    /// Failed to deserealize a network message
    FailedToDeserialize {
        /// originating bincode error
        source: anyhow::Error,
    },
    /// A timeout occurred
    Timeout {
        /// Source of error
        source: TimeoutError,
    },
    /// Error sending output to consumer of NetworkingImplementation
    /// TODO this should have more information
    ChannelSend,
    /// The underlying connection has been shut down
    ShutDown,
    /// unable to cancel a request, the request has already been cancelled
    UnableToCancel,
    /// The requested data was not found
    NotFound,
    /// Multiple errors
    MultipleErrors {
        /// vec of errors
        errors: Vec<Box<NetworkError>>,
    },
}

/// common traits we would like our network messages to implement
pub trait NetworkMsg:
    Serialize + for<'a> Deserialize<'a> + Clone + Sync + Send + Debug + 'static
{
}

impl<T> NetworkMsg for T where
    T: Serialize + for<'a> Deserialize<'a> + Clone + Sync + Send + Debug + 'static
{
}

/// Trait that bundles what we need from a request ID
pub trait Id: Eq + PartialEq + Hash {}

/// a message
pub trait ViewMessage<TYPES: NodeType> {
    /// get the view out of the message
    fn view_number(&self) -> TYPES::Time;
    // TODO move out of this trait.
    /// get the purpose of the message
    fn purpose(&self) -> MessagePurpose;
}

/// Wraps a oneshot channel for responding to requests
pub struct ResponseChannel<M: NetworkMsg> {
    /// underlying sender for this channel
    pub sender: oneshot::Sender<M>,
}

/// A request for some data that the consensus layer is asking for.
#[derive(Serialize, Deserialize, Derivative, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(bound(deserialize = ""))]
pub struct DataRequest<TYPES: NodeType> {
    /// Request
    pub request: RequestKind<TYPES>,
    /// View this message is for
    pub view: TYPES::Time,
    /// signature of the Sha256 hash of the data so outsiders can't use know
    /// public keys with stake.
    pub signature: <TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType,
}

/// Underlying data request
#[derive(Serialize, Deserialize, Derivative, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RequestKind<TYPES: NodeType> {
    /// Request VID data by our key and the VID commitment
    Vid(TYPES::Time, TYPES::SignatureKey),
    /// Request a DA proposal for a certain view
    DaProposal(TYPES::Time),
    /// Request for quorum proposal for a view
    Proposal(TYPES::Time),
}

/// A response for a request.  `SequencingMessage` is the same as other network messages
/// The kind of message `M` is is determined by what we requested
#[derive(Serialize, Deserialize, Derivative, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(bound(deserialize = ""))]
#[allow(clippy::large_enum_variant)]
/// TODO: Put `Found` content in a `Box` to make enum smaller
pub enum ResponseMessage<TYPES: NodeType> {
    /// Peer returned us some data
    Found(SequencingMessage<TYPES>),
    /// Peer failed to get us data
    NotFound,
    /// The Request was denied
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// When a message should be broadcast to the network.
///
/// Network implementations may or may not respect this, at their discretion.
pub enum BroadcastDelay {
    /// Broadcast the message immediately
    None,
    /// Delay the broadcast to a given view.
    View(u64),
}

#[async_trait]
/// represents a networking implmentration
/// exposes low level API for interacting with a network
/// intended to be implemented for libp2p, the centralized server,
/// and memory network
pub trait ConnectedNetwork<K: SignatureKey + 'static>: Clone + Send + Sync + 'static {
    /// Pauses the underlying network
    fn pause(&self);

    /// Resumes the underlying network
    fn resume(&self);

    /// Blocks until the network is successfully initialized
    async fn wait_for_ready(&self);

    /// Blocks until the network is shut down
    fn shut_down<'a, 'b>(&'a self) -> BoxSyncFuture<'b, ()>
    where
        'a: 'b,
        Self: 'b;

    /// broadcast message to some subset of nodes
    /// blocking
    async fn broadcast_message(
        &self,
        message: Vec<u8>,
        recipients: BTreeSet<K>,
        broadcast_delay: BroadcastDelay,
    ) -> Result<(), NetworkError>;

    /// broadcast a message only to a DA committee
    /// blocking
    async fn da_broadcast_message(
        &self,
        message: Vec<u8>,
        recipients: BTreeSet<K>,
        broadcast_delay: BroadcastDelay,
    ) -> Result<(), NetworkError>;

    /// send messages with vid shares to its recipients
    /// blocking
    async fn vid_broadcast_message(
        &self,
        messages: HashMap<K, Vec<u8>>,
    ) -> Result<(), NetworkError> {
        let future_results = messages
            .into_iter()
            .map(|(recipient_key, message)| self.direct_message(message, recipient_key));
        let results = join_all(future_results).await;

        let errors: Vec<_> = results
            .into_iter()
            .filter_map(|r| match r {
                Err(error) => Some(Box::new(error)),
                _ => None,
            })
            .collect();

        if errors.is_empty() {
            Ok(())
        } else {
            Err(NetworkError::MultipleErrors { errors })
        }
    }

    /// Sends a direct message to a specific node
    /// blocking
    async fn direct_message(&self, message: Vec<u8>, recipient: K) -> Result<(), NetworkError>;

    /// Receive one or many messages from the underlying network.
    ///
    /// # Errors
    /// If there is a network-related failure.
    async fn recv_msgs(&self) -> Result<Vec<Vec<u8>>, NetworkError>;

    /// Ask request the network for some data.  Returns the request ID for that data,
    /// The ID returned can be used for cancelling the request
    async fn request_data<TYPES: NodeType>(
        &self,
        _request: Vec<u8>,
        _recipient: &K,
    ) -> Result<Vec<u8>, NetworkError> {
        Err(NetworkError::UnimplementedFeature)
    }

    /// Spawn a request task in the given network layer.  If it supports
    /// Request and responses it will return the receiving end of a channel.
    /// Requests the network receives will be sent over this channel along
    /// with a return channel to send the response back to.
    ///
    /// Returns `None`` if network does not support handling requests
    async fn spawn_request_receiver_task(
        &self,
    ) -> Option<mpsc::Receiver<(Vec<u8>, ResponseChannel<Vec<u8>>)>> {
        None
    }

    /// queues lookup of a node
    async fn queue_node_lookup(
        &self,
        _view_number: ViewNumber,
        _pk: K,
    ) -> Result<(), UnboundedSendError<Option<(ViewNumber, K)>>> {
        Ok(())
    }

    /// handles view update
    async fn update_view<'a, TYPES>(&'a self, _view: u64, _membership: &TYPES::Membership)
    where
        TYPES: NodeType<SignatureKey = K> + 'a,
    {
    }

    /// Is primary network down? Makes sense only for combined network
    fn is_primary_down(&self) -> bool {
        false
    }
}

/// A channel generator for types that need asynchronous execution
pub type AsyncGenerator<T> = Pin<Box<dyn Fn(u64) -> Pin<Box<dyn Future<Output = T>>>>>;

/// Describes additional functionality needed by the test network implementation
pub trait TestableNetworkingImplementation<TYPES: NodeType>
where
    Self: Sized,
{
    /// generates a network given an expected node count
    #[allow(clippy::type_complexity)]
    fn generator(
        expected_node_count: usize,
        num_bootstrap: usize,
        network_id: usize,
        da_committee_size: usize,
        is_da: bool,
        reliability_config: Option<Box<dyn NetworkReliability>>,
        secondary_network_delay: Duration,
    ) -> AsyncGenerator<(Arc<Self>, Arc<Self>)>;

    /// Get the number of messages in-flight.
    ///
    /// Some implementations will not be able to tell how many messages there are in-flight. These implementations should return `None`.
    fn in_flight_message_count(&self) -> Option<usize>;
}

/// Changes that can occur in the network
#[derive(Debug)]
pub enum NetworkChange<P: SignatureKey> {
    /// A node is connected
    NodeConnected(P),

    /// A node is disconnected
    NodeDisconnected(P),
}

/// interface describing how reliable the network is
#[async_trait]
pub trait NetworkReliability: Debug + Sync + std::marker::Send + DynClone + 'static {
    /// Sample from bernoulli distribution to decide whether
    /// or not to keep a packet
    /// # Panics
    ///
    /// Panics if `self.keep_numerator > self.keep_denominator`
    ///
    fn sample_keep(&self) -> bool {
        true
    }

    /// sample from uniform distribution to decide whether
    /// or not to keep a packet
    fn sample_delay(&self) -> Duration {
        std::time::Duration::ZERO
    }

    /// scramble the packet
    fn scramble(&self, msg: Vec<u8>) -> Vec<u8> {
        msg
    }

    /// number of times to repeat the packet
    fn sample_repeat(&self) -> usize {
        1
    }

    /// given a message and a way to send the message,
    /// decide whether or not to send the message
    /// how long to delay the message
    /// whether or not to send duplicates
    /// and whether or not to include noise with the message
    /// then send the message
    /// note: usually self is stored in a rwlock
    /// so instead of doing the sending part, we just fiddle with the message
    /// then return a future that does the sending and delaying
    fn chaos_send_msg(
        &self,
        msg: Vec<u8>,
        send_fn: Arc<dyn Send + Sync + 'static + Fn(Vec<u8>) -> BoxSyncFuture<'static, ()>>,
    ) -> BoxSyncFuture<'static, ()> {
        let sample_keep = self.sample_keep();
        let delay = self.sample_delay();
        let repeats = self.sample_repeat();
        let mut msgs = Vec::new();
        for _idx in 0..repeats {
            let scrambled = self.scramble(msg.clone());
            msgs.push(scrambled);
        }
        let closure = async move {
            if sample_keep {
                async_sleep(delay).await;
                for msg in msgs {
                    send_fn(msg).await;
                }
            }
        };
        Box::pin(closure)
    }
}

// hack to get clone
dyn_clone::clone_trait_object!(NetworkReliability);

/// ideal network
#[derive(Clone, Copy, Debug, Default)]
pub struct PerfectNetwork {}

impl NetworkReliability for PerfectNetwork {}

/// A synchronous network. Packets may be delayed, but are guaranteed
/// to arrive within `timeout` ns
#[derive(Clone, Copy, Debug, Default)]
pub struct SynchronousNetwork {
    /// Max value in milliseconds that a packet may be delayed
    pub delay_high_ms: u64,
    /// Lowest value in milliseconds that a packet may be delayed
    pub delay_low_ms: u64,
}

impl NetworkReliability for SynchronousNetwork {
    /// never drop a packet
    fn sample_keep(&self) -> bool {
        true
    }
    fn sample_delay(&self) -> Duration {
        Duration::from_millis(
            Uniform::new_inclusive(self.delay_low_ms, self.delay_high_ms)
                .sample(&mut rand::thread_rng()),
        )
    }
}

/// An asynchronous network. Packets may be dropped entirely
/// or delayed for arbitrarily long periods
/// probability that packet is kept = `keep_numerator` / `keep_denominator`
/// packet delay is obtained by sampling from a uniform distribution
/// between `delay_low_ms` and `delay_high_ms`, inclusive
#[derive(Debug, Clone, Copy)]
pub struct AsynchronousNetwork {
    /// numerator for probability of keeping packets
    pub keep_numerator: u32,
    /// denominator for probability of keeping packets
    pub keep_denominator: u32,
    /// lowest value in milliseconds that a packet may be delayed
    pub delay_low_ms: u64,
    /// highest value in milliseconds that a packet may be delayed
    pub delay_high_ms: u64,
}

impl NetworkReliability for AsynchronousNetwork {
    fn sample_keep(&self) -> bool {
        Bernoulli::from_ratio(self.keep_numerator, self.keep_denominator)
            .unwrap()
            .sample(&mut rand::thread_rng())
    }
    fn sample_delay(&self) -> Duration {
        Duration::from_millis(
            Uniform::new_inclusive(self.delay_low_ms, self.delay_high_ms)
                .sample(&mut rand::thread_rng()),
        )
    }
}

/// An partially synchronous network. Behaves asynchronously
/// until some arbitrary time bound, GST,
/// then synchronously after GST
#[allow(clippy::similar_names)]
#[derive(Debug, Clone, Copy)]
pub struct PartiallySynchronousNetwork {
    /// asynchronous portion of network
    pub asynchronous: AsynchronousNetwork,
    /// synchronous portion of network
    pub synchronous: SynchronousNetwork,
    /// time when GST occurs
    pub gst: std::time::Duration,
    /// when the network was started
    pub start: std::time::Instant,
}

impl NetworkReliability for PartiallySynchronousNetwork {
    /// never drop a packet
    fn sample_keep(&self) -> bool {
        true
    }
    fn sample_delay(&self) -> Duration {
        // act asynchronous before gst
        if self.start.elapsed() < self.gst {
            if self.asynchronous.sample_keep() {
                self.asynchronous.sample_delay()
            } else {
                // assume packet was "dropped" and will arrive after gst
                self.synchronous.sample_delay() + self.gst
            }
        } else {
            // act synchronous after gst
            self.synchronous.sample_delay()
        }
    }
}

impl Default for AsynchronousNetwork {
    // disable all chance of failure
    fn default() -> Self {
        AsynchronousNetwork {
            keep_numerator: 1,
            keep_denominator: 1,
            delay_low_ms: 0,
            delay_high_ms: 0,
        }
    }
}

impl Default for PartiallySynchronousNetwork {
    fn default() -> Self {
        PartiallySynchronousNetwork {
            synchronous: SynchronousNetwork::default(),
            asynchronous: AsynchronousNetwork::default(),
            gst: std::time::Duration::new(0, 0),
            start: std::time::Instant::now(),
        }
    }
}

impl SynchronousNetwork {
    /// create new `SynchronousNetwork`
    #[must_use]
    pub fn new(timeout: u64, delay_low_ms: u64) -> Self {
        SynchronousNetwork {
            delay_high_ms: timeout,
            delay_low_ms,
        }
    }
}

impl AsynchronousNetwork {
    /// create new `AsynchronousNetwork`
    #[must_use]
    pub fn new(
        keep_numerator: u32,
        keep_denominator: u32,
        delay_low_ms: u64,
        delay_high_ms: u64,
    ) -> Self {
        AsynchronousNetwork {
            keep_numerator,
            keep_denominator,
            delay_low_ms,
            delay_high_ms,
        }
    }
}

impl PartiallySynchronousNetwork {
    /// create new `PartiallySynchronousNetwork`
    #[allow(clippy::similar_names)]
    #[must_use]
    pub fn new(
        asynchronous: AsynchronousNetwork,
        synchronous: SynchronousNetwork,
        gst: std::time::Duration,
    ) -> Self {
        PartiallySynchronousNetwork {
            asynchronous,
            synchronous,
            gst,
            start: std::time::Instant::now(),
        }
    }
}

/// A chaotic network using all the networking calls
#[derive(Debug, Clone)]
pub struct ChaosNetwork {
    /// numerator for probability of keeping packets
    pub keep_numerator: u32,
    /// denominator for probability of keeping packets
    pub keep_denominator: u32,
    /// lowest value in milliseconds that a packet may be delayed
    pub delay_low_ms: u64,
    /// highest value in milliseconds that a packet may be delayed
    pub delay_high_ms: u64,
    /// lowest value of repeats for a message
    pub repeat_low: usize,
    /// highest value of repeats for a message
    pub repeat_high: usize,
}

impl NetworkReliability for ChaosNetwork {
    fn sample_keep(&self) -> bool {
        Bernoulli::from_ratio(self.keep_numerator, self.keep_denominator)
            .unwrap()
            .sample(&mut rand::thread_rng())
    }

    fn sample_delay(&self) -> Duration {
        Duration::from_millis(
            Uniform::new_inclusive(self.delay_low_ms, self.delay_high_ms)
                .sample(&mut rand::thread_rng()),
        )
    }

    fn sample_repeat(&self) -> usize {
        Uniform::new_inclusive(self.repeat_low, self.repeat_high).sample(&mut rand::thread_rng())
    }
}
