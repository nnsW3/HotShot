//! Provides a generic rust implementation of the `HotShot` BFT protocol
//!

// Documentation module
#[cfg(feature = "docs")]
pub mod documentation;

use hotshot_types::traits::network::BroadcastDelay;
use vbs::version::StaticVersionType;

/// Contains traits consumed by [`SystemContext`]
pub mod traits;
/// Contains types used by the crate
pub mod types;

pub mod tasks;

use std::{
    collections::{BTreeMap, HashMap},
    marker::PhantomData,
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use async_broadcast::{broadcast, InactiveReceiver, Receiver, Sender};
use async_compatibility_layer::art::async_spawn;
use async_lock::RwLock;
use async_trait::async_trait;
use committable::Committable;
use futures::join;
use hotshot_task::task::{ConsensusTaskRegistry, NetworkTaskRegistry};
use hotshot_task_impls::{events::HotShotEvent, helpers::broadcast_event, network};
// Internal
/// Reexport error type
pub use hotshot_types::error::HotShotError;
use hotshot_types::{
    consensus::{Consensus, ConsensusMetricsValue, View, ViewInner},
    constants::{Base, EVENT_CHANNEL_SIZE, EXTERNAL_EVENT_CHANNEL_SIZE},
    data::{Leaf, QuorumProposal},
    event::{EventType, LeafInfo},
    message::{DataMessage, Message, MessageKind, Proposal, VersionedMessage},
    simple_certificate::{QuorumCertificate, UpgradeCertificate},
    traits::{
        consensus_api::ConsensusApi,
        election::Membership,
        network::ConnectedNetwork,
        node_implementation::{ConsensusTime, NodeType},
        signature_key::SignatureKey,
        states::ValidatedState,
        EncodeBytes,
    },
    HotShotConfig,
};
// -- Rexports
// External
/// Reexport rand crate
pub use rand;
use tasks::{add_request_network_task, add_response_task};
use tracing::{debug, instrument, trace};
use vbs::version::Version;

use crate::{
    tasks::{add_consensus_tasks, add_network_event_task, add_network_message_task},
    traits::NodeImplementation,
    types::{Event, SystemContextHandle},
};

/// Length, in bytes, of a 512 bit hash
pub const H_512: usize = 64;
/// Length, in bytes, of a 256 bit hash
pub const H_256: usize = 32;

/// Bundle of the networks used in consensus
pub struct Networks<TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// Network for reaching all nodes
    pub quorum_network: Arc<I::QuorumNetwork>,

    /// Network for reaching the DA committee
    pub da_network: Arc<I::DaNetwork>,

    /// Phantom for TYPES and I
    pub _pd: PhantomData<(TYPES, I)>,
}

impl<TYPES: NodeType, I: NodeImplementation<TYPES>> Networks<TYPES, I> {
    /// wait for all networks to be ready
    pub async fn wait_for_networks_ready(&self) {
        self.quorum_network.wait_for_ready().await;
        self.da_network.wait_for_ready().await;
    }

    /// shut down all networks
    pub async fn shut_down_networks(&self) {
        self.quorum_network.shut_down().await;
        self.da_network.shut_down().await;
    }
}

/// Bundle of all the memberships a consensus instance uses
#[derive(Clone)]
pub struct Memberships<TYPES: NodeType> {
    /// Quorum Membership
    pub quorum_membership: TYPES::Membership,
    /// DA
    pub da_membership: TYPES::Membership,
    /// VID
    pub vid_membership: TYPES::Membership,
    /// View Sync
    pub view_sync_membership: TYPES::Membership,
}

/// Holds the state needed to participate in `HotShot` consensus
pub struct SystemContext<TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// The public key of this node
    public_key: TYPES::SignatureKey,

    /// The private key of this node
    private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,

    /// Configuration items for this hotshot instance
    pub config: HotShotConfig<TYPES::SignatureKey>,

    /// Networks used by the instance of hotshot
    pub networks: Arc<Networks<TYPES, I>>,

    /// Memberships used by consensus
    pub memberships: Arc<Memberships<TYPES>>,

    /// the metrics that the implementor is using.
    metrics: Arc<ConsensusMetricsValue>,

    /// The hotstuff implementation
    consensus: Arc<RwLock<Consensus<TYPES>>>,

    /// Immutable instance state
    instance_state: Arc<TYPES::InstanceState>,

    /// The network version
    version: Arc<RwLock<Version>>,

    /// The view to enter when first starting consensus
    start_view: TYPES::Time,

    /// Access to the output event stream.
    output_event_stream: (Sender<Event<TYPES>>, InactiveReceiver<Event<TYPES>>),

    /// External event stream for communication with the application.
    pub(crate) external_event_stream: (Sender<Event<TYPES>>, InactiveReceiver<Event<TYPES>>),

    /// Anchored leaf provided by the initializer.
    anchored_leaf: Leaf<TYPES>,

    /// access to the internal event stream, in case we need to, say, shut something down
    #[allow(clippy::type_complexity)]
    internal_event_stream: (
        Sender<Arc<HotShotEvent<TYPES>>>,
        InactiveReceiver<Arc<HotShotEvent<TYPES>>>,
    ),

    /// uid for instrumentation
    pub id: u64,

    /// Reference to the internal storage for consensus datum.
    pub storage: Arc<RwLock<I::Storage>>,

    /// a potential upgrade certificate that has been decided on by the consensus tasks.
    pub decided_upgrade_certificate: Arc<RwLock<Option<UpgradeCertificate<TYPES>>>>,
}
impl<TYPES: NodeType, I: NodeImplementation<TYPES>> Clone for SystemContext<TYPES, I> {
    #![allow(deprecated)]
    fn clone(&self) -> Self {
        Self {
            public_key: self.public_key.clone(),
            private_key: self.private_key.clone(),
            config: self.config.clone(),
            networks: Arc::clone(&self.networks),
            memberships: Arc::clone(&self.memberships),
            metrics: Arc::clone(&self.metrics),
            consensus: Arc::clone(&self.consensus),
            instance_state: Arc::clone(&self.instance_state),
            version: Arc::clone(&self.version),
            start_view: self.start_view,
            output_event_stream: self.output_event_stream.clone(),
            external_event_stream: self.external_event_stream.clone(),
            anchored_leaf: self.anchored_leaf.clone(),
            internal_event_stream: self.internal_event_stream.clone(),
            id: self.id,
            storage: Arc::clone(&self.storage),
            decided_upgrade_certificate: Arc::clone(&self.decided_upgrade_certificate),
        }
    }
}

impl<TYPES: NodeType, I: NodeImplementation<TYPES>> SystemContext<TYPES, I> {
    #![allow(deprecated)]
    /// Creates a new [`Arc<SystemContext>`] with the given configuration options.
    ///
    /// To do a full initialization, use `fn init` instead, which will set up background tasks as
    /// well.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(private_key, memberships, networks, initializer, metrics, storage))]
    pub async fn new(
        public_key: TYPES::SignatureKey,
        private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
        nonce: u64,
        config: HotShotConfig<TYPES::SignatureKey>,
        memberships: Memberships<TYPES>,
        networks: Networks<TYPES, I>,
        initializer: HotShotInitializer<TYPES>,
        metrics: ConsensusMetricsValue,
        storage: I::Storage,
    ) -> Result<Arc<Self>, HotShotError<TYPES>> {
        debug!("Creating a new hotshot");

        let consensus_metrics = Arc::new(metrics);
        let anchored_leaf = initializer.inner;
        let instance_state = initializer.instance_state;

        let (internal_tx, internal_rx) = broadcast(EVENT_CHANNEL_SIZE);
        let (mut external_tx, mut external_rx) = broadcast(EXTERNAL_EVENT_CHANNEL_SIZE);

        let decided_upgrade_certificate = Arc::new(RwLock::new(None));

        // Allow overflow on the channel, otherwise sending to it may block.
        external_rx.set_overflow(true);

        // Get the validated state from the initializer or construct an incomplete one from the
        // block header.
        let validated_state = match initializer.validated_state {
            Some(state) => state,
            None => Arc::new(TYPES::ValidatedState::from_header(
                anchored_leaf.block_header(),
            )),
        };

        // Insert the validated state to state map.
        let mut validated_state_map = BTreeMap::default();
        validated_state_map.insert(
            anchored_leaf.view_number(),
            View {
                view_inner: ViewInner::Leaf {
                    leaf: anchored_leaf.commit(),
                    state: Arc::clone(&validated_state),
                    delta: initializer.state_delta.clone(),
                },
            },
        );
        for (view_num, inner) in initializer.undecided_state {
            validated_state_map.insert(view_num, inner);
        }

        let mut saved_leaves = HashMap::new();
        let mut saved_payloads = BTreeMap::new();
        saved_leaves.insert(anchored_leaf.commit(), anchored_leaf.clone());

        for leaf in initializer.undecided_leafs {
            saved_leaves.insert(leaf.commit(), leaf.clone());
        }
        if let Some(payload) = anchored_leaf.block_payload() {
            let encoded_txns = payload.encode();

            saved_payloads.insert(anchored_leaf.view_number(), Arc::clone(&encoded_txns));
        }

        let consensus = Consensus::new(
            validated_state_map,
            anchored_leaf.view_number(),
            anchored_leaf.view_number(),
            // TODO this is incorrect
            // https://github.com/EspressoSystems/HotShot/issues/560
            anchored_leaf.view_number(),
            initializer.saved_proposals,
            saved_leaves,
            saved_payloads,
            initializer.high_qc,
            Arc::clone(&consensus_metrics),
        );

        let consensus = Arc::new(RwLock::new(consensus));
        let version = Arc::new(RwLock::new(Base::VERSION));

        // This makes it so we won't block on broadcasting if there is not a receiver
        // Our own copy of the receiver is inactive so it doesn't count.
        external_tx.set_await_active(false);

        let inner: Arc<SystemContext<TYPES, I>> = Arc::new(SystemContext {
            id: nonce,
            consensus,
            instance_state: Arc::new(instance_state),
            public_key,
            private_key,
            config,
            version,
            start_view: initializer.start_view,
            networks: Arc::new(networks),
            memberships: Arc::new(memberships),
            metrics: Arc::clone(&consensus_metrics),
            internal_event_stream: (internal_tx, internal_rx.deactivate()),
            output_event_stream: (external_tx.clone(), external_rx.clone().deactivate()),
            external_event_stream: (external_tx, external_rx.deactivate()),
            anchored_leaf: anchored_leaf.clone(),
            storage: Arc::new(RwLock::new(storage)),
            decided_upgrade_certificate,
        });

        Ok(inner)
    }

    /// "Starts" consensus by sending a `QcFormed`, `ViewChange`, and `ValidatedStateUpdated` events
    ///
    /// # Panics
    /// Panics if sending genesis fails
    pub async fn start_consensus(&self) {
        #[cfg(feature = "dependncy-tasks")]
        error!("HotShot is running with the dependency tasks feature enabled!!");

        #[cfg(all(feature = "rewind", not(debug_assertions)))]
        compile_error!("Cannot run rewind in production builds!");

        debug!("Starting Consensus");
        let consensus = self.consensus.read().await;

        #[allow(clippy::panic)]
        self.internal_event_stream
            .0
            .broadcast_direct(Arc::new(HotShotEvent::ViewChange(self.start_view)))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "Genesis Broadcast failed; event = ViewChange({:?})",
                    self.start_view
                )
            });
        #[cfg(feature = "dependency-tasks")]
        {
            if let Some(validated_state) = consensus.validated_state_map().get(&self.start_view) {
                #[allow(clippy::panic)]
                self.internal_event_stream
                    .0
                    .broadcast_direct(Arc::new(HotShotEvent::ValidatedStateUpdated(
                        TYPES::Time::new(*self.start_view),
                        validated_state.clone(),
                    )))
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "Genesis Broadcast failed; event = ValidatedStateUpdated({:?})",
                            self.start_view,
                        )
                    });
            }
        }
        #[allow(clippy::panic)]
        self.internal_event_stream
            .0
            .broadcast_direct(Arc::new(HotShotEvent::QcFormed(either::Left(
                consensus.high_qc().clone(),
            ))))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "Genesis Broadcast failed; event = QcFormed(either::Left({:?}))",
                    consensus.high_qc()
                )
            });

        {
            // Some applications seem to expect a leaf decide event for the genesis leaf,
            // which contains only that leaf and nothing else.
            if self.anchored_leaf.view_number() == TYPES::Time::genesis() {
                let (validated_state, state_delta) =
                    TYPES::ValidatedState::genesis(&self.instance_state);

                let qc = Arc::new(
                    QuorumCertificate::genesis(&validated_state, self.instance_state.as_ref())
                        .await,
                );

                broadcast_event(
                    Event {
                        view_number: self.anchored_leaf.view_number(),
                        event: EventType::Decide {
                            leaf_chain: Arc::new(vec![LeafInfo::new(
                                self.anchored_leaf.clone(),
                                Arc::new(validated_state),
                                Some(Arc::new(state_delta)),
                                None,
                            )]),
                            qc,
                            block_size: None,
                        },
                    },
                    &self.external_event_stream.0,
                )
                .await;
            }
        }
    }

    /// Emit an external event
    // A copypasta of `ConsensusApi::send_event`
    // TODO: remove with https://github.com/EspressoSystems/HotShot/issues/2407
    async fn send_external_event(&self, event: Event<TYPES>) {
        debug!(?event, "send_external_event");
        broadcast_event(event, &self.external_event_stream.0).await;
    }

    /// Publishes a transaction asynchronously to the network.
    ///
    /// # Errors
    ///
    /// Always returns Ok; does not return an error if the transaction couldn't be published to the network
    #[instrument(skip(self), err)]
    pub async fn publish_transaction_async(
        &self,
        transaction: TYPES::Transaction,
        decided_upgrade_certificate: Arc<RwLock<Option<UpgradeCertificate<TYPES>>>>,
    ) -> Result<(), HotShotError<TYPES>> {
        trace!("Adding transaction to our own queue");

        let api = self.clone();
        let view_number = api.consensus.read().await.cur_view();

        // Wrap up a message
        let message_kind: DataMessage<TYPES> =
            DataMessage::SubmitTransaction(transaction.clone(), view_number);
        let message = Message {
            sender: api.public_key.clone(),
            kind: MessageKind::from(message_kind),
        };

        let cert = decided_upgrade_certificate.read().await.clone();

        let serialized_message = message
            .serialize(&cert)
            .map_err(|_| HotShotError::FailedToSerialize)?;

        async_spawn(async move {
            let da_membership = &api.memberships.da_membership.clone();
            join! {
                // TODO We should have a function that can return a network error if there is one
                // but first we'd need to ensure our network implementations can support that
                // (and not hang instead)

                // version <0, 1> currently fixed; this is the same as VERSION_0_1,
                // and will be updated to be part of SystemContext. I wanted to use associated
                // constants in NodeType, but that seems to be unavailable in the current Rust.
                api
                    .networks
                    .da_network
                    .broadcast_message(
                        serialized_message,
                        da_membership.whole_committee(view_number),
                        BroadcastDelay::None,
                    ),
                api
                    .send_external_event(Event {
                        view_number,
                        event: EventType::Transactions {
                            transactions: vec![transaction],
                        },
                    }),
            }
        });
        Ok(())
    }

    /// Returns a copy of the consensus struct
    #[must_use]
    pub fn consensus(&self) -> Arc<RwLock<Consensus<TYPES>>> {
        Arc::clone(&self.consensus)
    }

    /// Returns a copy of the instance state
    pub fn instance_state(&self) -> Arc<TYPES::InstanceState> {
        Arc::clone(&self.instance_state)
    }

    /// Returns a copy of the last decided leaf
    /// # Panics
    /// Panics if internal leaf for consensus is inconsistent
    pub async fn decided_leaf(&self) -> Leaf<TYPES> {
        self.consensus.read().await.decided_leaf()
    }

    /// [Non-blocking] instantly returns a copy of the last decided leaf if
    /// it is available to be read. If not, we return `None`.
    ///
    /// # Panics
    /// Panics if internal state for consensus is inconsistent
    #[must_use]
    pub fn try_decided_leaf(&self) -> Option<Leaf<TYPES>> {
        self.consensus.try_read().map(|guard| guard.decided_leaf())
    }

    /// Returns the last decided validated state.
    ///
    /// # Panics
    /// Panics if internal state for consensus is inconsistent
    pub async fn decided_state(&self) -> Arc<TYPES::ValidatedState> {
        Arc::clone(&self.consensus.read().await.decided_state())
    }

    /// Get the validated state from a given `view`.
    ///
    /// Returns the requested state, if the [`SystemContext`] is tracking this view. Consensus
    /// tracks views that have not yet been decided but could be in the future. This function may
    /// return [`None`] if the requested view has already been decided (but see
    /// [`decided_state`](Self::decided_state)) or if there is no path for the requested
    /// view to ever be decided.
    pub async fn state(&self, view: TYPES::Time) -> Option<Arc<TYPES::ValidatedState>> {
        self.consensus.read().await.state(view).cloned()
    }

    /// Initializes a new [`SystemContext`] and does the work of setting up all the background tasks
    ///
    /// Assumes networking implementation is already primed.
    ///
    /// Underlying `HotShot` instance starts out paused, and must be unpaused
    ///
    /// Upon encountering an unrecoverable error, such as a failure to send to a broadcast channel,
    /// the `HotShot` instance will log the error and shut down.
    ///
    /// To construct a [`SystemContext`] without setting up tasks, use `fn new` instead.
    /// # Errors
    ///
    /// Can throw an error if `Self::new` fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn init(
        public_key: TYPES::SignatureKey,
        private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
        node_id: u64,
        config: HotShotConfig<TYPES::SignatureKey>,
        memberships: Memberships<TYPES>,
        networks: Networks<TYPES, I>,
        initializer: HotShotInitializer<TYPES>,
        metrics: ConsensusMetricsValue,
        storage: I::Storage,
    ) -> Result<
        (
            SystemContextHandle<TYPES, I>,
            Sender<Arc<HotShotEvent<TYPES>>>,
            Receiver<Arc<HotShotEvent<TYPES>>>,
        ),
        HotShotError<TYPES>,
    > {
        let hotshot = Self::new(
            public_key,
            private_key,
            node_id,
            config,
            memberships,
            networks,
            initializer,
            metrics,
            storage,
        )
        .await?;
        let handle = Arc::clone(&hotshot).run_tasks().await;
        let (tx, rx) = hotshot.internal_event_stream.clone();

        Ok((handle, tx, rx.activate()))
    }
    /// return the timeout for a view for `self`
    #[must_use]
    pub fn next_view_timeout(&self) -> u64 {
        self.config.next_view_timeout
    }
}

impl<TYPES: NodeType, I: NodeImplementation<TYPES>> SystemContext<TYPES, I> {
    /// Spawn all tasks that operate on [`SystemContextHandle`].
    ///
    /// For a list of which tasks are being spawned, see this module's documentation.
    #[allow(clippy::too_many_lines)]
    pub async fn run_tasks(&self) -> SystemContextHandle<TYPES, I> {
        let consensus_registry = ConsensusTaskRegistry::new();
        let network_registry = NetworkTaskRegistry::new();

        let output_event_stream = self.external_event_stream.clone();
        let internal_event_stream = self.internal_event_stream.clone();

        let quorum_network = Arc::clone(&self.networks.quorum_network);
        let da_network = Arc::clone(&self.networks.da_network);
        let quorum_membership = self.memberships.quorum_membership.clone();
        let da_membership = self.memberships.da_membership.clone();
        let vid_membership = self.memberships.vid_membership.clone();
        let view_sync_membership = self.memberships.view_sync_membership.clone();

        let mut handle = SystemContextHandle {
            consensus_registry,
            network_registry,
            output_event_stream: output_event_stream.clone(),
            internal_event_stream: internal_event_stream.clone(),
            hotshot: self.clone().into(),
            storage: Arc::clone(&self.storage),
        };

        add_network_message_task(&mut handle, Arc::clone(&quorum_network)).await;
        add_network_message_task(&mut handle, Arc::clone(&da_network)).await;

        if let Some(request_receiver) = da_network.spawn_request_receiver_task().await {
            add_request_network_task(&mut handle).await;
            add_response_task(&mut handle, request_receiver).await;
        }

        add_network_event_task(
            &mut handle,
            Arc::clone(&quorum_network),
            quorum_membership.clone(),
            network::quorum_filter,
        )
        .await;
        add_network_event_task(
            &mut handle,
            Arc::clone(&quorum_network),
            quorum_membership,
            network::upgrade_filter,
        )
        .await;
        add_network_event_task(
            &mut handle,
            Arc::clone(&da_network),
            da_membership,
            network::da_filter,
        )
        .await;
        add_network_event_task(
            &mut handle,
            Arc::clone(&quorum_network),
            view_sync_membership,
            network::view_sync_filter,
        )
        .await;
        add_network_event_task(
            &mut handle,
            Arc::clone(&quorum_network),
            vid_membership,
            network::vid_filter,
        )
        .await;
        add_consensus_tasks::<TYPES, I, Base>(&mut handle).await;
        handle
    }
}

#[async_trait]
impl<TYPES: NodeType, I: NodeImplementation<TYPES>> ConsensusApi<TYPES, I>
    for SystemContextHandle<TYPES, I>
{
    fn total_nodes(&self) -> NonZeroUsize {
        self.hotshot.config.num_nodes_with_stake
    }

    fn builder_timeout(&self) -> Duration {
        self.hotshot.config.builder_timeout
    }

    async fn send_event(&self, event: Event<TYPES>) {
        debug!(?event, "send_event");
        broadcast_event(event, &self.hotshot.external_event_stream.0).await;
    }

    fn public_key(&self) -> &TYPES::SignatureKey {
        &self.hotshot.public_key
    }

    fn private_key(&self) -> &<TYPES::SignatureKey as SignatureKey>::PrivateKey {
        &self.hotshot.private_key
    }
}

/// initializer struct for creating starting block
pub struct HotShotInitializer<TYPES: NodeType> {
    /// the leaf specified initialization
    inner: Leaf<TYPES>,

    /// Instance-level state.
    instance_state: TYPES::InstanceState,

    /// Optional validated state.
    ///
    /// If it's given, we'll use it to construct the `SystemContext`. Otherwise, we'll construct
    /// the state from the block header.
    validated_state: Option<Arc<TYPES::ValidatedState>>,

    /// Optional state delta.
    ///
    /// If it's given, we'll use it to construct the `SystemContext`.
    state_delta: Option<Arc<<TYPES::ValidatedState as ValidatedState<TYPES>>::Delta>>,

    /// Starting view number that we are confident won't lead to a double vote after restart.
    start_view: TYPES::Time,
    /// Highest QC that was seen, for genesis it's the genesis QC.  It should be for a view greater
    /// than `inner`s view number for the non genesis case because we must have seen higher QCs
    /// to decide on the leaf.
    high_qc: QuorumCertificate<TYPES>,
    /// Undecided leafs that were seen, but not yet decided on.  These allow a restarting node
    /// to vote and propose right away if they didn't miss anything while down.
    undecided_leafs: Vec<Leaf<TYPES>>,
    /// Not yet decided state
    undecided_state: BTreeMap<TYPES::Time, View<TYPES>>,
    /// Proposals we have sent out to provide to others for catchup
    saved_proposals: BTreeMap<TYPES::Time, Proposal<TYPES, QuorumProposal<TYPES>>>,
}

impl<TYPES: NodeType> HotShotInitializer<TYPES> {
    /// initialize from genesis
    /// # Errors
    /// If we are unable to apply the genesis block to the default state
    pub async fn from_genesis(
        instance_state: TYPES::InstanceState,
    ) -> Result<Self, HotShotError<TYPES>> {
        let (validated_state, state_delta) = TYPES::ValidatedState::genesis(&instance_state);
        let high_qc = QuorumCertificate::genesis(&validated_state, &instance_state).await;

        Ok(Self {
            inner: Leaf::genesis(&validated_state, &instance_state).await,
            validated_state: Some(Arc::new(validated_state)),
            state_delta: Some(Arc::new(state_delta)),
            start_view: TYPES::Time::new(0),
            saved_proposals: BTreeMap::new(),
            high_qc,
            undecided_leafs: Vec::new(),
            undecided_state: BTreeMap::new(),
            instance_state,
        })
    }

    /// Reload previous state based on most recent leaf and the instance-level state.
    ///
    /// # Arguments
    /// *  `start_view` - The minimum view number that we are confident won't lead to a double vote
    /// after restart.
    /// * `validated_state` - Optional validated state that if given, will be used to construct the
    /// `SystemContext`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_reload(
        anchor_leaf: Leaf<TYPES>,
        instance_state: TYPES::InstanceState,
        validated_state: Option<Arc<TYPES::ValidatedState>>,
        start_view: TYPES::Time,
        saved_proposals: BTreeMap<TYPES::Time, Proposal<TYPES, QuorumProposal<TYPES>>>,
        high_qc: QuorumCertificate<TYPES>,
        undecided_leafs: Vec<Leaf<TYPES>>,
        undecided_state: BTreeMap<TYPES::Time, View<TYPES>>,
    ) -> Self {
        Self {
            inner: anchor_leaf,
            instance_state,
            validated_state,
            state_delta: None,
            start_view,
            saved_proposals,
            high_qc,
            undecided_leafs,
            undecided_state,
        }
    }
}
