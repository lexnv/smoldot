// Smoldot
// Copyright (C) 2023  Pierre Krieger
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! State machine of all networking connections.
//!
//! The [`ChainNetwork`] struct is a state machine containing multiple collections that are
//! controlled by the API user:
//!
//! - A list of networking connections, identified by a [`ConnectionId`]. Each connection is
//! either in the handshake phase, healthy, or shutting down. Each connection in the handshake
//! phase has an optional expected [`PeerId`] representing the identity of the node that is
//! expected to be reached once the handshake is finished. Each connection that is healthy or
//! shutting down has an (actual) [`PeerId`] associated to it.
//! - A list of chains, identified by a [`ChainId`].
//! - A set of "desired" `(ChainId, PeerId, GossipKind)` tuples representing, for each chain, the
//! identities of the nodes that the API user wants to establish a gossip link with.
//!
//! In addition to this, the [`ChainNetwork`] also exposes:
//!
//! - A set of `(ChainId, PeerId, GossipKind)` tuples representing the gossip links that have been
//! established.
//! - A list of outgoing requests, identified by a [`SubstreamId`], that have been sent to a peer
//! and that are awaiting a response.
//! - A list of ingoing requests, identified by a [`SubstreamId`], that have been received from a
//! peer and that must be answered by the API user.
//! - A list of outgoing gossip link connection attempts, identified by a [`SubstreamId`], that
//! must be answered by the peer.
//! - A set of `(ChainId, PeerId, GossipKind)` tuples representing the peers that would like to
//! establish a gossip link with the local node, and that are awaiting a response by the API user.
//!
//! # Usage
//!
//! At initialization, create a new [`ChainNetwork`] with [`ChainNetwork::new`], and add chains
//! using [`ChainNetwork::add_chain`].
//!
//! The [`ChainNetwork`] doesn't automatically open connections to peers. This must be done
//! manually using [`ChainNetwork::add_single_stream_connection`] or
//! [`ChainNetwork::add_multi_stream_connection`]. Choosing which peer to connect to and through
//! which address is outside of the scope of this module.
//!
//! Adding a connection using [`ChainNetwork::add_single_stream_connection`] or
//! [`ChainNetwork::add_multi_stream_connection`] returns a "connection task". This connection task
//! must be processed. TODO: expand explanation here
//!
//! After a message has been injected using [`ChainNetwork::inject_connection_message`], repeatedly
//! [`ChainNetwork::next_event`] until it returns `None` in order to determine what has happened.
//!
//! Once a connection has been established (which is indicated by a [`Event::HandshakeFinished`]
//! event), one can open a gossip link to this peer using [`ChainNetwork::gossip_open`].
//!
//! In order to faciliate this process, the [`ChainNetwork`] provides a "desired gossip links"
//! system. Use [`ChainNetwork::gossip_insert_desired`] and [`ChainNetwork::gossip_remove_desired`]
//! to insert or remove `(ChainId, PeerId, GossipKind)` tuples into the state machine. You can
//! then use [`ChainNetwork::unconnected_desired`] to obtain a list of [`PeerId`]s that are marked
//! as desired and for which a connection should be opened, and
//! [`ChainNetwork::connected_unopened_gossip_desired`] to obtain a list of [`PeerId`]s that are
//! marked as desired and that have a healthy connection and for which a gossip link should be
//! opened. Marking peers as desired only influences the return values of
//! [`ChainNetwork::unconnected_desired`] and [`ChainNetwork::connected_unopened_gossip_desired`]
//! and has no other effect.
//!

// TODO: expand explanations once the API is finalized

use crate::libp2p::collection;
use crate::network::protocol;
use crate::util::{self, SipHasherBuild};

use alloc::{borrow::ToOwned as _, collections::BTreeSet, string::String, vec::Vec};
use core::{
    fmt,
    hash::Hash,
    iter, mem,
    ops::{Add, Sub},
    time::Duration,
};
use rand_chacha::rand_core::{RngCore as _, SeedableRng as _};

pub use crate::libp2p::{
    collection::{
        ConnectionId, ConnectionToCoordinator, CoordinatorToConnection, InboundError,
        MultiStreamConnectionTask, NotificationsOutErr, ReadWrite, RequestError,
        SingleStreamConnectionTask, SubstreamId,
    },
    connection::noise::{self, NoiseKey},
    multiaddr::{self, Multiaddr},
    peer_id::{self, PeerId},
};

pub use crate::network::protocol::{BlockAnnouncesHandshakeDecodeError, Role};

/// Configuration for a [`ChainNetwork`].
pub struct Config {
    /// Capacity to initially reserve to the list of connections.
    pub connections_capacity: usize,

    /// Capacity to reserve for the list of chains.
    pub chains_capacity: usize,

    /// Seed for the randomness within the networking state machine.
    ///
    /// While this seed influences the general behavior of the networking state machine, it
    /// notably isn't used when generating the ephemeral key used for the Diffie-Hellman
    /// handshake.
    /// This is a defensive measure against users passing a dummy seed instead of actual entropy.
    pub randomness_seed: [u8; 32],

    /// Key used for the encryption layer.
    /// This is a Noise static key, according to the Noise specification.
    /// Signed using the actual libp2p key.
    pub noise_key: NoiseKey,

    /// Amount of time after which a connection hathat ndshake is considered to have taken too long
    /// and must be aborted.
    pub handshake_timeout: Duration,
}

/// Configuration for a specific overlay network.
///
/// See [`ChainNetwork::add_chain`].
pub struct ChainConfig {
    /// Hash of the genesis block (i.e. block number 0) according to the local node.
    pub genesis_hash: [u8; 32],

    /// Optional identifier to insert into the networking protocol names. Used to differentiate
    /// between chains with the same genesis hash.
    ///
    /// > **Note**: This value is typically found in the specification of the chain (the
    /// >           "chain spec").
    pub fork_id: Option<String>,

    /// Number of bytes of the block number in the networking protocol.
    pub block_number_bytes: usize,

    /// If `Some`, the chain uses the GrandPa networking protocol.
    pub grandpa_protocol_config: Option<GrandpaState>,

    /// `true` if incoming block requests are allowed.
    pub allow_inbound_block_requests: bool,

    /// Hash of the best block according to the local node.
    pub best_hash: [u8; 32],
    /// Height of the best block according to the local node.
    pub best_number: u64,

    /// Role of the local node. Sent to the remote nodes and used as a hint. Has no incidence
    /// on the behavior of any function.
    pub role: Role,
}

/// Identifier of a chain added through [`ChainNetwork::add_chain`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainId(usize);

/// Data structure containing the list of all connections and their latest known state. See also
/// [the module-level documentation](..).
pub struct ChainNetwork<TNow> {
    /// Underlying data structure.
    inner: collection::Network<ConnectionInfo, TNow>,

    /// List of all chains that have been added.
    // TODO: shrink to fit from time to time
    chains: slab::Slab<Chain>,

    /// All the substreams of [`ChainNetwork::inner`], with info attached to them.
    // TODO: add a substream user data to `collection::Network` instead
    // TODO: shrink to fit from time to time
    substreams: hashbrown::HashMap<SubstreamId, SubstreamInfo, fnv::FnvBuildHasher>,

    /// Connections indexed by the value in [`ConnectionInfo::peer_id`].
    connections_by_peer_id: BTreeSet<(PeerId, collection::ConnectionId)>,

    /// All the outbound notification substreams, indexed by protocol, `PeerId`, and state.
    // TODO: unclear whether PeerId should come before or after the state, same for direction/state
    notification_substreams_by_peer_id: BTreeSet<(
        NotificationsProtocol,
        PeerId,
        SubstreamDirection,
        NotificationsSubstreamState,
        collection::SubstreamId,
    )>,

    /// See [`Config::noise_key`].
    // TODO: make rotatable, see <https://github.com/smol-dot/smoldot/issues/44>
    noise_key: NoiseKey,

    /// Chains indexed by genesis hash and fork ID.
    ///
    /// Contains the same number of entries as [`ChainNetwork::chains`]. The values are `usize`s
    /// that are indices into [`ChainNetwork::chains`].
    // TODO: shrink to fit from time to time
    chains_by_protocol_info:
        hashbrown::HashMap<([u8; 32], Option<String>), usize, fnv::FnvBuildHasher>,

    /// List of peers that have been marked as desired. Can include peers not connected to the
    /// local node yet.
    gossip_desired_peers_by_chain: BTreeSet<(usize, GossipKind, PeerId)>,

    /// Same entries as [`ChainNetwork::gossip_desired_peers_by_chain`] but indexed differently.
    gossip_desired_peers: BTreeSet<(PeerId, GossipKind, usize)>,

    /// Subset of peers in [`ChainNetwork::gossip_desired_peers`] for which no healthy
    /// connection exists.
    // TODO: shrink to fit from time to time
    unconnected_desired: hashbrown::HashSet<PeerId, util::SipHasherBuild>,

    /// List of [`PeerId`]s that are marked as desired, and for which a healthy connection exists,
    /// but for which no substream connection (attempt or established) exists.
    // TODO: shrink to fit from time to time
    connected_unopened_gossip_desired:
        hashbrown::HashSet<(PeerId, ChainId, GossipKind), util::SipHasherBuild>,

    /// List of [`PeerId`]s for which a substream connection (attempt or established) exists, but
    /// that are not marked as desired.
    // TODO: shrink to fit from time to time
    opened_gossip_undesired:
        hashbrown::HashSet<(ChainId, PeerId, GossipKind), util::SipHasherBuild>,
}

struct Chain {
    /// See [`ChainConfig::block_number_bytes`].
    block_number_bytes: usize,

    /// See [`ChainConfig::genesis_hash`].
    genesis_hash: [u8; 32],
    /// See [`ChainConfig::fork_id`].
    fork_id: Option<String>,

    /// See [`ChainConfig::role`].
    role: Role,

    /// See [`ChainConfig::best_hash`].
    best_hash: [u8; 32],
    /// See [`ChainConfig::best_number`].
    best_number: u64,

    /// See [`ChainConfig::grandpa_protocol_config`].
    grandpa_protocol_config: Option<GrandpaState>,

    /// See [`ChainConfig::allow_inbound_block_requests`].
    allow_inbound_block_requests: bool,
}

/// See [`ChainNetwork::inner`].
struct ConnectionInfo {
    address: Vec<u8>,

    /// Identity of the remote. Can be either the expected or the actual identity.
    ///
    /// `None` if unknown, which can only be the case if the connection is still in its handshake
    /// phase.
    peer_id: Option<PeerId>,
}

/// See [`ChainNetwork::substreams`].
#[derive(Debug, Clone)]
struct SubstreamInfo {
    // TODO: substream <-> connection mapping should be provided by collection.rs instead
    connection_id: collection::ConnectionId,
    protocol: Protocol,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Protocol {
    Identify,
    Ping,
    BlockAnnounces { chain_index: usize },
    Transactions { chain_index: usize },
    Grandpa { chain_index: usize },
    Sync { chain_index: usize },
    LightUnknown { chain_index: usize },
    LightStorage { chain_index: usize },
    LightCall { chain_index: usize },
    Kad { chain_index: usize },
    SyncWarp { chain_index: usize },
    State { chain_index: usize },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum NotificationsProtocol {
    BlockAnnounces { chain_index: usize },
    Transactions { chain_index: usize },
    Grandpa { chain_index: usize },
}

impl TryFrom<Protocol> for NotificationsProtocol {
    type Error = ();

    fn try_from(value: Protocol) -> Result<Self, Self::Error> {
        match value {
            Protocol::BlockAnnounces { chain_index } => {
                Ok(NotificationsProtocol::BlockAnnounces { chain_index })
            }
            Protocol::Transactions { chain_index } => {
                Ok(NotificationsProtocol::Transactions { chain_index })
            }
            Protocol::Grandpa { chain_index } => Ok(NotificationsProtocol::Grandpa { chain_index }),
            Protocol::Identify => Err(()),
            Protocol::Ping => Err(()),
            Protocol::Sync { .. } => Err(()),
            Protocol::LightUnknown { .. } => Err(()),
            Protocol::LightStorage { .. } => Err(()),
            Protocol::LightCall { .. } => Err(()),
            Protocol::Kad { .. } => Err(()),
            Protocol::SyncWarp { .. } => Err(()),
            Protocol::State { .. } => Err(()),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum SubstreamDirection {
    In,
    Out,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum NotificationsSubstreamState {
    Pending,
    Open,
}

impl NotificationsSubstreamState {
    fn min_value() -> Self {
        NotificationsSubstreamState::Pending
    }

    fn max_value() -> Self {
        NotificationsSubstreamState::Open
    }
}

impl<TNow> ChainNetwork<TNow>
where
    TNow: Clone + Add<Duration, Output = TNow> + Sub<TNow, Output = Duration> + Ord,
{
    /// Initializes a new [`ChainNetwork`].
    pub fn new(config: Config) -> Self {
        let mut randomness = rand_chacha::ChaCha20Rng::from_seed(config.randomness_seed);

        ChainNetwork {
            inner: collection::Network::new(collection::Config {
                capacity: config.connections_capacity,
                max_inbound_substreams: 128, // TODO: arbitrary value ; this value should be dynamically adjusted based on the number of chains that have been added
                randomness_seed: {
                    let mut seed = [0; 32];
                    randomness.fill_bytes(&mut seed);
                    seed
                },
                ping_protocol: "/ipfs/ping/1.0.0".into(),
                handshake_timeout: config.handshake_timeout,
            }),
            substreams: hashbrown::HashMap::with_capacity_and_hasher(
                config.connections_capacity * 20, // TODO: capacity?
                fnv::FnvBuildHasher::default(),
            ),
            connections_by_peer_id: BTreeSet::new(),
            notification_substreams_by_peer_id: BTreeSet::new(),
            gossip_desired_peers_by_chain: BTreeSet::new(),
            gossip_desired_peers: BTreeSet::new(),
            unconnected_desired: hashbrown::HashSet::with_capacity_and_hasher(
                config.connections_capacity,
                SipHasherBuild::new({
                    let mut seed = [0; 16];
                    randomness.fill_bytes(&mut seed);
                    seed
                }),
            ),
            connected_unopened_gossip_desired: hashbrown::HashSet::with_capacity_and_hasher(
                config.connections_capacity,
                SipHasherBuild::new({
                    let mut seed = [0; 16];
                    randomness.fill_bytes(&mut seed);
                    seed
                }),
            ),
            opened_gossip_undesired: hashbrown::HashSet::with_capacity_and_hasher(
                config.connections_capacity,
                SipHasherBuild::new({
                    let mut seed = [0; 16];
                    randomness.fill_bytes(&mut seed);
                    seed
                }),
            ),
            chains: slab::Slab::with_capacity(config.chains_capacity),
            chains_by_protocol_info: hashbrown::HashMap::with_capacity_and_hasher(
                config.chains_capacity,
                Default::default(),
            ),
            noise_key: config.noise_key,
        }
    }

    /// Returns the Noise key originally passed as [`Config::noise_key`].
    pub fn noise_key(&self) -> &NoiseKey {
        &self.noise_key
    }

    /// Adds a chain to the list of chains that is handled by the [`ChainNetwork`].
    ///
    /// It is not possible to add a chain if its protocol names would conflict with an existing
    /// chain.
    pub fn add_chain(&mut self, config: ChainConfig) -> Result<ChainId, AddChainError> {
        let chain_entry = self.chains.vacant_entry();
        let chain_id = chain_entry.key();

        match self
            .chains_by_protocol_info
            .entry((config.genesis_hash, config.fork_id.clone()))
        {
            hashbrown::hash_map::Entry::Vacant(entry) => {
                entry.insert(chain_id);
            }
            hashbrown::hash_map::Entry::Occupied(entry) => {
                return Err(AddChainError::Duplicate {
                    existing_identical: ChainId(*entry.get()),
                })
            }
        }

        chain_entry.insert(Chain {
            block_number_bytes: config.block_number_bytes,
            genesis_hash: config.genesis_hash,
            fork_id: config.fork_id,
            role: config.role,
            best_hash: config.best_hash,
            best_number: config.best_number,
            allow_inbound_block_requests: config.allow_inbound_block_requests,
            grandpa_protocol_config: config.grandpa_protocol_config,
        });

        Ok(ChainId(chain_id))
    }

    // TODO: add `fn remove_chain(&mut self, chain_id: ChainId)` but the behavior w.r.t. closing that chain's substreams is tricky

    /// Modifies the best block of the local node for the given chain. See
    /// [`ChainConfig::best_hash`] and [`ChainConfig::best_number`].
    ///
    /// This information is sent to remotes whenever a block announces substream is opened.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is out of range.
    ///
    pub fn set_chain_local_best_block(
        &mut self,
        chain_id: ChainId,
        best_hash: [u8; 32],
        best_number: u64,
    ) {
        let chain = &mut self.chains[chain_id.0];
        chain.best_hash = best_hash;
        chain.best_number = best_number;
    }

    /// Returns the list of all the chains that have been added.
    pub fn chains(&'_ self) -> impl Iterator<Item = ChainId> + '_ {
        self.chains.iter().map(|(idx, _)| ChainId(idx))
    }

    /// Returns the value passed as [`ChainConfig::block_number_bytes`] for the given chain.
    ///
    /// # Panic
    ///
    /// Panics if the given [`ChainId`] is invalid.
    ///
    pub fn block_number_bytes(&self, chain_id: ChainId) -> usize {
        self.chains[chain_id.0].block_number_bytes
    }

    /// Marks the given chain-peer combination as "desired".
    ///
    /// Has no effect if it was already marked as desired.
    ///
    /// Returns `true` if the peer has been marked as desired, and `false` if it was already
    /// marked as desired.
    ///
    /// # Panic
    ///
    /// Panics if the given [`ChainId`] is invalid.
    ///
    pub fn gossip_insert_desired(
        &mut self,
        chain_id: ChainId,
        peer_id: PeerId,
        kind: GossipKind,
    ) -> bool {
        assert!(self.chains.contains(chain_id.0));

        // TODO: a lot of peerid cloning overhead in this function

        if !self
            .gossip_desired_peers_by_chain
            .insert((chain_id.0, kind, peer_id.clone()))
        {
            // Return if already marked as desired, as there's nothing more to update.
            // Note that this doesn't cover the possibility where the peer was desired with
            // another chain.
            return false;
        }

        let _was_inserted = self
            .gossip_desired_peers
            .insert((peer_id.clone(), kind, chain_id.0));
        debug_assert!(_was_inserted);

        self.opened_gossip_undesired
            .remove(&(chain_id, peer_id.clone(), kind));

        //  Add either to `unconnected_desired` or to `connected_unopened_gossip_desired`,
        // depending on the situation.
        if self
            .connections_by_peer_id
            .range(
                (peer_id.clone(), ConnectionId::min_value())
                    ..=(peer_id.clone(), ConnectionId::max_value()),
            )
            .any(|(_, connection_id)| {
                let state = self.inner.connection_state(*connection_id);
                !state.shutting_down
            })
        {
            if self
                .notification_substreams_by_peer_id
                .range(
                    (
                        NotificationsProtocol::BlockAnnounces {
                            chain_index: chain_id.0,
                        },
                        peer_id.clone(),
                        SubstreamDirection::Out,
                        NotificationsSubstreamState::min_value(),
                        SubstreamId::min_value(),
                    )
                        ..=(
                            NotificationsProtocol::BlockAnnounces {
                                chain_index: chain_id.0,
                            },
                            peer_id.clone(),
                            SubstreamDirection::Out,
                            NotificationsSubstreamState::max_value(),
                            SubstreamId::max_value(),
                        ),
                )
                .next()
                .is_none()
            {
                let _was_inserted = self.connected_unopened_gossip_desired.insert((
                    peer_id.clone(),
                    chain_id,
                    kind,
                ));
                debug_assert!(_was_inserted);
            }
        } else {
            // Note that that `PeerId` might already be desired towards a different chain, in
            // which case it is already present in `unconnected_desired`.
            self.unconnected_desired.insert(peer_id);
        }

        true
    }

    /// Removes the given chain-peer combination from the list of desired chain-peers.
    ///
    /// Has no effect if it was not marked as desired.
    ///
    /// Returns `true` if the peer was desired on this chain.
    ///
    /// # Panic
    ///
    /// Panics if the given [`ChainId`] is invalid.
    ///
    pub fn gossip_remove_desired(
        &mut self,
        chain_id: ChainId,
        peer_id: &PeerId,
        kind: GossipKind,
    ) -> bool {
        assert!(self.chains.contains(chain_id.0));

        if !self
            .gossip_desired_peers_by_chain
            .remove(&(chain_id.0, kind, peer_id.clone()))
        // TODO: spurious cloning
        {
            // Return if wasn't marked as desired, as there's nothing more to update.
            return false;
        }

        self.gossip_desired_peers
            .remove(&(peer_id.clone(), kind, chain_id.0));

        self.connected_unopened_gossip_desired
            .remove(&(peer_id.clone(), chain_id, kind)); // TODO: cloning

        if self
            .gossip_desired_peers
            .range(
                (peer_id.clone(), kind, usize::min_value())
                    ..=(peer_id.clone(), kind, usize::max_value()),
            )
            .next()
            .is_none()
        {
            self.unconnected_desired.remove(peer_id);
        }

        if self
            .notification_substreams_by_peer_id
            .range(
                (
                    NotificationsProtocol::BlockAnnounces {
                        chain_index: chain_id.0,
                    },
                    peer_id.clone(),
                    SubstreamDirection::Out,
                    NotificationsSubstreamState::min_value(),
                    SubstreamId::min_value(),
                )
                    ..=(
                        NotificationsProtocol::BlockAnnounces {
                            chain_index: chain_id.0,
                        },
                        peer_id.clone(),
                        SubstreamDirection::Out,
                        NotificationsSubstreamState::max_value(),
                        SubstreamId::max_value(),
                    ),
            )
            .next()
            .is_some()
        {
            let _was_inserted =
                self.opened_gossip_undesired
                    .insert((chain_id, peer_id.clone(), kind));
            debug_assert!(_was_inserted);
        }

        true
    }

    /// Removes the given peer from the list of desired chain-peers of all the chains that exist.
    ///
    /// Has no effect if it was not marked as desired.
    pub fn gossip_remove_desired_all(&mut self, peer_id: &PeerId, kind: GossipKind) {
        let chains = self
            .gossip_desired_peers
            .range(
                (peer_id.clone(), kind, usize::min_value())
                    ..=(peer_id.clone(), kind, usize::max_value()),
            )
            .map(|(_, _, chain_index)| *chain_index)
            .collect::<Vec<_>>();

        for chain_index in chains {
            let _was_in =
                self.gossip_desired_peers_by_chain
                    .remove(&(chain_index, kind, peer_id.clone()));
            debug_assert!(_was_in);
            let _was_in = self
                .gossip_desired_peers
                .remove(&(peer_id.clone(), kind, chain_index));
            debug_assert!(_was_in);
            self.connected_unopened_gossip_desired.remove(&(
                peer_id.clone(),
                ChainId(chain_index),
                kind,
            ));
        }

        self.unconnected_desired.remove(peer_id);
    }

    /// Returns the number of gossip-desired peers for the given chain.
    ///
    /// # Panic
    ///
    /// Panics if the given [`ChainId`] is invalid.
    ///
    pub fn gossip_desired_num(&mut self, chain_id: ChainId, kind: GossipKind) -> usize {
        // TODO: O(n), optimize
        self.gossip_desired_peers_by_chain
            .iter()
            .filter(|(c, k, _)| *c == chain_id.0 && *k == kind)
            .count()
    }

    /// Returns the list of [`PeerId`]s that are desired (for any chain) but for which no
    /// connection exists.
    ///
    /// > **Note**: Connections that are currently in the process of shutting down are also
    /// >           ignored for the purpose of this function.
    pub fn unconnected_desired(&'_ self) -> impl ExactSizeIterator<Item = &'_ PeerId> + Clone + '_ {
        self.unconnected_desired.iter()
    }

    /// Returns the list of [`PeerId`]s that are marked as desired, and for which a healthy
    /// connection exists, but for which no substream connection attempt exists.
    pub fn connected_unopened_gossip_desired(
        &'_ self,
    ) -> impl ExactSizeIterator<Item = (&'_ PeerId, ChainId, GossipKind)> + Clone + '_ {
        self.connected_unopened_gossip_desired
            .iter()
            .map(move |(peer_id, chain_id, gossip_kind)| (peer_id, *chain_id, *gossip_kind))
    }

    /// Returns the list of [`PeerId`]s for which a substream connection or connection attempt
    /// exists but that are not marked as desired.
    pub fn opened_gossip_undesired(
        &'_ self,
    ) -> impl ExactSizeIterator<Item = (&'_ PeerId, ChainId, GossipKind)> + Clone + '_ {
        self.opened_gossip_undesired
            .iter()
            .map(move |(chain_id, peer_id, gossip_kind)| (peer_id, *chain_id, *gossip_kind))
    }

    /// Returns the list of [`PeerId`]s for which a substream connection or connection attempt
    /// exists against the given chain but that are not marked as desired.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    pub fn opened_gossip_undesired_by_chain(
        &'_ self,
        chain_id: ChainId,
    ) -> impl Iterator<Item = (&'_ PeerId, GossipKind)> + Clone + '_ {
        // TODO: optimize and add an ExactSizeIterator bound to the return value, and update the users to use len() instead of count()
        self.opened_gossip_undesired
            .iter()
            .filter(move |(c, _, _)| *c == chain_id)
            .map(move |(_, peer_id, gossip_kind)| (peer_id, *gossip_kind))
    }

    /// Adds a single-stream connection to the state machine.
    ///
    /// This connection hasn't finished handshaking and the [`PeerId`] of the remote isn't known
    /// yet.
    ///
    /// If `expected_peer_id` is `Some`, this connection is expected to reach the given [`PeerId`].
    /// The `expected_peer_id` is only used to influence the result of
    /// [`ChainNetwork::unconnected_desired`].
    ///
    /// Must be passed the moment (as a `TNow`) when the connection has first been opened, in
    /// order to determine when the handshake timeout expires.
    ///
    /// The `remote_addr` is the multiaddress used to reach back the remote. In the case of TCP, it
    /// contains the TCP dialing port of the remote. The remote can ask, through the `identify`
    /// libp2p protocol, its own address, in which case we send it. Because the multiaddress
    /// specification is flexible, this module doesn't attempt to parse the address.
    pub fn add_single_stream_connection(
        &mut self,
        when_connection_start: TNow,
        handshake_kind: SingleStreamHandshakeKind,
        remote_addr: Vec<u8>,
        expected_peer_id: Option<PeerId>,
    ) -> (ConnectionId, SingleStreamConnectionTask<TNow>) {
        // TODO: do the max protocol name length better ; knowing that it can later change if a chain with a long forkId is added
        let max_protocol_name_len = 256;
        let substreams_capacity = 16; // TODO: ?
        let (id, task) = self.inner.insert_single_stream(
            when_connection_start,
            match handshake_kind {
                SingleStreamHandshakeKind::MultistreamSelectNoiseYamux { is_initiator } => {
                    collection::SingleStreamHandshakeKind::MultistreamSelectNoiseYamux {
                        is_initiator,
                        noise_key: &self.noise_key,
                    }
                }
            },
            substreams_capacity,
            max_protocol_name_len,
            ConnectionInfo {
                address: remote_addr,
                peer_id: expected_peer_id.clone(),
            },
        );
        if let Some(expected_peer_id) = expected_peer_id {
            self.unconnected_desired.remove(&expected_peer_id);
            self.connections_by_peer_id.insert((expected_peer_id, id));
        }
        (id, task)
    }

    /// Adds a multi-stream connection to the state machine.
    ///
    /// This connection hasn't finished handshaking and the [`PeerId`] of the remote isn't known
    /// yet.
    ///
    /// If `expected_peer_id` is `Some`, this connection is expected to reach the given [`PeerId`].
    /// The `expected_peer_id` is only used to influence the result of
    /// [`ChainNetwork::unconnected_desired`].
    ///
    /// Must be passed the moment (as a `TNow`) when the connection has first been opened, in
    /// order to determine when the handshake timeout expires.
    ///
    /// The `remote_addr` is the multiaddress used to reach back the remote. In the case of TCP, it
    /// contains the TCP dialing port of the remote. The remote can ask, through the `identify`
    /// libp2p protocol, its own address, in which case we send it. Because the multiaddress
    /// specification is flexible, this module doesn't attempt to parse the address.
    pub fn add_multi_stream_connection<TSubId>(
        &mut self,
        when_connection_start: TNow,
        handshake_kind: MultiStreamHandshakeKind,
        remote_addr: Vec<u8>,
        expected_peer_id: Option<PeerId>,
    ) -> (ConnectionId, MultiStreamConnectionTask<TNow, TSubId>)
    where
        TSubId: Clone + PartialEq + Eq + Hash,
    {
        // TODO: do the max protocol name length better ; knowing that it can later change if a chain with a long forkId is added
        let max_protocol_name_len = 256;
        let substreams_capacity = 16; // TODO: ?
        let (id, task) = self.inner.insert_multi_stream(
            when_connection_start,
            match handshake_kind {
                MultiStreamHandshakeKind::WebRtc {
                    is_initiator,
                    local_tls_certificate_multihash,
                    remote_tls_certificate_multihash,
                } => collection::MultiStreamHandshakeKind::WebRtc {
                    is_initiator,
                    noise_key: &self.noise_key,
                    local_tls_certificate_multihash,
                    remote_tls_certificate_multihash,
                },
            },
            substreams_capacity,
            max_protocol_name_len,
            ConnectionInfo {
                address: remote_addr,
                peer_id: expected_peer_id.clone(),
            },
        );
        if let Some(expected_peer_id) = expected_peer_id {
            self.unconnected_desired.remove(&expected_peer_id);
            self.connections_by_peer_id.insert((expected_peer_id, id));
        }
        (id, task)
    }

    /// Returns the number of connections, both handshaking or established.
    pub fn num_connections(&self) -> usize {
        self.inner.len()
    }

    /// Returns the remote address that was passed to [`ChainNetwork::add_single_stream_connection`]
    /// or [`ChainNetwork::add_multi_stream_connection`] for the given connection.
    ///
    /// > **Note**: This module does in no way attempt to parse the address. This function simply
    /// >           returns the value that was provided by the API user, whatever it is.
    ///
    /// # Panic
    ///
    /// Panics if the [`ConnectionId`] is invalid.
    ///
    pub fn connection_remote_addr(&self, id: ConnectionId) -> &[u8] {
        &self.inner[id].address
    }

    /// Pulls a message that must be sent to a connection.
    ///
    /// The message must be passed to [`SingleStreamConnectionTask::inject_coordinator_message`]
    /// or [`MultiStreamConnectionTask::inject_coordinator_message`] in the appropriate connection.
    ///
    /// This function guarantees that the [`ConnectionId`] always refers to a connection that
    /// is still alive, in the sense that [`SingleStreamConnectionTask::inject_coordinator_message`]
    /// or [`MultiStreamConnectionTask::inject_coordinator_message`] has never returned `None`.
    pub fn pull_message_to_connection(
        &mut self,
    ) -> Option<(ConnectionId, CoordinatorToConnection)> {
        self.inner.pull_message_to_connection()
    }

    /// Injects into the state machine a message generated by
    /// [`SingleStreamConnectionTask::pull_message_to_coordinator`] or
    /// [`MultiStreamConnectionTask::pull_message_to_coordinator`].
    pub fn inject_connection_message(
        &mut self,
        connection_id: ConnectionId,
        message: ConnectionToCoordinator,
    ) {
        self.inner.inject_connection_message(connection_id, message)
    }

    /// Returns the next event produced by the service.
    pub fn next_event(&mut self) -> Option<Event> {
        loop {
            let inner_event = self.inner.next_event()?;
            match inner_event {
                collection::Event::HandshakeFinished {
                    id,
                    peer_id: actual_peer_id,
                } => {
                    // Store the actual `PeerId` into the connection, making sure to update `self`.
                    let connection_info = &mut self.inner[id];
                    let expected_peer_id = connection_info.peer_id.clone();
                    match &mut connection_info.peer_id {
                        Some(expected_peer_id) if *expected_peer_id == actual_peer_id => {}
                        peer_id_refmut @ None => {
                            self.unconnected_desired.remove(&actual_peer_id);
                            *peer_id_refmut = Some(actual_peer_id.clone());
                        }
                        Some(peer_id_refmut) => {
                            // The actual PeerId doesn't match the expected PeerId.
                            let expected_peer_id =
                                mem::replace(peer_id_refmut, actual_peer_id.clone());

                            let _was_removed = self
                                .connections_by_peer_id
                                .remove(&(expected_peer_id.clone(), id));
                            debug_assert!(_was_removed);
                            if self
                                .gossip_desired_peers
                                .range(
                                    (
                                        expected_peer_id.clone(),
                                        GossipKind::ConsensusTransactions,
                                        usize::min_value(),
                                    )
                                        ..=(
                                            expected_peer_id.clone(),
                                            GossipKind::ConsensusTransactions,
                                            usize::max_value(),
                                        ),
                                )
                                .next()
                                .is_some()
                            {
                                if !self
                                    .connections_by_peer_id
                                    .range(
                                        (expected_peer_id.clone(), ConnectionId::min_value())
                                            ..=(
                                                expected_peer_id.clone(),
                                                ConnectionId::max_value(),
                                            ),
                                    )
                                    .any(|(_, connection_id)| {
                                        let state = self.inner.connection_state(*connection_id);
                                        !state.shutting_down
                                    })
                                {
                                    let _was_inserted =
                                        self.unconnected_desired.insert(expected_peer_id.clone());
                                    debug_assert!(_was_inserted);
                                }
                            }
                            let _was_inserted = self
                                .connections_by_peer_id
                                .insert((actual_peer_id.clone(), id));
                            debug_assert!(_was_inserted);
                            self.unconnected_desired.remove(&actual_peer_id);
                        }
                    }

                    debug_assert!(!self.unconnected_desired.contains(&actual_peer_id));

                    // TODO: limit the number of connections per peer?

                    for (_, _, chain_id) in self.gossip_desired_peers.range(
                        (
                            actual_peer_id.clone(),
                            GossipKind::ConsensusTransactions,
                            usize::min_value(),
                        )
                            ..=(
                                actual_peer_id.clone(),
                                GossipKind::ConsensusTransactions,
                                usize::max_value(),
                            ),
                    ) {
                        if self
                            .notification_substreams_by_peer_id
                            .range(
                                (
                                    NotificationsProtocol::BlockAnnounces {
                                        chain_index: *chain_id,
                                    },
                                    actual_peer_id.clone(),
                                    SubstreamDirection::Out,
                                    NotificationsSubstreamState::min_value(),
                                    SubstreamId::min_value(),
                                )
                                    ..=(
                                        NotificationsProtocol::BlockAnnounces {
                                            chain_index: *chain_id,
                                        },
                                        actual_peer_id.clone(),
                                        SubstreamDirection::Out,
                                        NotificationsSubstreamState::max_value(),
                                        SubstreamId::max_value(),
                                    ),
                            )
                            .next()
                            .is_none()
                        {
                            self.connected_unopened_gossip_desired.insert((
                                actual_peer_id.clone(),
                                ChainId(*chain_id),
                                GossipKind::ConsensusTransactions,
                            ));
                        }
                    }

                    return Some(Event::HandshakeFinished {
                        id,
                        expected_peer_id,
                        peer_id: actual_peer_id,
                    });
                }

                collection::Event::PingOutFailed { id }
                | collection::Event::StartShutdown { id, .. } => {
                    if let collection::Event::PingOutFailed { .. } = inner_event {
                        self.inner.start_shutdown(id);
                    }

                    // TODO: IMPORTANT this event should be turned into `NewOutboundSubstreamsForbidden` and the `reason` removed; see <https://github.com/smol-dot/smoldot/pull/391>

                    let connection_info = &self.inner[id];

                    // If peer is desired, and we have no connection or only shutting down
                    // connections, add peer to `unconnected_desired` and remove it from
                    // `connected_unopened_gossip_desired`.
                    if let Some(peer_id) = &connection_info.peer_id {
                        if self
                            .gossip_desired_peers
                            .range(
                                (
                                    peer_id.clone(),
                                    GossipKind::ConsensusTransactions,
                                    usize::min_value(),
                                )
                                    ..=(
                                        peer_id.clone(),
                                        GossipKind::ConsensusTransactions,
                                        usize::max_value(),
                                    ),
                            )
                            .count()
                            != 0
                        {
                            if !self
                                .connections_by_peer_id
                                .range(
                                    (peer_id.clone(), ConnectionId::min_value())
                                        ..=(peer_id.clone(), ConnectionId::max_value()),
                                )
                                .any(|(_, connection_id)| {
                                    let state = self.inner.connection_state(*connection_id);
                                    !state.shutting_down
                                })
                            {
                                self.unconnected_desired.insert(peer_id.clone());
                                for (_, _, chain_index) in self.gossip_desired_peers.range(
                                    (
                                        peer_id.clone(),
                                        GossipKind::ConsensusTransactions,
                                        usize::min_value(),
                                    )
                                        ..=(
                                            peer_id.clone(),
                                            GossipKind::ConsensusTransactions,
                                            usize::max_value(),
                                        ),
                                ) {
                                    self.connected_unopened_gossip_desired.remove(&(
                                        peer_id.clone(),
                                        ChainId(*chain_index),
                                        GossipKind::ConsensusTransactions,
                                    ));
                                }
                            }
                        }
                    }
                }

                collection::Event::Shutdown {
                    id,
                    was_established,
                    user_data: connection_info,
                } => {
                    // A connection has been closed.
                    // Note that the underlying state machine guarantees that all the substreams
                    // have been closed beforehand through other events.

                    debug_assert!(connection_info.peer_id.is_some() || !was_established);

                    if let Some(peer_id) = &connection_info.peer_id {
                        let _was_removed =
                            self.connections_by_peer_id.remove(&(peer_id.clone(), id));
                        debug_assert!(_was_removed);
                    }

                    // TODO: IMPORTANT this event should indicate a clean shutdown, a pre-handshake interruption, a protocol error, a reset, etc. and should get a `reason`; see <https://github.com/smol-dot/smoldot/pull/391>

                    if was_established {
                        return Some(Event::Disconnected {
                            id,
                            address: connection_info.address,
                            peer_id: connection_info.peer_id.unwrap(),
                        });
                    } else {
                        return Some(Event::PreHandshakeDisconnected {
                            id,
                            address: connection_info.address,
                            expected_peer_id: connection_info.peer_id,
                        });
                    }
                }

                collection::Event::InboundError { .. } => {
                    // TODO: report the error for diagnostic purposes, but revisit the concept of "InboundError"
                    continue;
                }

                collection::Event::InboundNegotiated {
                    id,
                    substream_id,
                    protocol_name,
                } => {
                    // An inbound substream has negotiated a protocol. We must decide whether to
                    // accept this protocol or instead reject the substream.
                    // If accepted, we must also save the protocol somewhere in `self` in order to
                    // load it later once things happen on this substream.
                    match self.recognize_protocol(&protocol_name) {
                        Ok(protocol) => {
                            let inbound_type = match protocol {
                                Protocol::Identify => collection::InboundTy::Request {
                                    request_max_size: None,
                                },
                                Protocol::Ping => collection::InboundTy::Ping,
                                Protocol::BlockAnnounces { .. } => {
                                    collection::InboundTy::Notifications {
                                        max_handshake_size: 1024 * 1024, // TODO: arbitrary
                                    }
                                }
                                Protocol::Transactions { .. } => {
                                    collection::InboundTy::Notifications {
                                        max_handshake_size: 4,
                                    }
                                }
                                Protocol::Grandpa { chain_index }
                                    if self.chains[chain_index]
                                        .grandpa_protocol_config
                                        .is_some() =>
                                {
                                    collection::InboundTy::Notifications {
                                        max_handshake_size: 4,
                                    }
                                }
                                Protocol::Grandpa { .. } => {
                                    self.inner.reject_inbound(substream_id);
                                    continue;
                                }
                                Protocol::Sync { chain_index }
                                    if self.chains[chain_index].allow_inbound_block_requests =>
                                {
                                    collection::InboundTy::Request {
                                        request_max_size: Some(1024),
                                    }
                                }
                                Protocol::Sync { .. } => {
                                    self.inner.reject_inbound(substream_id);
                                    continue;
                                }

                                // TODO: protocols that are not supported
                                Protocol::LightUnknown { .. }
                                | Protocol::Kad { .. }
                                | Protocol::SyncWarp { .. }
                                | Protocol::State { .. } => {
                                    self.inner.reject_inbound(substream_id);
                                    continue;
                                }

                                Protocol::LightStorage { .. } | Protocol::LightCall { .. } => {
                                    unreachable!()
                                }
                            };

                            self.inner.accept_inbound(substream_id, inbound_type);

                            let _prev_value = self.substreams.insert(
                                substream_id,
                                SubstreamInfo {
                                    connection_id: id,
                                    protocol,
                                },
                            );
                            debug_assert!(_prev_value.is_none());
                        }
                        Err(()) => {
                            self.inner.reject_inbound(substream_id);
                        }
                    }
                    continue;
                }

                collection::Event::InboundNegotiatedCancel { .. } => {
                    // Because we immediately accept or reject substreams, this event can never
                    // happen.
                    unreachable!()
                }

                collection::Event::InboundAcceptedCancel { substream_id } => {
                    // An inbound substream has been aborted after having been accepted.
                    // Since we don't report any event to the API user when a substream is
                    // accepted, we have nothing to do but clean up our state.
                    let _was_in = self.substreams.remove(&substream_id);
                    debug_assert!(_was_in.is_some());
                    continue;
                }

                collection::Event::Response {
                    substream_id,
                    response,
                } => {
                    // Received a response to a request in a request-response protocol.
                    let substream_info = self
                        .substreams
                        .remove(&substream_id)
                        .unwrap_or_else(|| unreachable!());

                    // Decode/verify the response.
                    let response = match substream_info.protocol {
                        Protocol::Identify => todo!(), // TODO: we don't send identify requests yet, so it's fine to leave this unimplemented
                        Protocol::Sync { .. } => RequestResult::Blocks(
                            response
                                .map_err(BlocksRequestError::Request)
                                .and_then(|response| {
                                    protocol::decode_block_response(&response)
                                        .map_err(BlocksRequestError::Decode)
                                }),
                        ),
                        Protocol::LightUnknown { .. } => unreachable!(),
                        Protocol::LightStorage { .. } => RequestResult::StorageProof(
                            response
                                .map_err(StorageProofRequestError::Request)
                                .and_then(|payload| {
                                    match protocol::decode_storage_or_call_proof_response(
                                        protocol::StorageOrCallProof::StorageProof,
                                        &payload,
                                    ) {
                                        Err(err) => Err(StorageProofRequestError::Decode(err)),
                                        Ok(None) => {
                                            Err(StorageProofRequestError::RemoteCouldntAnswer)
                                        }
                                        Ok(Some(_)) => Ok(EncodedMerkleProof(
                                            payload,
                                            protocol::StorageOrCallProof::StorageProof,
                                        )),
                                    }
                                }),
                        ),
                        Protocol::LightCall { .. } => {
                            RequestResult::CallProof(
                                response.map_err(CallProofRequestError::Request).and_then(
                                    |payload| match protocol::decode_storage_or_call_proof_response(
                                        protocol::StorageOrCallProof::CallProof,
                                        &payload,
                                    ) {
                                        Err(err) => Err(CallProofRequestError::Decode(err)),
                                        Ok(None) => Err(CallProofRequestError::RemoteCouldntAnswer),
                                        Ok(Some(_)) => Ok(EncodedMerkleProof(
                                            payload,
                                            protocol::StorageOrCallProof::CallProof,
                                        )),
                                    },
                                ),
                            )
                        }
                        Protocol::Kad { .. } => RequestResult::KademliaFindNode(
                            response
                                .map_err(KademliaFindNodeError::RequestFailed)
                                .and_then(|payload| {
                                    match protocol::decode_find_node_response(&payload) {
                                        Err(err) => Err(KademliaFindNodeError::DecodeError(err)),
                                        Ok(nodes) => Ok(nodes),
                                    }
                                }),
                        ),
                        Protocol::SyncWarp { chain_index } => RequestResult::GrandpaWarpSync(
                            response
                                .map_err(GrandpaWarpSyncRequestError::Request)
                                .and_then(|message| {
                                    if let Err(err) = protocol::decode_grandpa_warp_sync_response(
                                        &message,
                                        self.chains[chain_index].block_number_bytes,
                                    ) {
                                        Err(GrandpaWarpSyncRequestError::Decode(err))
                                    } else {
                                        Ok(EncodedGrandpaWarpSyncResponse {
                                            message,
                                            block_number_bytes: self.chains[chain_index]
                                                .block_number_bytes,
                                        })
                                    }
                                }),
                        ),
                        Protocol::State { .. } => RequestResult::State(
                            response
                                .map_err(StateRequestError::Request)
                                .and_then(|payload| {
                                    if let Err(err) = protocol::decode_state_response(&payload) {
                                        Err(StateRequestError::Decode(err))
                                    } else {
                                        Ok(EncodedStateResponse(payload))
                                    }
                                }),
                        ),

                        // The protocols below aren't request-response protocols.
                        Protocol::Ping
                        | Protocol::BlockAnnounces { .. }
                        | Protocol::Transactions { .. }
                        | Protocol::Grandpa { .. } => unreachable!(),
                    };

                    return Some(Event::RequestResult {
                        substream_id,
                        response,
                    });
                }

                collection::Event::RequestIn {
                    substream_id,
                    request_payload,
                } => {
                    // Received a request on a connection.
                    let substream_info = self
                        .substreams
                        .get(&substream_id)
                        .unwrap_or_else(|| unreachable!());
                    let connection_info = &self.inner[substream_info.connection_id];
                    // Requests can only happen on connections after their handshake phase is
                    // finished, therefore their `PeerId` is known.
                    let peer_id = connection_info
                        .peer_id
                        .as_ref()
                        .unwrap_or_else(|| unreachable!())
                        .clone();

                    match substream_info.protocol {
                        Protocol::Identify => {
                            if request_payload.is_empty() {
                                return Some(Event::IdentifyRequestIn {
                                    peer_id,
                                    substream_id,
                                });
                            } else {
                                // TODO: can this actually be reached? isn't the inner code going to refuse a bad request anyway due to no length prefix?
                                let _ = self.substreams.remove(&substream_id);
                                self.inner.respond_in_request(substream_id, Err(()));
                                return Some(Event::ProtocolError {
                                    peer_id,
                                    error: ProtocolError::BadIdentifyRequest,
                                });
                            }
                        }
                        Protocol::Sync { chain_index } => {
                            match protocol::decode_block_request(
                                self.chains[chain_index].block_number_bytes,
                                &request_payload,
                            ) {
                                Ok(config) => {
                                    return Some(Event::BlocksRequestIn {
                                        peer_id,
                                        chain_id: ChainId(chain_index),
                                        config,
                                        substream_id,
                                    })
                                }
                                Err(error) => {
                                    let _ = self.substreams.remove(&substream_id);
                                    self.inner.respond_in_request(substream_id, Err(()));
                                    return Some(Event::ProtocolError {
                                        peer_id,
                                        error: ProtocolError::BadBlocksRequest(error),
                                    });
                                }
                            }
                        }
                        // Any other protocol is declined when the protocol is negotiated.
                        _ => unreachable!(),
                    }
                }

                collection::Event::RequestInCancel { substream_id } => {
                    let _was_in = self.substreams.remove(&substream_id);
                    debug_assert!(_was_in.is_some());
                    return Some(Event::RequestInCancel { substream_id });
                }

                collection::Event::NotificationsOutResult {
                    substream_id,
                    result,
                } => {
                    // Outgoing notifications substream has finished opening.
                    let substream_info = if result.is_ok() {
                        self.substreams
                            .get(&substream_id)
                            .unwrap_or_else(|| unreachable!())
                            .clone()
                    } else {
                        self.substreams.remove(&substream_id).unwrap()
                    };

                    let connection_id = substream_info.connection_id;
                    let connection_info = &self.inner[connection_id];
                    // Notification substreams can only happen on connections after their
                    // handshake phase is finished, therefore their `PeerId` is known.
                    let peer_id = connection_info
                        .peer_id
                        .as_ref()
                        .unwrap_or_else(|| unreachable!())
                        .clone();

                    let _was_in = self.notification_substreams_by_peer_id.remove(&(
                        substream_info.protocol.try_into().unwrap(),
                        peer_id.clone(),
                        SubstreamDirection::Out,
                        NotificationsSubstreamState::Pending,
                        substream_id,
                    ));
                    debug_assert!(_was_in);

                    // The behaviour is very specific to the protocol.
                    match substream_info.protocol {
                        Protocol::BlockAnnounces { chain_index } => {
                            let result = match &result {
                                Ok(handshake) => {
                                    match protocol::decode_block_announces_handshake(
                                        self.chains[chain_index].block_number_bytes,
                                        &handshake,
                                    ) {
                                        Ok(decoded_handshake)
                                            if *decoded_handshake.genesis_hash
                                                == self.chains[chain_index].genesis_hash =>
                                        {
                                            Ok(decoded_handshake)
                                        }
                                        Ok(decoded_handshake) => {
                                            Err(GossipConnectError::GenesisMismatch {
                                                local_genesis: self.chains[chain_index]
                                                    .genesis_hash,
                                                remote_genesis: *decoded_handshake.genesis_hash,
                                            })
                                        }
                                        Err(err) => Err(GossipConnectError::HandshakeDecode(err)),
                                    }
                                }
                                Err(err) => Err(GossipConnectError::Substream(err.clone())),
                            };

                            match result {
                                Ok(decoded_handshake) => {
                                    let _was_inserted =
                                        self.notification_substreams_by_peer_id.insert((
                                            NotificationsProtocol::BlockAnnounces { chain_index },
                                            peer_id.clone(),
                                            SubstreamDirection::Out,
                                            NotificationsSubstreamState::Open,
                                            substream_id,
                                        ));
                                    debug_assert!(_was_inserted);

                                    if self
                                        .notification_substreams_by_peer_id
                                        .range(
                                            (
                                                NotificationsProtocol::Transactions { chain_index },
                                                peer_id.clone(),
                                                SubstreamDirection::Out,
                                                NotificationsSubstreamState::min_value(),
                                                SubstreamId::min_value(),
                                            )
                                                ..=(
                                                    NotificationsProtocol::Transactions {
                                                        chain_index,
                                                    },
                                                    peer_id.clone(),
                                                    SubstreamDirection::Out,
                                                    NotificationsSubstreamState::max_value(),
                                                    SubstreamId::max_value(),
                                                ),
                                        )
                                        .next()
                                        .is_none()
                                    {
                                        let new_substream_id = self.inner.open_out_notifications(
                                            connection_id,
                                            protocol::encode_protocol_name_string(
                                                protocol::ProtocolName::Transactions {
                                                    genesis_hash: self.chains[chain_index]
                                                        .genesis_hash,
                                                    fork_id: self.chains[chain_index]
                                                        .fork_id
                                                        .as_deref(),
                                                },
                                            ),
                                            Duration::from_secs(10), // TODO: arbitrary
                                            Vec::new(),
                                            128, // TODO: arbitrary
                                        );

                                        self.substreams.insert(
                                            new_substream_id,
                                            SubstreamInfo {
                                                connection_id,
                                                protocol: Protocol::Transactions { chain_index },
                                            },
                                        );

                                        self.notification_substreams_by_peer_id.insert((
                                            NotificationsProtocol::Transactions { chain_index },
                                            peer_id.clone(),
                                            SubstreamDirection::Out,
                                            NotificationsSubstreamState::Pending,
                                            new_substream_id,
                                        ));
                                    }

                                    if self.chains[chain_index].grandpa_protocol_config.is_some()
                                        && self
                                            .notification_substreams_by_peer_id
                                            .range(
                                                (
                                                    NotificationsProtocol::Grandpa { chain_index },
                                                    peer_id.clone(),
                                                    SubstreamDirection::Out,
                                                    NotificationsSubstreamState::min_value(),
                                                    SubstreamId::min_value(),
                                                )
                                                    ..=(
                                                        NotificationsProtocol::Grandpa {
                                                            chain_index,
                                                        },
                                                        peer_id.clone(),
                                                        SubstreamDirection::Out,
                                                        NotificationsSubstreamState::max_value(),
                                                        SubstreamId::max_value(),
                                                    ),
                                            )
                                            .next()
                                            .is_none()
                                    {
                                        let new_substream_id = self.inner.open_out_notifications(
                                            connection_id,
                                            protocol::encode_protocol_name_string(
                                                protocol::ProtocolName::Grandpa {
                                                    genesis_hash: self.chains[chain_index]
                                                        .genesis_hash,
                                                    fork_id: self.chains[chain_index]
                                                        .fork_id
                                                        .as_deref(),
                                                },
                                            ),
                                            Duration::from_secs(10), // TODO: arbitrary
                                            self.chains[chain_index].role.scale_encoding().to_vec(),
                                            1024 * 1024, // TODO: arbitrary
                                        );

                                        self.substreams.insert(
                                            new_substream_id,
                                            SubstreamInfo {
                                                connection_id,
                                                protocol: Protocol::Grandpa { chain_index },
                                            },
                                        );

                                        self.notification_substreams_by_peer_id.insert((
                                            NotificationsProtocol::Grandpa { chain_index },
                                            peer_id.clone(),
                                            SubstreamDirection::Out,
                                            NotificationsSubstreamState::Pending,
                                            new_substream_id,
                                        ));
                                    }

                                    return Some(Event::GossipConnected {
                                        peer_id,
                                        chain_id: ChainId(chain_index),
                                        kind: GossipKind::ConsensusTransactions,
                                        role: decoded_handshake.role,
                                        best_number: decoded_handshake.best_number,
                                        best_hash: *decoded_handshake.best_hash,
                                    });
                                }
                                Err(error) => {
                                    // TODO: lots of unnecessary cloning below
                                    if self
                                        .connections_by_peer_id
                                        .range(
                                            (peer_id.clone(), ConnectionId::min_value())
                                                ..=(peer_id.clone(), ConnectionId::min_value()),
                                        )
                                        .any(|(_, c)| {
                                            let state = self.inner.connection_state(*c);
                                            state.established && !state.shutting_down
                                        })
                                        && self.gossip_desired_peers_by_chain.contains(&(
                                            chain_index,
                                            GossipKind::ConsensusTransactions,
                                            peer_id.clone(),
                                        ))
                                    {
                                        debug_assert!(self
                                            .notification_substreams_by_peer_id
                                            .range(
                                                (
                                                    NotificationsProtocol::BlockAnnounces {
                                                        chain_index
                                                    },
                                                    peer_id.clone(),
                                                    SubstreamDirection::Out,
                                                    NotificationsSubstreamState::Open,
                                                    SubstreamId::min_value(),
                                                )
                                                    ..=(
                                                        NotificationsProtocol::BlockAnnounces {
                                                            chain_index
                                                        },
                                                        peer_id.clone(),
                                                        SubstreamDirection::Out,
                                                        NotificationsSubstreamState::Open,
                                                        SubstreamId::max_value(),
                                                    ),
                                            )
                                            .next()
                                            .is_none());

                                        self.connected_unopened_gossip_desired.insert((
                                            peer_id.clone(),
                                            ChainId(chain_index),
                                            GossipKind::ConsensusTransactions,
                                        ));
                                    }

                                    self.opened_gossip_undesired.remove(&(
                                        ChainId(chain_index),
                                        peer_id.clone(),
                                        GossipKind::ConsensusTransactions,
                                    ));

                                    if let GossipConnectError::HandshakeDecode(_)
                                    | GossipConnectError::GenesisMismatch { .. } = error
                                    {
                                        self.inner.close_out_notifications(substream_id);
                                        self.substreams.remove(&substream_id).unwrap();
                                    }

                                    for substream_id in self
                                        .notification_substreams_by_peer_id
                                        .range(
                                            (
                                                NotificationsProtocol::Transactions { chain_index },
                                                peer_id.clone(),
                                                SubstreamDirection::Out,
                                                NotificationsSubstreamState::min_value(),
                                                SubstreamId::min_value(),
                                            )
                                                ..=(
                                                    NotificationsProtocol::Transactions {
                                                        chain_index,
                                                    },
                                                    peer_id.clone(),
                                                    SubstreamDirection::Out,
                                                    NotificationsSubstreamState::max_value(),
                                                    SubstreamId::max_value(),
                                                ),
                                        )
                                        .map(|(_, _, _, _, s)| *s)
                                        .collect::<Vec<_>>()
                                    {
                                        self.inner.close_out_notifications(substream_id);
                                    }

                                    for substream_id in self
                                        .notification_substreams_by_peer_id
                                        .range(
                                            (
                                                NotificationsProtocol::Grandpa { chain_index },
                                                peer_id.clone(),
                                                SubstreamDirection::Out,
                                                NotificationsSubstreamState::min_value(),
                                                SubstreamId::min_value(),
                                            )
                                                ..=(
                                                    NotificationsProtocol::Grandpa { chain_index },
                                                    peer_id.clone(),
                                                    SubstreamDirection::Out,
                                                    NotificationsSubstreamState::max_value(),
                                                    SubstreamId::max_value(),
                                                ),
                                        )
                                        .map(|(_, _, _, _, s)| *s)
                                        .collect::<Vec<_>>()
                                    {
                                        self.inner.close_out_notifications(substream_id);
                                    }

                                    // TODO: also close the ingoing ba+tx+gp substreams

                                    return Some(Event::GossipOpenFailed {
                                        peer_id,
                                        chain_id: ChainId(chain_index),
                                        kind: GossipKind::ConsensusTransactions,
                                        error,
                                    });
                                }
                            }
                        }

                        Protocol::Transactions { chain_index }
                        | Protocol::Grandpa { chain_index } => {
                            // This can only happen if we have a block announces substream with
                            // that peer, otherwise the substream opening attempt should have
                            // been cancelled.
                            debug_assert!(self
                                .notification_substreams_by_peer_id
                                .range(
                                    (
                                        NotificationsProtocol::BlockAnnounces { chain_index },
                                        peer_id.clone(),
                                        SubstreamDirection::Out,
                                        NotificationsSubstreamState::Open,
                                        SubstreamId::min_value()
                                    )
                                        ..=(
                                            NotificationsProtocol::BlockAnnounces { chain_index },
                                            peer_id.clone(),
                                            SubstreamDirection::Out,
                                            NotificationsSubstreamState::Open,
                                            SubstreamId::max_value()
                                        )
                                )
                                .next()
                                .is_some());

                            // If the substream failed to open, we simply try again.
                            // Trying agains means that we might be hammering the remote with
                            // substream requests, however as of the writing of this text this is
                            // necessary in order to bypass an issue in Substrate.
                            if result.is_err()
                                && !self.inner.connection_state(connection_id).shutting_down
                            {
                                let new_substream_id = self.inner.open_out_notifications(
                                    connection_id,
                                    protocol::encode_protocol_name_string(
                                        match substream_info.protocol {
                                            Protocol::Transactions { .. } => {
                                                protocol::ProtocolName::Transactions {
                                                    genesis_hash: self.chains[chain_index]
                                                        .genesis_hash,
                                                    fork_id: self.chains[chain_index]
                                                        .fork_id
                                                        .as_deref(),
                                                }
                                            }
                                            Protocol::Grandpa { .. } => {
                                                protocol::ProtocolName::Grandpa {
                                                    genesis_hash: self.chains[chain_index]
                                                        .genesis_hash,
                                                    fork_id: self.chains[chain_index]
                                                        .fork_id
                                                        .as_deref(),
                                                }
                                            }
                                            _ => unreachable!(),
                                        },
                                    ),
                                    Duration::from_secs(10), // TODO: arbitrary
                                    match substream_info.protocol {
                                        Protocol::Transactions { .. } => Vec::new(),
                                        Protocol::Grandpa { .. } => {
                                            self.chains[chain_index].role.scale_encoding().to_vec()
                                        }
                                        _ => unreachable!(),
                                    },
                                    1024 * 1024, // TODO: arbitrary
                                );

                                let _was_inserted =
                                    self.notification_substreams_by_peer_id.insert((
                                        NotificationsProtocol::try_from(substream_info.protocol)
                                            .unwrap(),
                                        peer_id.clone(),
                                        SubstreamDirection::Out,
                                        NotificationsSubstreamState::Pending,
                                        new_substream_id,
                                    ));
                                debug_assert!(_was_inserted);

                                let _prev_value = self.substreams.insert(
                                    new_substream_id,
                                    SubstreamInfo {
                                        connection_id,
                                        protocol: substream_info.protocol.clone(),
                                    },
                                );
                                debug_assert!(_prev_value.is_none());

                                continue;
                            }

                            let _was_inserted = self.notification_substreams_by_peer_id.insert((
                                NotificationsProtocol::try_from(substream_info.protocol).unwrap(),
                                peer_id.clone(),
                                SubstreamDirection::Out,
                                NotificationsSubstreamState::Open,
                                substream_id,
                            ));
                            debug_assert!(_was_inserted);

                            // In case of Grandpa, we immediately send a neighbor packet with
                            // the current local state.
                            if matches!(substream_info.protocol, Protocol::Grandpa { .. }) {
                                let grandpa_state = &self.chains[chain_index]
                                    .grandpa_protocol_config
                                    .as_ref()
                                    .unwrap();
                                let packet = protocol::GrandpaNotificationRef::Neighbor(
                                    protocol::NeighborPacket {
                                        round_number: grandpa_state.round_number,
                                        set_id: grandpa_state.set_id,
                                        commit_finalized_height: grandpa_state
                                            .commit_finalized_height,
                                    },
                                )
                                .scale_encoding(self.chains[chain_index].block_number_bytes)
                                .fold(Vec::new(), |mut a, b| {
                                    a.extend_from_slice(b.as_ref());
                                    a
                                });
                                match self.inner.queue_notification(substream_id, packet) {
                                    Ok(()) => {}
                                    Err(collection::QueueNotificationError::QueueFull) => {
                                        unreachable!()
                                    }
                                }
                            }
                        }

                        // The other protocols aren't notification protocols.
                        Protocol::Identify
                        | Protocol::Ping
                        | Protocol::Sync { .. }
                        | Protocol::LightUnknown { .. }
                        | Protocol::LightStorage { .. }
                        | Protocol::LightCall { .. }
                        | Protocol::Kad { .. }
                        | Protocol::SyncWarp { .. }
                        | Protocol::State { .. } => unreachable!(),
                    }
                }

                collection::Event::NotificationsOutCloseDemanded { substream_id }
                | collection::Event::NotificationsOutReset { substream_id } => {
                    // Outgoing notifications substream has been closed or must be closed.

                    // If the request demands the closing, we immediately comply.
                    if matches!(
                        inner_event,
                        collection::Event::NotificationsOutCloseDemanded { .. }
                    ) {
                        self.inner.close_out_notifications(substream_id);
                    }

                    let substream_info = self
                        .substreams
                        .remove(&substream_id)
                        .unwrap_or_else(|| unreachable!());
                    let connection_id = substream_info.connection_id;
                    let connection_info = &self.inner[connection_id];
                    // Notification substreams can only happen on connections after their
                    // handshake phase is finished, therefore their `PeerId` is known.
                    let peer_id = connection_info
                        .peer_id
                        .as_ref()
                        .unwrap_or_else(|| unreachable!())
                        .clone();

                    // Clean up the local state.
                    let _was_in = self.notification_substreams_by_peer_id.remove(&(
                        NotificationsProtocol::try_from(substream_info.protocol).unwrap(),
                        peer_id.clone(), // TODO: cloning overhead :-/
                        SubstreamDirection::Out,
                        NotificationsSubstreamState::Open,
                        substream_id,
                    ));
                    debug_assert!(_was_in);

                    // Some substreams are tied to the state of the block announces substream.
                    match substream_info.protocol {
                        Protocol::BlockAnnounces { chain_index } => {
                            self.opened_gossip_undesired.remove(&(
                                ChainId(chain_index),
                                peer_id.clone(),
                                GossipKind::ConsensusTransactions,
                            ));

                            // Insert back in `connected_unopened_gossip_desired` if relevant.
                            if self.gossip_desired_peers_by_chain.contains(&(
                                chain_index,
                                GossipKind::ConsensusTransactions,
                                peer_id.clone(),
                            )) && !self
                                .connections_by_peer_id
                                .range(
                                    (peer_id.clone(), ConnectionId::min_value())
                                        ..=(peer_id.clone(), ConnectionId::max_value()),
                                )
                                .any(|(_, connection_id)| {
                                    let state = self.inner.connection_state(*connection_id);
                                    !state.shutting_down
                                })
                            {
                                debug_assert!(self
                                    .notification_substreams_by_peer_id
                                    .range(
                                        (
                                            NotificationsProtocol::BlockAnnounces { chain_index },
                                            peer_id.clone(),
                                            SubstreamDirection::Out,
                                            NotificationsSubstreamState::Open,
                                            SubstreamId::min_value(),
                                        )
                                            ..=(
                                                NotificationsProtocol::BlockAnnounces {
                                                    chain_index
                                                },
                                                peer_id.clone(),
                                                SubstreamDirection::Out,
                                                NotificationsSubstreamState::Open,
                                                SubstreamId::max_value(),
                                            ),
                                    )
                                    .next()
                                    .is_none());

                                let _was_inserted =
                                    self.connected_unopened_gossip_desired.insert((
                                        peer_id.clone(),
                                        ChainId(chain_index),
                                        GossipKind::ConsensusTransactions,
                                    ));
                                debug_assert!(_was_inserted);
                            }

                            for proto in [
                                NotificationsProtocol::Transactions { chain_index },
                                NotificationsProtocol::Grandpa { chain_index },
                            ] {
                                for (substream_state, substream_id) in self
                                    .notification_substreams_by_peer_id
                                    .range(
                                        (
                                            proto,
                                            peer_id.clone(),
                                            SubstreamDirection::Out,
                                            NotificationsSubstreamState::min_value(),
                                            SubstreamId::min_value(),
                                        )
                                            ..=(
                                                proto,
                                                peer_id.clone(),
                                                SubstreamDirection::Out,
                                                NotificationsSubstreamState::max_value(),
                                                SubstreamId::max_value(),
                                            ),
                                    )
                                    .map(|(_, _, _, state, substream_id)| (*state, *substream_id))
                                    .collect::<Vec<_>>()
                                {
                                    self.inner.close_out_notifications(substream_id);
                                    self.substreams.remove(&substream_id);
                                    self.notification_substreams_by_peer_id.remove(&(
                                        proto,
                                        peer_id.clone(),
                                        SubstreamDirection::Out,
                                        substream_state,
                                        substream_id,
                                    ));
                                }
                            }

                            // TODO: also close inbound substreams?

                            return Some(Event::GossipDisconnected {
                                peer_id: peer_id.clone(),
                                chain_id: ChainId(chain_index),
                                kind: GossipKind::ConsensusTransactions,
                            });
                        }
                        // The transactions and Grandpa protocols are tied to the block announces
                        // substream. If there is a block announce substream with the peer, we try
                        // to reopen these two substreams.
                        Protocol::Transactions { chain_index } => {
                            let new_substream_id = self.inner.open_out_notifications(
                                connection_id,
                                protocol::encode_protocol_name_string(
                                    protocol::ProtocolName::Transactions {
                                        genesis_hash: self.chains[chain_index].genesis_hash,
                                        fork_id: self.chains[chain_index].fork_id.as_deref(),
                                    },
                                ),
                                Duration::from_secs(10), // TODO: arbitrary
                                Vec::new(),
                                1024 * 1024, // TODO: arbitrary
                            );
                            self.substreams.insert(
                                new_substream_id,
                                SubstreamInfo {
                                    connection_id,
                                    protocol: Protocol::Transactions { chain_index },
                                },
                            );
                            self.notification_substreams_by_peer_id.insert((
                                NotificationsProtocol::Transactions { chain_index },
                                peer_id.clone(),
                                SubstreamDirection::Out,
                                NotificationsSubstreamState::Pending,
                                new_substream_id,
                            ));
                        }
                        Protocol::Grandpa { chain_index } => {
                            let new_substream_id = self.inner.open_out_notifications(
                                connection_id,
                                protocol::encode_protocol_name_string(
                                    protocol::ProtocolName::Grandpa {
                                        genesis_hash: self.chains[chain_index].genesis_hash,
                                        fork_id: self.chains[chain_index].fork_id.as_deref(),
                                    },
                                ),
                                Duration::from_secs(10), // TODO: arbitrary
                                self.chains[chain_index].role.scale_encoding().to_vec(),
                                1024 * 1024, // TODO: arbitrary
                            );
                            self.substreams.insert(
                                new_substream_id,
                                SubstreamInfo {
                                    connection_id,
                                    protocol: Protocol::Grandpa { chain_index },
                                },
                            );
                            self.notification_substreams_by_peer_id.insert((
                                NotificationsProtocol::Grandpa { chain_index },
                                peer_id.clone(),
                                SubstreamDirection::Out,
                                NotificationsSubstreamState::Pending,
                                new_substream_id,
                            ));
                        }
                        _ => unreachable!(),
                    }
                }

                collection::Event::NotificationsInOpen { substream_id, .. } => {
                    // Remote would like to open a notifications substream with us.

                    // There exists three possible ways to handle this event:
                    //
                    // - Accept the demand immediately. This happens if the API user has opened
                    //   a gossip substream in the past or is currently trying to open a gossip
                    //   substream with this peer.
                    // - Refuse the demand immediately. This happens if there already exists a
                    //   pending inbound notifications substream. Opening multiple notification
                    //   substreams of the same protocol is a protocol violation. This also happens
                    //   for transactions and grandpa substreams if no block announce substream is
                    //   open.
                    // - Generate an event to ask the API user whether to accept the demand. This
                    //   happens specifically for block announce substreams.

                    let substream_info = self
                        .substreams
                        .get(&substream_id)
                        .unwrap_or_else(|| unreachable!());
                    let connection_info = &self.inner[substream_info.connection_id];
                    // Notification substreams can only happen on connections after their
                    // handshake phase is finished, therefore their `PeerId` is known.
                    let peer_id = connection_info
                        .peer_id
                        .as_ref()
                        .unwrap_or_else(|| unreachable!());

                    // Check whether a substream with the same protocol already exists with that
                    // peer, and if so deny the request.
                    if self
                        .notification_substreams_by_peer_id
                        .range(
                            (
                                substream_info.protocol.try_into().unwrap(),
                                peer_id.clone(),
                                SubstreamDirection::In,
                                NotificationsSubstreamState::min_value(),
                                SubstreamId::min_value(),
                            )
                                ..=(
                                    substream_info.protocol.try_into().unwrap(),
                                    peer_id.clone(),
                                    SubstreamDirection::In,
                                    NotificationsSubstreamState::max_value(),
                                    SubstreamId::max_value(),
                                ),
                        )
                        .next()
                        .is_some()
                    {
                        self.inner.reject_in_notifications(substream_id);
                        self.substreams.remove(&substream_id);
                        continue;
                    }

                    // Find the `chain_index`.
                    let (Protocol::BlockAnnounces { chain_index }
                    | Protocol::Transactions { chain_index }
                    | Protocol::Grandpa { chain_index }) = substream_info.protocol
                    else {
                        // Any other protocol isn't a notifications protocol.
                        unreachable!()
                    };

                    // If an outgoing block announces notifications protocol (either pending or
                    // fully open) exists, accept the substream immediately.
                    if self
                        .notification_substreams_by_peer_id
                        .range(
                            (
                                NotificationsProtocol::BlockAnnounces { chain_index },
                                peer_id.clone(),
                                SubstreamDirection::Out,
                                NotificationsSubstreamState::min_value(),
                                SubstreamId::min_value(),
                            )
                                ..=(
                                    NotificationsProtocol::BlockAnnounces { chain_index },
                                    peer_id.clone(),
                                    SubstreamDirection::Out,
                                    NotificationsSubstreamState::max_value(),
                                    SubstreamId::max_value(),
                                ),
                        )
                        .next()
                        .is_some()
                    {
                        self.notification_substreams_by_peer_id.insert((
                            substream_info.protocol.try_into().unwrap(),
                            peer_id.clone(),
                            SubstreamDirection::In,
                            NotificationsSubstreamState::Open,
                            substream_id,
                        ));
                        let handshake = match substream_info.protocol {
                            Protocol::BlockAnnounces { .. } => {
                                protocol::encode_block_announces_handshake(
                                    protocol::BlockAnnouncesHandshakeRef {
                                        best_hash: &self.chains[chain_index].best_hash,
                                        best_number: self.chains[chain_index].best_number,
                                        role: self.chains[chain_index].role,
                                        genesis_hash: &self.chains[chain_index].genesis_hash,
                                    },
                                    self.chains[chain_index].block_number_bytes,
                                )
                                .fold(Vec::new(), |mut a, b| {
                                    a.extend_from_slice(b.as_ref());
                                    a
                                })
                            }
                            Protocol::Grandpa { .. } => {
                                self.chains[chain_index].role.scale_encoding().to_vec()
                            }
                            Protocol::Transactions { .. } => Vec::new(),
                            _ => unreachable!(),
                        };
                        self.inner.accept_in_notifications(
                            substream_id,
                            handshake,
                            1024 * 1024, // TODO: ?!
                        );
                        continue;
                    }

                    // It is forbidden to cold-open a substream other than the block announces
                    // substream.
                    if !matches!(substream_info.protocol, Protocol::BlockAnnounces { .. }) {
                        self.inner.reject_in_notifications(substream_id);
                        self.substreams.remove(&substream_id);
                        continue;
                    }

                    // Update the local state and return the event.
                    self.notification_substreams_by_peer_id.insert((
                        NotificationsProtocol::BlockAnnounces { chain_index },
                        peer_id.clone(),
                        SubstreamDirection::In,
                        NotificationsSubstreamState::Pending,
                        substream_id,
                    ));
                    return Some(Event::GossipInDesired {
                        peer_id: peer_id.clone(),
                        chain_id: ChainId(chain_index),
                        kind: GossipKind::ConsensusTransactions,
                    });
                }

                collection::Event::NotificationsInOpenCancel { substream_id } => {
                    // Remote has cancelled a pending `NotificationsInOpen`.

                    let substream_info = self
                        .substreams
                        .get(&substream_id)
                        .unwrap_or_else(|| unreachable!());
                    let connection_info = &self.inner[substream_info.connection_id];
                    // Notification substreams can only happen on connections after their
                    // handshake phase is finished, therefore their `PeerId` is known.
                    let peer_id = connection_info
                        .peer_id
                        .as_ref()
                        .unwrap_or_else(|| unreachable!());

                    // All incoming notification substreams are immediately accepted/rejected
                    // except for block announce substreams. Therefore, this event can only happen
                    // for block announce substreams.
                    let Protocol::BlockAnnounces { chain_index } = substream_info.protocol else {
                        unreachable!()
                    };

                    // Clean up the local state.
                    let _was_in = self.notification_substreams_by_peer_id.remove(&(
                        NotificationsProtocol::BlockAnnounces { chain_index },
                        peer_id.clone(), // TODO: cloning overhead :-/
                        SubstreamDirection::Out,
                        NotificationsSubstreamState::Open,
                        substream_id,
                    ));
                    debug_assert!(_was_in);

                    // Notify API user.
                    return Some(Event::GossipInDesiredCancel {
                        peer_id: peer_id.clone(),
                        chain_id: ChainId(chain_index),
                        kind: GossipKind::ConsensusTransactions,
                    });
                }

                collection::Event::NotificationsIn {
                    substream_id,
                    notification,
                } => {
                    // Received a notification from a remote.
                    let substream_info = self
                        .substreams
                        .get(&substream_id)
                        .unwrap_or_else(|| unreachable!());
                    let chain_index = match substream_info.protocol {
                        Protocol::BlockAnnounces { chain_index } => chain_index,
                        Protocol::Transactions { chain_index } => chain_index,
                        Protocol::Grandpa { chain_index } => chain_index,
                        // Other protocols are not notification protocols.
                        Protocol::Identify
                        | Protocol::Ping
                        | Protocol::Sync { .. }
                        | Protocol::LightUnknown { .. }
                        | Protocol::LightStorage { .. }
                        | Protocol::LightCall { .. }
                        | Protocol::Kad { .. }
                        | Protocol::SyncWarp { .. }
                        | Protocol::State { .. } => unreachable!(),
                    };
                    let connection_info = &self.inner[substream_info.connection_id];
                    // Notification substreams can only happen on connections after their
                    // handshake phase is finished, therefore their `PeerId` is known.
                    let peer_id = connection_info
                        .peer_id
                        .as_ref()
                        .unwrap_or_else(|| unreachable!());

                    // Check whether there is an open outgoing block announces substream, as this
                    // means that we are "gossip-connected". If not, then the notification is
                    // silently discarded.
                    // TODO: cloning of the peer_id
                    if self
                        .notification_substreams_by_peer_id
                        .range(
                            (
                                NotificationsProtocol::BlockAnnounces { chain_index },
                                peer_id.clone(),
                                SubstreamDirection::Out,
                                NotificationsSubstreamState::Open,
                                collection::SubstreamId::min_value(),
                            )
                                ..=(
                                    NotificationsProtocol::BlockAnnounces { chain_index },
                                    peer_id.clone(),
                                    SubstreamDirection::Out,
                                    NotificationsSubstreamState::Open,
                                    collection::SubstreamId::max_value(),
                                ),
                        )
                        .next()
                        .is_none()
                    {
                        continue;
                    }

                    // Decode the notification and return an event.
                    match substream_info.protocol {
                        Protocol::BlockAnnounces { .. } => {
                            if let Err(err) = protocol::decode_block_announce(
                                &notification,
                                self.chains[chain_index].block_number_bytes,
                            ) {
                                return Some(Event::ProtocolError {
                                    error: ProtocolError::BadBlockAnnounce(err),
                                    peer_id: peer_id.clone(),
                                });
                            }

                            return Some(Event::BlockAnnounce {
                                chain_id: ChainId(chain_index),
                                peer_id: peer_id.clone(),
                                announce: EncodedBlockAnnounce {
                                    message: notification,
                                    block_number_bytes: self.chains[chain_index].block_number_bytes,
                                },
                            });
                        }
                        Protocol::Transactions { .. } => {
                            // TODO: not implemented
                        }
                        Protocol::Grandpa { .. } => {
                            let decoded_notif = match protocol::decode_grandpa_notification(
                                &notification,
                                self.chains[chain_index].block_number_bytes,
                            ) {
                                Ok(n) => n,
                                Err(err) => {
                                    return Some(Event::ProtocolError {
                                        error: ProtocolError::BadGrandpaNotification(err),
                                        peer_id: peer_id.clone(),
                                    })
                                }
                            };

                            match decoded_notif {
                                protocol::GrandpaNotificationRef::Commit(_) => {
                                    return Some(Event::GrandpaCommitMessage {
                                        chain_id: ChainId(chain_index),
                                        peer_id: peer_id.clone(),
                                        message: EncodedGrandpaCommitMessage {
                                            message: notification,
                                            block_number_bytes: self.chains[chain_index]
                                                .block_number_bytes,
                                        },
                                    })
                                }
                                protocol::GrandpaNotificationRef::Neighbor(n) => {
                                    return Some(Event::GrandpaNeighborPacket {
                                        chain_id: ChainId(chain_index),
                                        peer_id: peer_id.clone(),
                                        state: GrandpaState {
                                            round_number: n.round_number,
                                            set_id: n.set_id,
                                            commit_finalized_height: n.commit_finalized_height,
                                        },
                                    })
                                }
                                _ => {
                                    // Any other type of message is currently ignored. Support
                                    // for them could be added in the future.
                                }
                            }
                        }

                        // Other protocols are not notification protocols.
                        Protocol::Identify
                        | Protocol::Ping
                        | Protocol::Sync { .. }
                        | Protocol::LightUnknown { .. }
                        | Protocol::LightStorage { .. }
                        | Protocol::LightCall { .. }
                        | Protocol::Kad { .. }
                        | Protocol::SyncWarp { .. }
                        | Protocol::State { .. } => unreachable!(),
                    }
                }

                collection::Event::NotificationsInClose { substream_id, .. } => {
                    // An incoming notifications substream has been closed.
                    // Nothing to do except clean up the local state.
                    let _was_in = self.substreams.remove(&substream_id);
                    debug_assert!(_was_in.is_some());
                }

                collection::Event::PingOutSuccess { .. } => {
                    // We ignore ping events.
                    // TODO: report to end user or something
                }
            }
        }
    }

    /// Sends a blocks request to the given peer.
    ///
    /// The code in this module does not verify the response in any way. The blocks might be
    /// completely different from the ones requested, or might be missing some information. In
    /// other words, the response is completely untrusted.
    ///
    /// This function might generate a message destined a connection. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    // TODO: more docs
    pub fn start_blocks_request(
        &mut self,
        target: &PeerId,
        chain_id: ChainId,
        config: protocol::BlocksRequestConfig,
        timeout: Duration,
    ) -> Result<SubstreamId, StartRequestError> {
        let request_data =
            protocol::build_block_request(self.chains[chain_id.0].block_number_bytes, &config)
                .fold(Vec::new(), |mut a, b| {
                    a.extend_from_slice(b.as_ref());
                    a
                });

        self.start_request(
            target,
            request_data,
            Protocol::Sync {
                chain_index: chain_id.0,
            },
            timeout,
        )
    }

    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    // TODO: docs
    pub fn start_grandpa_warp_sync_request(
        &mut self,
        target: &PeerId,
        chain_id: ChainId,
        begin_hash: [u8; 32],
        timeout: Duration,
    ) -> Result<SubstreamId, StartRequestError> {
        let request_data = begin_hash.to_vec();

        self.start_request(
            target,
            request_data,
            Protocol::SyncWarp {
                chain_index: chain_id.0,
            },
            timeout,
        )
    }

    /// Sends a state request to a peer.
    ///
    /// A state request makes it possible to download the storage of the chain at a given block.
    /// The response is not unverified by this function. In other words, the peer is free to send
    /// back erroneous data. It is the responsibility of the API user to verify the storage by
    /// calculating the state trie root hash and comparing it with the value stored in the
    /// block's header.
    ///
    /// Because response have a size limit, it is unlikely that a single request will return the
    /// entire storage of the chain at once. Instead, call this function multiple times, each call
    /// passing a `start_key` that follows the last key of the previous response.
    ///
    /// This function might generate a message destined a connection. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    pub fn start_state_request(
        &mut self,
        target: &PeerId,
        chain_id: ChainId,
        block_hash: &[u8; 32],
        start_key: protocol::StateRequestStart,
        timeout: Duration,
    ) -> Result<SubstreamId, StartRequestError> {
        let request_data = protocol::build_state_request(protocol::StateRequest {
            block_hash,
            start_key,
        })
        .fold(Vec::new(), |mut a, b| {
            a.extend_from_slice(b.as_ref());
            a
        });

        self.start_request(
            target,
            request_data,
            Protocol::State {
                chain_index: chain_id.0,
            },
            timeout,
        )
    }

    /// Sends a storage request to the given peer.
    ///
    /// This function might generate a message destined a connection. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    // TODO: more docs
    pub fn start_storage_proof_request(
        &mut self,
        target: &PeerId,
        chain_id: ChainId,
        config: protocol::StorageProofRequestConfig<impl Iterator<Item = impl AsRef<[u8]> + Clone>>,
        timeout: Duration,
    ) -> Result<SubstreamId, StartRequestMaybeTooLargeError> {
        let request_data =
            protocol::build_storage_proof_request(config).fold(Vec::new(), |mut a, b| {
                a.extend_from_slice(b.as_ref());
                a
            });

        // The request data can possibly by higher than the protocol limit, especially due to the
        // call data.
        // TODO: check limit

        Ok(self.start_request(
            target,
            request_data,
            Protocol::LightStorage {
                chain_index: chain_id.0,
            },
            timeout,
        )?)
    }

    /// Sends a call proof request to the given peer.
    ///
    /// This request is similar to [`ChainNetwork::start_storage_proof_request`]. Instead of
    /// requesting specific keys, we request the list of all the keys that are accessed for a
    /// specific runtime call.
    ///
    /// There exists no guarantee that the proof is complete (i.e. that it contains all the
    /// necessary entries), as it is impossible to know this from just the proof itself. As such,
    /// this method is just an optimization. When performing the actual call, regular storage proof
    /// requests should be performed if the key is not present in the call proof response.
    ///
    /// This function might generate a message destined a connection. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    pub fn start_call_proof_request(
        &mut self,
        target: &PeerId,
        chain_id: ChainId,
        config: protocol::CallProofRequestConfig<'_, impl Iterator<Item = impl AsRef<[u8]>>>,
        timeout: Duration,
    ) -> Result<SubstreamId, StartRequestMaybeTooLargeError> {
        let request_data =
            protocol::build_call_proof_request(config).fold(Vec::new(), |mut a, b| {
                a.extend_from_slice(b.as_ref());
                a
            });

        // The request data can possibly by higher than the protocol limit, especially due to the
        // call data.
        // TODO: check limit

        Ok(self.start_request(
            target,
            request_data,
            Protocol::LightCall {
                chain_index: chain_id.0,
            },
            timeout,
        )?)
    }

    /// Sends a Kademlia find node request to the given peer.
    ///
    /// This function might generate a message destined a connection. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    pub fn start_kademlia_find_node_request(
        &mut self,
        target: &PeerId,
        chain_id: ChainId,
        peer_id_to_find: &PeerId,
        timeout: Duration,
    ) -> Result<SubstreamId, StartRequestError> {
        let request_data = protocol::build_find_node_request(peer_id_to_find.as_bytes());

        // The request data can possibly by higher than the protocol limit, especially due to the
        // call data.
        // TODO: check limit

        Ok(self.start_request(
            target,
            request_data,
            Protocol::Kad {
                chain_index: chain_id.0,
            },
            timeout,
        )?)
    }

    /// Underlying implementation of all the functions that start requests.
    fn start_request(
        &mut self,
        target: &PeerId,
        request_data: Vec<u8>,
        protocol: Protocol,
        timeout: Duration,
    ) -> Result<SubstreamId, StartRequestError> {
        // TODO: cloning of `PeerId` overhead
        // TODO: this is O(n) but is it really a problem? you're only supposed to have max 1 or 2 connections per PeerId
        let connection_id = self
            .connections_by_peer_id
            .range(
                (target.clone(), collection::ConnectionId::min_value())
                    ..=(target.clone(), collection::ConnectionId::max_value()),
            )
            .map(|(_, connection_id)| *connection_id)
            .find(|connection_id| {
                let state = self.inner.connection_state(*connection_id);
                state.established && !state.shutting_down
            })
            .ok_or(StartRequestError::NoConnection)?;

        let protocol_name = {
            let protocol_name = match protocol {
                Protocol::Identify => protocol::ProtocolName::Identify,
                Protocol::Ping => protocol::ProtocolName::Ping,
                Protocol::BlockAnnounces { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::BlockAnnounces {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::Transactions { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::Transactions {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::Grandpa { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::Grandpa {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::Sync { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::Sync {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::LightUnknown { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::Light {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::LightStorage { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::Light {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::LightCall { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::Light {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::Kad { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::Kad {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::SyncWarp { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::SyncWarp {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
                Protocol::State { chain_index } => {
                    let chain_info = &self.chains[chain_index];
                    protocol::ProtocolName::State {
                        genesis_hash: chain_info.genesis_hash,
                        fork_id: chain_info.fork_id.as_deref(),
                    }
                }
            };

            protocol::encode_protocol_name_string(protocol_name)
        };

        let substream_id = self.inner.start_request(
            connection_id,
            protocol_name,
            Some(request_data),
            timeout,
            16 * 1024 * 1024,
        );

        let _prev_value = self.substreams.insert(
            substream_id,
            SubstreamInfo {
                connection_id,
                protocol,
            },
        );
        debug_assert!(_prev_value.is_none());

        Ok(substream_id)
    }

    /// Responds to an identify request. Call this function in response to
    /// a [`Event::IdentifyRequestIn`].
    ///
    /// Only the `agent_version` needs to be specified. The other fields are automatically
    /// filled by the [`ChainNetwork`].
    ///
    /// This function might generate a message destined a connection. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`SubstreamId`] is invalid or doesn't correspond to a blocks request or
    /// if the request has been cancelled with a [`Event::RequestInCancel`].
    ///
    pub fn respond_identify(&mut self, substream_id: SubstreamId, agent_version: &str) {
        let substream_info = self.substreams.remove(&substream_id).unwrap();
        assert!(matches!(substream_info.protocol, Protocol::Identify { .. }));

        let response = {
            let observed_addr = &self.inner[substream_info.connection_id].address;

            // TODO: all protocols
            let supported_protocols = [protocol::ProtocolName::Ping].into_iter();

            let supported_protocols_names = supported_protocols
                .map(|proto| protocol::encode_protocol_name_string(proto))
                .collect::<Vec<_>>();

            protocol::build_identify_response(protocol::IdentifyResponse {
                protocol_version: "/substrate/1.0", // TODO: same value as in Substrate, see also https://github.com/paritytech/substrate/issues/14331
                agent_version,
                ed25519_public_key: *self.noise_key.libp2p_public_ed25519_key(),
                listen_addrs: iter::empty(), // TODO:
                observed_addr,
                protocols: supported_protocols_names.iter().map(|p| &p[..]),
            })
            .fold(Vec::new(), |mut a, b| {
                a.extend_from_slice(b.as_ref());
                a
            })
        };

        self.inner.respond_in_request(substream_id, Ok(response));
    }

    /// Responds to a blocks request. Call this function in response to
    /// a [`Event::BlocksRequestIn`].
    ///
    /// Pass `None` in order to deny the request. Do this if blocks aren't available locally.
    ///
    /// This function might generate a message destined a connection. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`SubstreamId`] is invalid or doesn't correspond to a blocks request or
    /// if the request has been cancelled with a [`Event::RequestInCancel`].
    ///
    // TOOD: more zero-cost parameter
    pub fn respond_blocks(
        &mut self,
        substream_id: SubstreamId,
        response: Option<Vec<protocol::BlockData>>,
    ) {
        let substream_info = self.substreams.remove(&substream_id).unwrap();
        assert!(matches!(substream_info.protocol, Protocol::Sync { .. }));

        let response = if let Some(response) = response {
            Ok(
                protocol::build_block_response(response).fold(Vec::new(), |mut a, b| {
                    a.extend_from_slice(b.as_ref());
                    a
                }),
            )
        } else {
            Err(())
        };

        self.inner.respond_in_request(substream_id, response);
    }

    /// Returns the list of all peers for a [`Event::GossipConnected`] event of the given kind has
    /// been emitted.
    /// It is possible to send gossip notifications to these peers.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    pub fn gossip_connected_peers(
        &'_ self,
        chain_id: ChainId,
        kind: GossipKind,
    ) -> impl Iterator<Item = &'_ PeerId> + '_ {
        assert!(self.chains.contains(chain_id.0));
        let GossipKind::ConsensusTransactions = kind;
        // TODO: O(n) ; optimize this by using range(), but that's a bit complicated
        self.notification_substreams_by_peer_id
            .iter()
            .filter(move |(p, _, d, s, _)| {
                *p == NotificationsProtocol::BlockAnnounces {
                    chain_index: chain_id.0,
                } && *d == SubstreamDirection::Out
                    && *s == NotificationsSubstreamState::Open
            })
            .map(|(_, peer_id, _, _, _)| peer_id)
    }

    /// Open a gossiping substream with the given peer on the given chain.
    ///
    /// Either a [`Event::GossipConnected`] or [`Event::GossipOpenFailed`] is guaranteed to later
    /// be generated, unless [`ChainNetwork::gossip_close`] is called in the meanwhile.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    // TODO: proper error
    pub fn gossip_open(
        &mut self,
        chain_id: ChainId,
        target: &PeerId,
        kind: GossipKind,
    ) -> Result<(), ()> {
        let GossipKind::ConsensusTransactions = kind;

        let chain_info = &self.chains[chain_id.0];

        // It is forbidden to open more than one gossip notifications substream with any given
        // peer.
        if self
            .notification_substreams_by_peer_id
            .range(
                (
                    NotificationsProtocol::BlockAnnounces {
                        chain_index: chain_id.0,
                    },
                    target.clone(),
                    SubstreamDirection::Out,
                    NotificationsSubstreamState::min_value(),
                    SubstreamId::min_value(),
                )
                    ..=(
                        NotificationsProtocol::BlockAnnounces {
                            chain_index: chain_id.0,
                        },
                        target.clone(),
                        SubstreamDirection::Out,
                        NotificationsSubstreamState::max_value(),
                        SubstreamId::max_value(),
                    ),
            )
            .next()
            .is_some()
        {
            return Err(());
        }

        let protocol_name =
            protocol::encode_protocol_name_string(protocol::ProtocolName::BlockAnnounces {
                genesis_hash: chain_info.genesis_hash,
                fork_id: chain_info.fork_id.as_deref(),
            });

        // TODO: cloning of `PeerId` overhead
        // TODO: this is O(n) but is it really a problem? you're only supposed to have max 1 or 2 connections per PeerId
        let connection_id = self
            .connections_by_peer_id
            .range(
                (target.clone(), collection::ConnectionId::min_value())
                    ..=(target.clone(), collection::ConnectionId::max_value()),
            )
            .map(|(_, connection_id)| *connection_id)
            .find(|connection_id| {
                let state = self.inner.connection_state(*connection_id);
                state.established && !state.shutting_down
            })
            .ok_or(())?;

        let handshake = protocol::encode_block_announces_handshake(
            protocol::BlockAnnouncesHandshakeRef {
                best_hash: &chain_info.best_hash,
                best_number: chain_info.best_number,
                role: chain_info.role,
                genesis_hash: &chain_info.genesis_hash,
            },
            self.chains[chain_id.0].block_number_bytes,
        )
        .fold(Vec::new(), |mut a, b| {
            a.extend_from_slice(b.as_ref());
            a
        });

        let substream_id = self.inner.open_out_notifications(
            connection_id,
            protocol_name,
            Duration::from_secs(10), // TODO: arbitrary
            handshake,
            1024 * 1024, // TODO: arbitrary
        );

        let _prev_value = self.substreams.insert(
            substream_id,
            SubstreamInfo {
                connection_id,
                protocol: Protocol::BlockAnnounces {
                    chain_index: chain_id.0,
                },
            },
        );
        debug_assert!(_prev_value.is_none());

        let _was_inserted = self.notification_substreams_by_peer_id.insert((
            NotificationsProtocol::BlockAnnounces {
                chain_index: chain_id.0,
            },
            target.clone(),
            SubstreamDirection::Out,
            NotificationsSubstreamState::Pending,
            substream_id,
        ));
        debug_assert!(_was_inserted);

        if !self
            .gossip_desired_peers
            .contains(&(target.clone(), kind, chain_id.0))
        {
            let _was_inserted = self.opened_gossip_undesired.insert((
                chain_id,
                target.clone(),
                GossipKind::ConsensusTransactions,
            ));
            debug_assert!(_was_inserted);
        }

        self.connected_unopened_gossip_desired
            .remove(&(target.clone(), chain_id, kind)); // TODO: clone

        Ok(())
    }

    /// Switches the gossip link to the given peer to the "closed" state.
    ///
    /// This can be used:
    ///
    /// - To close a opening in progress after having called [`ChainNetwork::gossip_open`], in
    /// which case no [`Event::GossipConnected`] or [`Event::GossipOpenFailed`] is generated.
    /// - To close a fully open gossip link. All the notifications that have been queued are still
    /// delivered. No event is generated.
    /// - To respond to a [`Event::GossipInDesired`] by rejecting the request.
    ///
    /// # Panic
    ///
    /// Panics if [`ChainId`] is invalid.
    ///
    pub fn gossip_close(
        &mut self,
        chain_id: ChainId,
        peer_id: &PeerId,
        kind: GossipKind,
    ) -> Result<(), ()> {
        // TODO: proper return value
        let GossipKind::ConsensusTransactions = kind;

        // An `assert!` is necessary because we don't actually access the chain information
        // anywhere, but still want to panic if the chain is invalid.
        assert!(self.chains.contains(chain_id.0));

        // Reject inbound requests, if any.
        if let Some(substream_id) = self
            .notification_substreams_by_peer_id
            .range(
                (
                    NotificationsProtocol::BlockAnnounces {
                        chain_index: chain_id.0,
                    },
                    peer_id.clone(),
                    SubstreamDirection::In,
                    NotificationsSubstreamState::Pending,
                    SubstreamId::min_value(),
                )
                    ..=(
                        NotificationsProtocol::BlockAnnounces {
                            chain_index: chain_id.0,
                        },
                        peer_id.clone(),
                        SubstreamDirection::In,
                        NotificationsSubstreamState::Pending,
                        SubstreamId::max_value(),
                    ),
            )
            .next()
            .map(|(_, _, _, _, substream_id)| *substream_id)
        {
            self.inner.reject_in_notifications(substream_id);

            let _was_in = self.notification_substreams_by_peer_id.remove(&(
                NotificationsProtocol::BlockAnnounces {
                    chain_index: chain_id.0,
                },
                peer_id.clone(),
                SubstreamDirection::In,
                NotificationsSubstreamState::Pending,
                substream_id,
            ));
            debug_assert!(_was_in);

            let _was_in = self.substreams.remove(&substream_id);
            debug_assert!(_was_in.is_some());

            self.opened_gossip_undesired.remove(&(
                chain_id,
                peer_id.clone(),
                GossipKind::ConsensusTransactions,
            ));

            // TODO: debug_assert that there's no inbound tx/gp substream?
        }

        // Close outbound substreams, if any.
        for protocol in [
            NotificationsProtocol::BlockAnnounces {
                chain_index: chain_id.0,
            },
            NotificationsProtocol::Transactions {
                chain_index: chain_id.0,
            },
            NotificationsProtocol::Grandpa {
                chain_index: chain_id.0,
            },
        ] {
            if let Some((substream_id, state)) = self
                .notification_substreams_by_peer_id
                .range(
                    (
                        protocol,
                        peer_id.clone(),
                        SubstreamDirection::Out,
                        NotificationsSubstreamState::min_value(),
                        SubstreamId::min_value(),
                    )
                        ..=(
                            protocol,
                            peer_id.clone(),
                            SubstreamDirection::Out,
                            NotificationsSubstreamState::max_value(),
                            SubstreamId::max_value(),
                        ),
                )
                .next()
                .map(|(_, _, _, state, substream_id)| (*substream_id, *state))
            {
                self.inner.close_out_notifications(substream_id);

                let _was_in = self.notification_substreams_by_peer_id.remove(&(
                    protocol,
                    peer_id.clone(),
                    SubstreamDirection::Out,
                    state,
                    substream_id,
                ));
                debug_assert!(_was_in);

                let _was_in = self.substreams.remove(&substream_id);
                debug_assert!(_was_in.is_some());

                // TODO: close tx and gp as well
                // TODO: doesn't close inbound substreams
            }
        }

        Ok(())
    }

    /// Update the state of the local node with regards to GrandPa rounds.
    ///
    /// Calling this method does two things:
    ///
    /// - Send on all the active GrandPa substreams a "neighbor packet" indicating the state of
    ///   the local node.
    /// - Update the neighbor packet that is automatically sent to peers when a GrandPa substream
    ///   gets opened.
    ///
    /// In other words, calling this function atomically informs all the present and future peers
    /// of the state of the local node regarding the GrandPa protocol.
    ///
    /// > **Note**: The information passed as parameter isn't validated in any way by this method.
    ///
    /// This function might generate a message destined to connections. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process these messages after it has
    /// returned.
    ///
    /// # Panic
    ///
    /// Panics if [`ChainId`] is invalid, or if the chain has GrandPa disabled.
    ///
    pub fn gossip_broadcast_grandpa_state_and_update(
        &mut self,
        chain_id: ChainId,
        grandpa_state: GrandpaState,
    ) {
        // Bytes of the neighbor packet to send out.
        let packet = protocol::GrandpaNotificationRef::Neighbor(protocol::NeighborPacket {
            round_number: grandpa_state.round_number,
            set_id: grandpa_state.set_id,
            commit_finalized_height: grandpa_state.commit_finalized_height,
        })
        .scale_encoding(self.chains[chain_id.0].block_number_bytes)
        .fold(Vec::new(), |mut a, b| {
            a.extend_from_slice(b.as_ref());
            a
        });

        // Now sending out to all the grandpa substreams that exist.
        // TODO: O(n)
        for (_, _, _, _, substream_id) in
            self.notification_substreams_by_peer_id
                .iter()
                .filter(|(p, _, d, s, _)| {
                    *p == NotificationsProtocol::Grandpa {
                        chain_index: chain_id.0,
                    } && *d == SubstreamDirection::Out
                        && *s == NotificationsSubstreamState::Open
                })
        {
            match self.inner.queue_notification(*substream_id, packet.clone()) {
                Ok(()) => {}
                Err(collection::QueueNotificationError::QueueFull) => {}
            }
        }

        // Update the locally-stored state.
        *self.chains[chain_id.0]
            .grandpa_protocol_config
            .as_mut()
            .unwrap() = grandpa_state;
    }

    /// Sends a block announce gossip message to the given peer.
    ///
    /// If no [`Event::GossipConnected`] event of kind [`GossipKind::ConsensusTransactions`] has
    /// been emitted for the given peer, then a [`QueueNotificationError::NoConnection`] will be
    /// returned.
    ///
    /// This function might generate a message destined a connection. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    // TODO: there this extra parameter in block announces that is unused on many chains but not always
    pub fn gossip_send_block_announce(
        &mut self,
        target: &PeerId,
        chain_id: ChainId,
        scale_encoded_header: &[u8],
        is_best: bool,
    ) -> Result<(), QueueNotificationError> {
        let notification = protocol::encode_block_announce(protocol::BlockAnnounceRef {
            scale_encoded_header,
            is_best,
        })
        .fold(Vec::new(), |mut a, b| {
            a.extend_from_slice(b.as_ref());
            a
        });

        self.queue_notification(
            target,
            NotificationsProtocol::BlockAnnounces {
                chain_index: chain_id.0,
            },
            notification,
        )
    }

    /// Sends a transaction gossip message to the given peer.
    ///
    /// Must be passed the SCALE-encoded transaction.
    ///
    /// If no [`Event::GossipConnected`] event of kind [`GossipKind::ConsensusTransactions`] has
    /// been emitted for the given peer, then a [`QueueNotificationError::NoConnection`] will be
    /// returned.
    ///
    /// This function might generate a message destined connections. Use
    /// [`ChainNetwork::pull_message_to_connection`] to process messages after it has returned.
    ///
    /// # Panic
    ///
    /// Panics if the [`ChainId`] is invalid.
    ///
    // TODO: function is awkward due to tx substream not being necessarily always open
    pub fn gossip_send_transaction(
        &mut self,
        target: &PeerId,
        chain_id: ChainId,
        extrinsic: &[u8],
    ) -> Result<(), QueueNotificationError> {
        let mut val = Vec::with_capacity(1 + extrinsic.len());
        val.extend_from_slice(util::encode_scale_compact_usize(1).as_ref());
        val.extend_from_slice(extrinsic);
        self.queue_notification(
            target,
            NotificationsProtocol::Transactions {
                chain_index: chain_id.0,
            },
            val,
        )
    }

    /// Inner implementation for all the notifications sends.
    fn queue_notification(
        &mut self,
        target: &PeerId,
        protocol: NotificationsProtocol,
        notification: Vec<u8>,
    ) -> Result<(), QueueNotificationError> {
        let chain_index = match protocol {
            NotificationsProtocol::BlockAnnounces { chain_index } => chain_index,
            NotificationsProtocol::Transactions { chain_index } => chain_index,
            NotificationsProtocol::Grandpa { chain_index } => chain_index,
        };

        assert!(self.chains.contains(chain_index));

        // We first find a block announces substream for that peer.
        // TODO: only relevant for GossipKind::ConsensusTransactions
        // If none is found, then we are not considered "gossip-connected", and return an error
        // no matter what, even if a substream of the requested protocol exists.
        // TODO: O(n) ; optimize this by using range()
        let block_announces_substream = self
            .notification_substreams_by_peer_id
            .iter()
            .find(move |(p, id, d, s, _)| {
                *p == NotificationsProtocol::BlockAnnounces { chain_index }
                    && id == target
                    && *d == SubstreamDirection::Out
                    && *s == NotificationsSubstreamState::Open
            })
            .map(|(_, _, _, _, substream_id)| *substream_id)
            .ok_or(QueueNotificationError::NoConnection)?;

        // Now find a substream of the requested protocol.
        let substream_id = if matches!(protocol, NotificationsProtocol::BlockAnnounces { .. }) {
            block_announces_substream
        } else {
            // TODO: O(n) ; optimize this by using range()
            let id = self
                .notification_substreams_by_peer_id
                .iter()
                .find(move |(p, id, d, s, _)| {
                    *p == protocol
                        && id == target
                        && *d == SubstreamDirection::Out
                        && *s == NotificationsSubstreamState::Open
                })
                .map(|(_, _, _, _, substream_id)| *substream_id);
            // If we are "gossip-connected" but no open transaction/grandpa substream exists, we
            // silently discard the notification.
            // TODO: this is a questionable behavior
            let Some(id) = id else { return Ok(()) };
            id
        };

        match self.inner.queue_notification(substream_id, notification) {
            Ok(()) => Ok(()),
            Err(collection::QueueNotificationError::QueueFull) => {
                Err(QueueNotificationError::QueueFull)
            }
        }
    }

    fn recognize_protocol(&self, protocol_name: &str) -> Result<Protocol, ()> {
        Ok(match protocol::decode_protocol_name(protocol_name)? {
            protocol::ProtocolName::Identify => Protocol::Identify,
            protocol::ProtocolName::Ping => Protocol::Ping,
            protocol::ProtocolName::BlockAnnounces {
                genesis_hash,
                fork_id,
            } => Protocol::BlockAnnounces {
                chain_index: *self
                    .chains_by_protocol_info
                    .get(&(genesis_hash, fork_id.map(|fork_id| fork_id.to_owned())))
                    .ok_or(())?,
            },
            protocol::ProtocolName::Transactions {
                genesis_hash,
                fork_id,
            } => Protocol::Transactions {
                chain_index: *self
                    .chains_by_protocol_info
                    .get(&(genesis_hash, fork_id.map(|fork_id| fork_id.to_owned())))
                    .ok_or(())?,
            },
            protocol::ProtocolName::Grandpa {
                genesis_hash,
                fork_id,
            } => Protocol::Grandpa {
                chain_index: *self
                    .chains_by_protocol_info
                    .get(&(genesis_hash, fork_id.map(|fork_id| fork_id.to_owned())))
                    .ok_or(())?,
            },
            protocol::ProtocolName::Sync {
                genesis_hash,
                fork_id,
            } => Protocol::Sync {
                chain_index: *self
                    .chains_by_protocol_info
                    .get(&(genesis_hash, fork_id.map(|fork_id| fork_id.to_owned())))
                    .ok_or(())?,
            },
            protocol::ProtocolName::Light {
                genesis_hash,
                fork_id,
            } => Protocol::LightUnknown {
                chain_index: *self
                    .chains_by_protocol_info
                    .get(&(genesis_hash, fork_id.map(|fork_id| fork_id.to_owned())))
                    .ok_or(())?,
            },
            protocol::ProtocolName::Kad {
                genesis_hash,
                fork_id,
            } => Protocol::Kad {
                chain_index: *self
                    .chains_by_protocol_info
                    .get(&(genesis_hash, fork_id.map(|fork_id| fork_id.to_owned())))
                    .ok_or(())?,
            },
            protocol::ProtocolName::SyncWarp {
                genesis_hash,
                fork_id,
            } => Protocol::SyncWarp {
                chain_index: *self
                    .chains_by_protocol_info
                    .get(&(genesis_hash, fork_id.map(|fork_id| fork_id.to_owned())))
                    .ok_or(())?,
            },
            protocol::ProtocolName::State {
                genesis_hash,
                fork_id,
            } => Protocol::State {
                chain_index: *self
                    .chains_by_protocol_info
                    .get(&(genesis_hash, fork_id.map(|fork_id| fork_id.to_owned())))
                    .ok_or(())?,
            },
        })
    }
}

/// What kind of handshake to perform on the newly-added connection.
pub enum SingleStreamHandshakeKind {
    /// Use the multistream-select protocol to negotiate the Noise encryption, then use the
    /// multistream-select protocol to negotiate the Yamux multiplexing.
    MultistreamSelectNoiseYamux {
        /// Must be `true` if the connection has been initiated locally, or `false` if it has been
        /// initiated by the remote.
        is_initiator: bool,
    },
}

/// What kind of handshake to perform on the newly-added connection.
pub enum MultiStreamHandshakeKind {
    /// The connection is a WebRTC connection.
    ///
    /// See <https://github.com/libp2p/specs/pull/412> for details.
    ///
    /// The reading and writing side of substreams must never be closed. Substreams can only be
    /// abruptly destroyed by either side.
    WebRtc {
        /// Must be `true` if the connection has been initiated locally, or `false` if it has been
        /// initiated by the remote.
        is_initiator: bool,
        /// Multihash encoding of the TLS certificate used by the local node at the DTLS layer.
        local_tls_certificate_multihash: Vec<u8>,
        /// Multihash encoding of the TLS certificate used by the remote node at the DTLS layer.
        remote_tls_certificate_multihash: Vec<u8>,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GossipKind {
    ConsensusTransactions,
}

/// Error returned by [`ChainNetwork::add_chain`].
#[derive(Debug, derive_more::Display, Clone)]
pub enum AddChainError {
    /// The genesis hash and fork id are identical to the ones of an existing chain.
    #[display(fmt = "Genesis hash and fork id are identical to the ones of an existing chain.")]
    Duplicate {
        /// Identifier of the chain that uses the same genesis hash and fork id.
        existing_identical: ChainId,
    },
}

/// Event generated by [`ChainNetwork::next_event`].
#[derive(Debug)]
pub enum Event {
    /// A connection that was added with [`ChainNetwork::add_single_stream_connection`] or
    /// [`ChainNetwork::add_multi_stream_connection`] has now finished its handshake phase.
    /// Its [`PeerId`] is now known which certainty.
    HandshakeFinished {
        /// Identifier of the connection.
        id: ConnectionId,
        /// Parameter that was passed to [`ChainNetwork::add_single_stream_connection`] or
        /// [`ChainNetwork::add_multi_stream_connection`].
        expected_peer_id: Option<PeerId>,
        /// Actual [`PeerId`] of the connection.
        peer_id: PeerId,
    },

    /// A connection has shut down before finishing its handshake.
    PreHandshakeDisconnected {
        /// Identifier of the connection.
        id: ConnectionId,
        /// Parameter that was passed to [`ChainNetwork::add_single_stream_connection`] or
        /// [`ChainNetwork::add_multi_stream_connection`].
        address: Vec<u8>,
        /// Parameter that was passed to [`ChainNetwork::add_single_stream_connection`] or
        /// [`ChainNetwork::add_multi_stream_connection`].
        expected_peer_id: Option<PeerId>,
    },

    /// A connection has shut down after finishing its handshake.
    Disconnected {
        /// Identifier of the connection.
        id: ConnectionId,
        /// Parameter that was passed to [`ChainNetwork::add_single_stream_connection`] or
        /// [`ChainNetwork::add_multi_stream_connection`].
        address: Vec<u8>,
        /// Peer that was connected.
        peer_id: PeerId,
    },

    /// Now connected to the given peer for gossiping purposes.
    ///
    /// This event can only happen as a result of a call to [`ChainNetwork::gossip_open`].
    GossipConnected {
        /// Peer we are now connected to.
        peer_id: PeerId,
        /// Chain of the gossip connection.
        chain_id: ChainId,
        /// Which kind of gossip link is concerned.
        kind: GossipKind,
        /// Role the node reports playing on the network.
        role: Role,
        /// Height of the best block according to this node.
        best_number: u64,
        /// Hash of the best block according to this node.
        best_hash: [u8; 32],
    },

    /// An attempt has been made to open the given chain, but something wrong happened.
    ///
    /// This event can only happen as a result of a call to [`ChainNetwork::gossip_open`].
    GossipOpenFailed {
        /// Peer concerned by the event.
        peer_id: PeerId,
        /// Chain of the gossip connection.
        chain_id: ChainId,
        /// Which kind of gossip link is concerned.
        kind: GossipKind,
        /// Problem that happened.
        error: GossipConnectError,
    },

    /// No longer connected to the given peer for gossiping purposes.
    GossipDisconnected {
        /// Peer we are no longer connected to.
        peer_id: PeerId,
        /// Chain of the gossip connection.
        chain_id: ChainId,
        /// Which kind of gossip link is concerned.
        kind: GossipKind,
    },

    /// A peer would like to open a gossiping link with the local node.
    // TODO: document what to do
    // TODO: include handshake content?
    GossipInDesired {
        /// Peer concerned by the event.
        peer_id: PeerId,
        /// Chain of the gossip connection.
        chain_id: ChainId,
        /// Which kind of gossip link is concerned.
        kind: GossipKind,
    },

    /// A previously-emitted [`Event::GossipInDesired`] is no longer relevant as the peer has
    /// stopped the opening attempt.
    GossipInDesiredCancel {
        /// Peer concerned by the event.
        peer_id: PeerId,
        /// Chain of the gossip connection.
        chain_id: ChainId,
        /// Which kind of gossip link is concerned.
        kind: GossipKind,
    },

    /// An outgoing request has finished, either successfully or not.
    RequestResult {
        /// Identifier of the request that was returned by the function that started the request.
        substream_id: SubstreamId,
        /// Outcome of the request.
        response: RequestResult,
    },

    /// Received a new block announce from a peer.
    ///
    /// Can only happen after a [`Event::GossipConnected`] with the given [`PeerId`] and [`ChainId`]
    /// combination has happened.
    BlockAnnounce {
        /// Identity of the sender of the block announce.
        peer_id: PeerId,
        /// Index of the chain the block relates to.
        chain_id: ChainId,
        announce: EncodedBlockAnnounce,
    },

    /// Received a GrandPa neighbor packet from the network. This contains an update to the
    /// finality state of the given peer.
    ///
    /// Can only happen after a [`Event::GossipConnected`] with the given [`PeerId`] and [`ChainId`]
    /// combination has happened.
    GrandpaNeighborPacket {
        /// Identity of the sender of the message.
        peer_id: PeerId,
        /// Index of the chain the message relates to.
        chain_id: ChainId,
        /// State of the remote.
        state: GrandpaState,
    },

    /// Received a GrandPa commit message from the network.
    ///
    /// Can only happen after a [`Event::GossipConnected`] with the given [`PeerId`] and [`ChainId`]
    /// combination has happened.
    GrandpaCommitMessage {
        /// Identity of the sender of the message.
        peer_id: PeerId,
        /// Index of the chain the commit message relates to.
        chain_id: ChainId,
        message: EncodedGrandpaCommitMessage,
    },

    /// Error in the protocol in a connection, such as failure to decode a message. This event
    /// doesn't have any consequence on the health of the connection, and is purely for diagnostic
    /// purposes.
    // TODO: review the concept of protocol error
    ProtocolError {
        /// Peer that has caused the protocol error.
        peer_id: PeerId,
        /// Error that happened.
        error: ProtocolError,
    },

    /// A remote has sent a request for identification information.
    ///
    /// You are strongly encouraged to call [`ChainNetwork::respond_identify`].
    IdentifyRequestIn {
        /// Remote that has sent the request.
        peer_id: PeerId,
        /// Identifier of the request. Necessary to send back the answer.
        substream_id: SubstreamId,
    },

    /// A remote has sent a request for blocks.
    ///
    /// Can only happen for chains where [`ChainConfig::allow_inbound_block_requests`] is `true`.
    ///
    /// You are strongly encouraged to call [`ChainNetwork::respond_blocks`].
    BlocksRequestIn {
        /// Remote that has sent the request.
        peer_id: PeerId,
        /// Index of the chain concerned by the request.
        chain_id: ChainId,
        /// Information about the request.
        config: protocol::BlocksRequestConfig,
        /// Identifier of the request. Necessary to send back the answer.
        substream_id: SubstreamId,
    },

    /// A remote is no longer interested in the response to a request.
    ///
    /// Calling [`ChainNetwork::respond_identify`], [`ChainNetwork::respond_blocks`], or similar
    /// will now panic.
    RequestInCancel {
        /// Identifier of the request.
        ///
        /// This [`SubstreamId`] is considered dead and no longer valid.
        substream_id: SubstreamId,
    },
    /*Transactions {
        peer_id: PeerId,
        transactions: EncodedTransactions,
    }*/
}

/// See [`Event::ProtocolError`].
// TODO: reexport these error types
#[derive(Debug, derive_more::Display)]
pub enum ProtocolError {
    /// Error in an incoming substream.
    #[display(fmt = "Error in an incoming substream: {_0}")]
    InboundError(InboundError),
    /// Error while decoding the handshake of the block announces substream.
    #[display(fmt = "Error while decoding the handshake of the block announces substream: {_0}")]
    BadBlockAnnouncesHandshake(BlockAnnouncesHandshakeDecodeError),
    /// Error while decoding a received block announce.
    #[display(fmt = "Error while decoding a received block announce: {_0}")]
    BadBlockAnnounce(protocol::DecodeBlockAnnounceError),
    /// Error while decoding a received Grandpa notification.
    #[display(fmt = "Error while decoding a received Grandpa notification: {_0}")]
    BadGrandpaNotification(protocol::DecodeGrandpaNotificationError),
    /// Received an invalid identify request.
    BadIdentifyRequest,
    /// Error while decoding a received blocks request.
    #[display(fmt = "Error while decoding a received blocks request: {_0}")]
    BadBlocksRequest(protocol::DecodeBlockRequestError),
}

/// Error potentially returned when starting a request.
#[derive(Debug, Clone, derive_more::Display)]
pub enum StartRequestError {
    /// There is no valid connection to the given peer on which the request can be started.
    NoConnection,
}

/// Error potentially returned when starting a request that might be too large.
#[derive(Debug, Clone, derive_more::Display)]
pub enum StartRequestMaybeTooLargeError {
    /// There is no valid connection to the given peer on which the request can be started.
    NoConnection,
    /// Size of the request is over maximum allowed by the protocol.
    RequestTooLarge,
}

impl From<StartRequestError> for StartRequestMaybeTooLargeError {
    fn from(err: StartRequestError) -> StartRequestMaybeTooLargeError {
        match err {
            StartRequestError::NoConnection => StartRequestMaybeTooLargeError::NoConnection,
        }
    }
}

/// Response to an outgoing request.
///
/// See [`Event::RequestResult`̀].
#[derive(Debug)]
pub enum RequestResult {
    Blocks(Result<Vec<protocol::BlockData>, BlocksRequestError>),
    GrandpaWarpSync(Result<EncodedGrandpaWarpSyncResponse, GrandpaWarpSyncRequestError>),
    State(Result<EncodedStateResponse, StateRequestError>),
    StorageProof(Result<EncodedMerkleProof, StorageProofRequestError>),
    CallProof(Result<EncodedMerkleProof, CallProofRequestError>),
    KademliaFindNode(Result<Vec<(peer_id::PeerId, Vec<Vec<u8>>)>, KademliaFindNodeError>),
}

/// Error returned by [`ChainNetwork::start_blocks_request`].
#[derive(Debug, derive_more::Display)]
pub enum BlocksRequestError {
    /// Error while waiting for the response from the peer.
    #[display(fmt = "{_0}")]
    Request(RequestError),
    /// Error while decoding the response returned by the peer.
    #[display(fmt = "Response decoding error: {_0}")]
    Decode(protocol::DecodeBlockResponseError),
}

/// Error returned by [`ChainNetwork::start_storage_proof_request`].
#[derive(Debug, derive_more::Display, Clone)]
pub enum StorageProofRequestError {
    #[display(fmt = "{_0}")]
    Request(RequestError),
    #[display(fmt = "Response decoding error: {_0}")]
    Decode(protocol::DecodeStorageCallProofResponseError),
    /// The remote is incapable of answering this specific request.
    RemoteCouldntAnswer,
}

/// Error returned by [`ChainNetwork::start_call_proof_request`].
#[derive(Debug, Clone, derive_more::Display)]
pub enum CallProofRequestError {
    #[display(fmt = "{_0}")]
    Request(RequestError),
    #[display(fmt = "Response decoding error: {_0}")]
    Decode(protocol::DecodeStorageCallProofResponseError),
    /// The remote is incapable of answering this specific request.
    RemoteCouldntAnswer,
}

impl CallProofRequestError {
    /// Returns `true` if this is caused by networking issues, as opposed to a consensus-related
    /// issue.
    pub fn is_network_problem(&self) -> bool {
        match self {
            CallProofRequestError::Request(_) => true,
            CallProofRequestError::Decode(_) => false,
            CallProofRequestError::RemoteCouldntAnswer => true,
        }
    }
}

/// Error returned by [`ChainNetwork::start_grandpa_warp_sync_request`].
#[derive(Debug, derive_more::Display)]
pub enum GrandpaWarpSyncRequestError {
    #[display(fmt = "{_0}")]
    Request(RequestError),
    #[display(fmt = "Response decoding error: {_0}")]
    Decode(protocol::DecodeGrandpaWarpSyncResponseError),
}

/// Error returned by [`ChainNetwork::start_state_request`].
#[derive(Debug, derive_more::Display)]
pub enum StateRequestError {
    #[display(fmt = "{_0}")]
    Request(RequestError),
    #[display(fmt = "Response decoding error: {_0}")]
    Decode(protocol::DecodeStateResponseError),
}

/// Error during [`ChainNetwork::start_kademlia_find_node_request`].
#[derive(Debug, derive_more::Display)]
pub enum KademliaFindNodeError {
    /// Error during the request.
    #[display(fmt = "{_0}")]
    RequestFailed(RequestError),
    /// Failed to decode the response.
    #[display(fmt = "Response decoding error: {_0}")]
    DecodeError(protocol::DecodeFindNodeResponseError),
}

/// Error potentially returned when queueing a notification.
#[derive(Debug, derive_more::Display)]
pub enum QueueNotificationError {
    /// There is no valid substream to the given peer on which the notification can be sent.
    NoConnection,
    /// Queue of notifications with that peer is full.
    QueueFull,
}

/// Undecoded but valid block announce.
#[derive(Clone)]
pub struct EncodedBlockAnnounce {
    message: Vec<u8>,
    block_number_bytes: usize,
}

impl EncodedBlockAnnounce {
    /// Returns the decoded version of the announcement.
    pub fn decode(&self) -> protocol::BlockAnnounceRef {
        protocol::decode_block_announce(&self.message, self.block_number_bytes).unwrap()
    }
}

impl fmt::Debug for EncodedBlockAnnounce {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}

/// Undecoded but valid Merkle proof.
#[derive(Clone)]
pub struct EncodedMerkleProof(Vec<u8>, protocol::StorageOrCallProof);

impl EncodedMerkleProof {
    /// Returns the SCALE-encoded Merkle proof.
    pub fn decode(&self) -> &[u8] {
        protocol::decode_storage_or_call_proof_response(self.1, &self.0)
            .unwrap()
            .unwrap()
    }
}

impl fmt::Debug for EncodedMerkleProof {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}

/// Undecoded but valid GrandPa warp sync response.
#[derive(Clone)]
pub struct EncodedGrandpaWarpSyncResponse {
    message: Vec<u8>,
    block_number_bytes: usize,
}

impl EncodedGrandpaWarpSyncResponse {
    /// Returns the encoded bytes of the warp sync message.
    pub fn as_encoded(&self) -> &[u8] {
        &self.message
    }

    /// Returns the decoded version of the warp sync message.
    pub fn decode(&self) -> protocol::GrandpaWarpSyncResponse {
        match protocol::decode_grandpa_warp_sync_response(&self.message, self.block_number_bytes) {
            Ok(msg) => msg,
            _ => unreachable!(),
        }
    }
}

impl fmt::Debug for EncodedGrandpaWarpSyncResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}

/// Undecoded but valid state response.
// TODO: merge with EncodedMerkleProof?
#[derive(Clone)]
pub struct EncodedStateResponse(Vec<u8>);

impl EncodedStateResponse {
    /// Returns the Merkle proof of the state response.
    pub fn decode(&self) -> &[u8] {
        match protocol::decode_state_response(&self.0) {
            Ok(r) => r,
            Err(_) => unreachable!(),
        }
    }
}

impl fmt::Debug for EncodedStateResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}

#[derive(Debug, Copy, Clone)]
// TODO: link to some doc about how GrandPa works: what is a round, what is the set id, etc.
pub struct GrandpaState {
    pub round_number: u64,
    /// Set of authorities that will be used by the node to try finalize the children of the block
    /// of [`GrandpaState::commit_finalized_height`].
    pub set_id: u64,
    /// Height of the highest block considered final by the node.
    pub commit_finalized_height: u64,
}

/// Undecoded but valid block announce handshake.
pub struct EncodedBlockAnnounceHandshake {
    handshake: Vec<u8>,
    block_number_bytes: usize,
}

impl EncodedBlockAnnounceHandshake {
    /// Returns the decoded version of the handshake.
    pub fn decode(&self) -> protocol::BlockAnnouncesHandshakeRef {
        protocol::decode_block_announces_handshake(self.block_number_bytes, &self.handshake)
            .unwrap()
    }
}

impl fmt::Debug for EncodedBlockAnnounceHandshake {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}

/// Error that can happen when trying to open an outbound block announces notifications substream.
#[derive(Debug, Clone, derive_more::Display)]
pub enum GossipConnectError {
    /// Error in the underlying protocol.
    #[display(fmt = "{_0}")]
    Substream(NotificationsOutErr),
    /// Error decoding the block announces handshake.
    HandshakeDecode(BlockAnnouncesHandshakeDecodeError),
    /// Mismatch between the genesis hash of the remote and the local genesis hash.
    #[display(fmt = "Mismatch between the genesis hash of the remote and the local genesis hash")]
    GenesisMismatch {
        /// Hash of the genesis block of the chain according to the local node.
        local_genesis: [u8; 32],
        /// Hash of the genesis block of the chain according to the remote node.
        remote_genesis: [u8; 32],
    },
}

/// Undecoded but valid GrandPa commit message.
#[derive(Clone)]
pub struct EncodedGrandpaCommitMessage {
    message: Vec<u8>,
    block_number_bytes: usize,
}

impl EncodedGrandpaCommitMessage {
    /// Returns the encoded bytes of the commit message.
    pub fn into_encoded(mut self) -> Vec<u8> {
        // Skip the first byte because `self.message` is a `GrandpaNotificationRef`.
        self.message.remove(0);
        self.message
    }

    /// Returns the encoded bytes of the commit message.
    pub fn as_encoded(&self) -> &[u8] {
        // Skip the first byte because `self.message` is a `GrandpaNotificationRef`.
        &self.message[1..]
    }

    /// Returns the decoded version of the commit message.
    pub fn decode(&self) -> protocol::CommitMessageRef {
        match protocol::decode_grandpa_notification(&self.message, self.block_number_bytes) {
            Ok(protocol::GrandpaNotificationRef::Commit(msg)) => msg,
            _ => unreachable!(),
        }
    }
}

impl fmt::Debug for EncodedGrandpaCommitMessage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}
