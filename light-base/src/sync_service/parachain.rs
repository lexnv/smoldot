// Smoldot
// Copyright (C) 2019-2022  Parity Technologies (UK) Ltd.
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

use super::ToBackground;
use crate::{network_service, platform::PlatformRef, runtime_service, util};

use alloc::{borrow::ToOwned as _, boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{
    iter, mem,
    num::{NonZeroU32, NonZeroUsize},
    pin::Pin,
    time::Duration,
};
use futures_lite::FutureExt as _;
use futures_util::{future, stream, FutureExt as _, StreamExt as _};
use hashbrown::HashMap;
use itertools::Itertools as _;
use smoldot::{
    chain::async_tree,
    executor::{host, runtime_host},
    header,
    informant::HashDisplay,
    libp2p::PeerId,
    network::protocol,
    sync::{all_forks::sources, para},
};

/// Starts a sync service background task to synchronize a parachain.
pub(super) async fn start_parachain<TPlat: PlatformRef>(
    log_target: String,
    platform: TPlat,
    finalized_block_header: Vec<u8>,
    block_number_bytes: usize,
    relay_chain_sync: Arc<runtime_service::RuntimeService<TPlat>>,
    relay_chain_block_number_bytes: usize,
    parachain_id: u32,
    from_foreground: async_channel::Receiver<ToBackground>,
    network_service: Arc<network_service::NetworkService<TPlat>>,
    network_chain_id: network_service::ChainId,
    from_network_service: stream::BoxStream<'static, network_service::Event>,
) {
    ParachainBackgroundTask {
        log_target,
        from_foreground,
        block_number_bytes,
        relay_chain_block_number_bytes,
        parachain_id,
        network_service,
        network_chain_id,
        from_network_service: from_network_service.fuse(),
        sync_sources: sources::AllForksSources::new(
            40,
            header::decode(&finalized_block_header, block_number_bytes)
                .unwrap()
                .number,
        ),
        obsolete_finalized_parahead: finalized_block_header,
        sync_sources_map: HashMap::with_capacity_and_hasher(
            0,
            util::SipHasherBuild::new({
                let mut seed = [0; 16];
                platform.fill_random_bytes(&mut seed);
                seed
            }),
        ),
        subscription_state: ParachainBackgroundState::NotSubscribed {
            all_subscriptions: Vec::new(),
            subscribe_future: {
                let relay_chain_sync = relay_chain_sync.clone();
                Box::pin(async move {
                    relay_chain_sync
                        .subscribe_all(
                            "parachain-sync",
                            32,
                            NonZeroUsize::new(usize::max_value()).unwrap(),
                        )
                        .await
                })
            },
        },
        relay_chain_sync,
        platform,
    }
    .run()
    .await;
}

/// Task that is running in the background.
struct ParachainBackgroundTask<TPlat: PlatformRef> {
    /// Target to use for all logs.
    log_target: String,

    /// Access to the platform's capabilities.
    platform: TPlat,

    /// Channel receiving message from the sync service frontend.
    from_foreground: async_channel::Receiver<ToBackground>,

    /// Number of bytes to use to encode the parachain block numbers in headers.
    block_number_bytes: usize,

    /// Number of bytes to use to encode the relay chain block numbers in headers.
    relay_chain_block_number_bytes: usize,

    /// Id of the parachain registered within the relay chain. Chosen by the user.
    parachain_id: u32,

    /// Networking service connected to the peer-to-peer network of the parachain.
    network_service: Arc<network_service::NetworkService<TPlat>>,

    /// Index of the chain within the associated network service.
    ///
    /// Used to filter events from [`ParachainBackgroundTask::from_network_service`].
    network_chain_id: network_service::ChainId,

    /// Events coming from the networking service.
    from_network_service: stream::Fuse<stream::BoxStream<'static, network_service::Event>>,

    /// Runtime service of the relay chain.
    relay_chain_sync: Arc<runtime_service::RuntimeService<TPlat>>,

    /// Last-known finalized parachain header. Can be very old and obsolete.
    /// Updated after we successfully fetch the parachain head of a relay chain finalized block,
    /// and left untouched if the fetch fails.
    /// Initialized to the parachain genesis block header.
    obsolete_finalized_parahead: Vec<u8>,

    /// State machine that tracks the list of parachain network sources and their known blocks.
    sync_sources: sources::AllForksSources<(PeerId, protocol::Role)>,

    /// Maps `PeerId`s to their indices within `sync_sources`.
    sync_sources_map: HashMap<PeerId, sources::SourceId, util::SipHasherBuild>,

    /// Extra fields that are set after the subscription to the runtime service events has
    /// succeeded.
    subscription_state: ParachainBackgroundState<TPlat>,
}

enum ParachainBackgroundState<TPlat: PlatformRef> {
    /// Currently subscribing to the relay chain runtime service.
    NotSubscribed {
        /// List of senders that will get notified when the tree of blocks is modified.
        ///
        /// These subscriptions are pending and no notification should be sent to them until the
        /// subscription to the relay chain runtime service is finished.
        all_subscriptions: Vec<async_channel::Sender<super::Notification>>,

        /// Future when the subscription has finished.
        subscribe_future: future::BoxFuture<'static, runtime_service::SubscribeAll<TPlat>>,
    },

    /// Subscribed to the relay chain runtime service.
    Subscribed(ParachainBackgroundTaskAfterSubscription<TPlat>),
}

struct ParachainBackgroundTaskAfterSubscription<TPlat: PlatformRef> {
    /// List of senders that get notified when the tree of blocks is modified.
    all_subscriptions: Vec<async_channel::Sender<super::Notification>>,

    /// Stream of blocks of the relay chain this parachain is registered on.
    /// The buffer size should be large enough so that, if the CPU is busy, it doesn't become full
    /// before the execution of the sync service resumes.
    /// The maximum number of pinned block is ignored, as this maximum is a way to avoid malicious
    /// behaviors. This code is by definition not considered malicious.
    relay_chain_subscribe_all: runtime_service::Subscription<TPlat>,

    /// Hash of the best parachain that has been reported to the subscriptions.
    /// `None` if and only if no finalized parachain head is known yet.
    reported_best_parahead_hash: Option<[u8; 32]>,

    /// Tree of relay chain blocks. Blocks are inserted when received from the relay chain
    /// sync service. Once inside, their corresponding parachain head is fetched. Once the
    /// parachain head is fetched, this parachain head is reported to our subscriptions.
    ///
    /// The root of the tree is a "virtual" block. It can be thought as the parent of the relay
    /// chain finalized block, but is there even if the relay chain finalized block is block 0.
    ///
    /// All block in the tree has an associated parachain head behind an `Option`. This `Option`
    /// always contains `Some`, except for the "virtual" root block for which it is `None`.
    ///
    /// If the output finalized block has a parachain head equal to `None`, it therefore means
    /// that no finalized parachain head is known yet.
    /// Note that, when it is the case, `SubscribeAll` messages from the frontend are still
    /// answered with a single finalized block set to `obsolete_finalized_parahead`. Once a
    /// finalized parachain head is known, it is important to reset all subscriptions.
    ///
    /// The set of blocks in this tree whose parachain block hasn't been fetched yet is the same
    /// as the set of blocks that is maintained pinned on the runtime service. Blocks are unpinned
    /// when their parachain head fetching succeeds or when they are removed from the tree.
    async_tree: async_tree::AsyncTree<TPlat::Instant, [u8; 32], Option<Vec<u8>>>,

    /// List of in-progress parachain head fetching operations.
    ///
    /// The operations require some blocks to be pinned within the relay chain runtime service,
    /// which is guaranteed by the fact that `relay_chain_subscribe_all.new_blocks` stays
    /// alive for longer than this container, and by the fact that we unpin block after a
    /// fetching operation has finished and that we never fetch twice for the same block.
    in_progress_paraheads: stream::FuturesUnordered<
        future::BoxFuture<'static, (async_tree::AsyncOpId, Result<Vec<u8>, ParaheadError>)>,
    >,

    /// Future that is ready when we need to start a new parachain head fetch operation.
    next_start_parahead_fetch:
        future::Either<Pin<Box<future::Fuse<TPlat::Delay>>>, future::Pending<()>>,
}

impl<TPlat: PlatformRef> ParachainBackgroundTask<TPlat> {
    async fn run(mut self) {
        loop {
            // Start fetching paraheads of new blocks whose parahead needs to be fetched.
            self.start_paraheads_fetch();

            // Report to the outside any block in the `async_tree` that is now ready.
            self.advance_and_report_notifications().await;

            // Now wait until something interesting happens.
            enum WhatHappened<TPlat: PlatformRef> {
                ForegroundClosed,
                ForegroundMessage(ToBackground),
                NewSubscription(runtime_service::SubscribeAll<TPlat>),
                StartParaheadFetch,
                ParaheadFetchFinished {
                    async_op_id: async_tree::AsyncOpId,
                    parahead_result: Result<Vec<u8>, ParaheadError>,
                },
                Notification(runtime_service::Notification),
                SubscriptionDead,
                NetworkEvent(network_service::Event),
            }

            let what_happened: WhatHappened<_> = {
                let (
                    subscribe_future,
                    next_start_parahead_fetch,
                    relay_chain_subscribe_all,
                    in_progress_paraheads,
                    is_subscribed,
                ) = match &mut self.subscription_state {
                    ParachainBackgroundState::NotSubscribed {
                        subscribe_future, ..
                    } => (Some(subscribe_future), None, None, None, false),
                    ParachainBackgroundState::Subscribed(runtime_subscription) => (
                        None,
                        Some(&mut runtime_subscription.next_start_parahead_fetch),
                        Some(&mut runtime_subscription.relay_chain_subscribe_all),
                        Some(&mut runtime_subscription.in_progress_paraheads),
                        true,
                    ),
                };

                let new_subscription = async {
                    if let Some(subscribe_future) = subscribe_future {
                        WhatHappened::NewSubscription(subscribe_future.await)
                    } else {
                        future::pending().await
                    }
                };

                let start_parahead_fetch = async {
                    if let Some(next_start_parahead_fetch) = next_start_parahead_fetch {
                        next_start_parahead_fetch.await;
                        WhatHappened::StartParaheadFetch
                    } else {
                        future::pending().await
                    }
                };

                let parahead_fetch_finished = async {
                    if let Some(in_progress_paraheads) = in_progress_paraheads {
                        if !in_progress_paraheads.is_empty() {
                            let (async_op_id, parahead_result) =
                                in_progress_paraheads.next().await.unwrap();
                            WhatHappened::ParaheadFetchFinished {
                                async_op_id,
                                parahead_result,
                            }
                        } else {
                            future::pending().await
                        }
                    } else {
                        future::pending().await
                    }
                };

                let subscription_notification = async {
                    if let Some(relay_chain_subscribe_all) = relay_chain_subscribe_all {
                        match relay_chain_subscribe_all.next().await {
                            Some(notif) => WhatHappened::Notification(notif),
                            None => WhatHappened::SubscriptionDead,
                        }
                    } else {
                        future::pending().await
                    }
                };

                let on_foreground_message = async {
                    match self.from_foreground.next().await {
                        Some(msg) => WhatHappened::ForegroundMessage(msg),
                        None => WhatHappened::ForegroundClosed,
                    }
                };

                let network_event = async {
                    if is_subscribed {
                        // We expect the networking channel to never close, so the event is
                        // unwrapped.
                        WhatHappened::NetworkEvent(self.from_network_service.next().await.unwrap())
                    } else {
                        future::pending().await
                    }
                };

                on_foreground_message
                    .or(new_subscription)
                    .or(start_parahead_fetch)
                    .or(parahead_fetch_finished)
                    .or(subscription_notification)
                    .or(network_event)
                    .await
            };

            match what_happened {
                WhatHappened::ForegroundClosed => {
                    // Terminate the parachain syncing.
                    return;
                }

                WhatHappened::NewSubscription(subscription) => {
                    self.set_new_subscription(subscription);
                }

                WhatHappened::StartParaheadFetch => {
                    // Do nothing. This is simply to wake up and loop again.
                }

                WhatHappened::Notification(relay_chain_notif) => {
                    // Update the local tree of blocks to match the update sent by the
                    // relay chain syncing service.
                    self.process_relay_chain_notification(relay_chain_notif);
                }

                WhatHappened::SubscriptionDead => {
                    // Recreate the channel.
                    log::debug!(target: &self.log_target, "Subscriptions <= Reset");
                    self.subscription_state = ParachainBackgroundState::NotSubscribed {
                        all_subscriptions: Vec::new(),
                        subscribe_future: {
                            let relay_chain_sync = self.relay_chain_sync.clone();
                            Box::pin(async move {
                                relay_chain_sync
                                    .subscribe_all(
                                        "parachain-sync",
                                        32,
                                        NonZeroUsize::new(usize::max_value()).unwrap(),
                                    )
                                    .await
                            })
                        },
                    };
                    continue;
                }

                WhatHappened::ParaheadFetchFinished {
                    async_op_id,
                    parahead_result,
                } => {
                    // A parahead fetching operation is finished.
                    self.process_parahead_fetch_result(async_op_id, parahead_result)
                        .await;
                }

                WhatHappened::ForegroundMessage(foreground_message) => {
                    // Message from the public API of the syncing service.
                    self.process_foreground_message(foreground_message).await;
                }

                WhatHappened::NetworkEvent(event) => {
                    // Something happened on the networking.
                    self.process_network_event(event)
                }
            }
        }
    }

    async fn process_foreground_message(&mut self, foreground_message: ToBackground) {
        match (foreground_message, &mut self.subscription_state) {
            (
                ToBackground::IsNearHeadOfChainHeuristic { send_back },
                ParachainBackgroundState::Subscribed(sub),
            ) if sub.async_tree.output_finalized_async_user_data().is_some() => {
                // Since there is a mapping between relay chain blocks and parachain blocks,
                // whether a parachain is at the head of the chain is the same thing as whether
                // its relay chain is at the head of the chain.
                // Note that there is no ordering guarantee of any kind w.r.t. block subscriptions
                // notifications.
                let val = self
                    .relay_chain_sync
                    .is_near_head_of_chain_heuristic()
                    .await;
                let _ = send_back.send(val);
            }
            (ToBackground::IsNearHeadOfChainHeuristic { send_back }, _) => {
                // If no finalized parahead is known yet, we might be very close to the head but
                // also maybe very very far away. We lean on the cautious side and always return
                // `false`.
                let _ = send_back.send(false);
            }
            (
                ToBackground::SubscribeAll {
                    send_back,
                    buffer_size,
                    ..
                },
                ParachainBackgroundState::NotSubscribed {
                    all_subscriptions, ..
                },
            ) => {
                let (tx, new_blocks) = async_channel::bounded(buffer_size.saturating_sub(1));

                // No known finalized parahead.
                let _ = send_back.send(super::SubscribeAll {
                    finalized_block_scale_encoded_header: self.obsolete_finalized_parahead.clone(),
                    finalized_block_runtime: None,
                    non_finalized_blocks_ancestry_order: Vec::new(),
                    new_blocks,
                });

                all_subscriptions.push(tx);
            }
            (
                ToBackground::SubscribeAll {
                    send_back,
                    buffer_size,
                    ..
                },
                ParachainBackgroundState::Subscribed(runtime_subscription),
            ) => {
                let (tx, new_blocks) = async_channel::bounded(buffer_size.saturating_sub(1));

                // There are two possibilities here: either we know of any recent finalized
                // parahead, or we don't. In case where we don't know of any finalized parahead
                // yet, we report a single obsolete finalized parahead, which is
                // `obsolete_finalized_parahead`. The rest of this module makes sure that no
                // other block is reported to subscriptions as long as this is the case, and that
                // subscriptions are reset once the first known finalized parahead is known.
                if let Some(finalized_parahead) = runtime_subscription
                    .async_tree
                    .output_finalized_async_user_data()
                {
                    // Finalized parahead is known.
                    let finalized_parahash =
                        header::hash_from_scale_encoded_header(finalized_parahead);
                    let _ = send_back.send(super::SubscribeAll {
                        finalized_block_scale_encoded_header: finalized_parahead.clone(),
                        finalized_block_runtime: None,
                        non_finalized_blocks_ancestry_order: {
                            let mut list =
                                Vec::<([u8; 32], super::BlockNotification)>::with_capacity(
                                    runtime_subscription
                                        .async_tree
                                        .num_input_non_finalized_blocks(),
                                );

                            for relay_block in runtime_subscription
                                .async_tree
                                .input_output_iter_ancestry_order()
                            {
                                let parablock = match relay_block.async_op_user_data {
                                    Some(b) => b.as_ref().unwrap(),
                                    None => continue,
                                };

                                let parablock_hash =
                                    header::hash_from_scale_encoded_header(parablock);

                                // TODO: O(n)
                                if let Some((_, entry)) =
                                    list.iter_mut().find(|(h, _)| *h == parablock_hash)
                                {
                                    // Block is already in the list. Don't add it a second time.
                                    if relay_block.is_output_best {
                                        entry.is_new_best = true;
                                    }
                                    continue;
                                }

                                // Find the parent of the parablock. This is done by going through
                                // the ancestors of the corresponding relay chain block (until and
                                // including the finalized relay chain block) until we find one
                                // whose parablock is different from the parablock in question.
                                // If none is found, the parablock is the same as the finalized
                                // parablock.
                                let parent_hash = runtime_subscription
                                    .async_tree
                                    .ancestors(relay_block.id)
                                    .find_map(|idx| {
                                        let hash = header::hash_from_scale_encoded_header(
                                            runtime_subscription
                                                .async_tree
                                                .block_async_user_data(idx)
                                                .unwrap()
                                                .as_ref()
                                                .unwrap(),
                                        );
                                        if hash != parablock_hash {
                                            Some(hash)
                                        } else {
                                            None
                                        }
                                    })
                                    .or_else(|| {
                                        if finalized_parahash != parablock_hash {
                                            Some(finalized_parahash)
                                        } else {
                                            None
                                        }
                                    });

                                // `parent_hash` is `None` if the parablock is
                                // the same as the finalized parablock, in which case we
                                // don't add it to the list.
                                if let Some(parent_hash) = parent_hash {
                                    debug_assert!(
                                        list.iter().filter(|(h, _)| *h == parent_hash).count() == 1
                                            || parent_hash == finalized_parahash
                                    );
                                    list.push((
                                        parablock_hash,
                                        super::BlockNotification {
                                            is_new_best: relay_block.is_output_best,
                                            scale_encoded_header: parablock.clone(),
                                            parent_hash,
                                        },
                                    ));
                                }
                            }

                            list.into_iter().map(|(_, v)| v).collect()
                        },
                        new_blocks,
                    });
                } else {
                    // No known finalized parahead.
                    let _ = send_back.send(super::SubscribeAll {
                        finalized_block_scale_encoded_header: self
                            .obsolete_finalized_parahead
                            .clone(),
                        finalized_block_runtime: None,
                        non_finalized_blocks_ancestry_order: Vec::new(),
                        new_blocks,
                    });
                }

                runtime_subscription.all_subscriptions.push(tx);
            }

            (
                ToBackground::PeersAssumedKnowBlock {
                    send_back,
                    block_number,
                    block_hash,
                },
                _,
            ) => {
                // If `block_number` is over the finalized block, then which source knows which
                // block is precisely tracked. Otherwise, it is assumed that all sources are on
                // the finalized chain and thus that all sources whose best block is superior to
                // `block_number` have it.
                let list = if block_number > self.sync_sources.finalized_block_height() {
                    self.sync_sources
                        .knows_non_finalized_block(block_number, &block_hash)
                        .map(|local_id| self.sync_sources[local_id].0.clone())
                        .collect()
                } else {
                    self.sync_sources
                        .keys()
                        .filter(|local_id| {
                            self.sync_sources.best_block(*local_id).0 >= block_number
                        })
                        .map(|local_id| self.sync_sources[local_id].0.clone())
                        .collect()
                };

                let _ = send_back.send(list);
            }
            (ToBackground::SyncingPeers { send_back }, _) => {
                let _ = send_back.send(
                    self.sync_sources
                        .keys()
                        .map(|local_id| {
                            let (height, hash) = self.sync_sources.best_block(local_id);
                            let (peer_id, role) = self.sync_sources[local_id].clone();
                            (peer_id, role, height, *hash)
                        })
                        .collect(),
                );
            }
            (ToBackground::SerializeChainInformation { send_back }, _) => {
                let _ = send_back.send(None);
            }
        }
    }

    fn process_network_event(&mut self, network_event: network_service::Event) {
        match network_event {
            network_service::Event::Connected {
                peer_id,
                role,
                chain_id,
                best_block_number,
                best_block_hash,
            } if chain_id == self.network_chain_id => {
                let local_id = self.sync_sources.add_source(
                    best_block_number,
                    best_block_hash,
                    (peer_id.clone(), role),
                );
                self.sync_sources_map.insert(peer_id, local_id);
            }
            network_service::Event::Disconnected { peer_id, chain_id }
                if chain_id == self.network_chain_id =>
            {
                let local_id = self.sync_sources_map.remove(&peer_id).unwrap();
                let (_peer_id, _role) = self.sync_sources.remove(local_id);
                debug_assert_eq!(peer_id, _peer_id);
            }
            network_service::Event::BlockAnnounce {
                chain_id,
                peer_id,
                announce,
            } if chain_id == self.network_chain_id => {
                let local_id = *self.sync_sources_map.get(&peer_id).unwrap();
                let decoded = announce.decode();
                if let Ok(decoded_header) =
                    header::decode(decoded.scale_encoded_header, self.block_number_bytes)
                {
                    let decoded_header_hash =
                        header::hash_from_scale_encoded_header(decoded.scale_encoded_header);
                    self.sync_sources.add_known_block(
                        local_id,
                        decoded_header.number,
                        decoded_header_hash,
                    );
                    if decoded.is_best {
                        self.sync_sources.add_known_block_and_set_best(
                            local_id,
                            decoded_header.number,
                            decoded_header_hash,
                        );
                    }
                }
            }
            _ => {
                // Uninteresting message or irrelevant chain index.
            }
        }
    }

    /// Start fetching parachain headers of new blocks whose parachain block needs to be fetched.
    fn start_paraheads_fetch(&mut self) {
        let runtime_subscription = match &mut self.subscription_state {
            ParachainBackgroundState::NotSubscribed { .. } => return,
            ParachainBackgroundState::Subscribed(s) => s,
        };

        // Internal state check.
        debug_assert_eq!(
            runtime_subscription.reported_best_parahead_hash.is_some(),
            runtime_subscription
                .async_tree
                .output_finalized_async_user_data()
                .is_some()
        );

        while runtime_subscription.in_progress_paraheads.len() < 4 {
            match runtime_subscription
                .async_tree
                .next_necessary_async_op(&self.platform.now())
            {
                async_tree::NextNecessaryAsyncOp::NotReady { when: Some(when) } => {
                    runtime_subscription.next_start_parahead_fetch =
                        future::Either::Left(Box::pin(self.platform.sleep_until(when).fuse()));
                    break;
                }
                async_tree::NextNecessaryAsyncOp::NotReady { when: None } => {
                    runtime_subscription.next_start_parahead_fetch =
                        future::Either::Right(future::pending());
                    break;
                }
                async_tree::NextNecessaryAsyncOp::Ready(op) => {
                    log::debug!(
                        target: &self.log_target,
                        "ParaheadFetchOperations <= StartFetch(relay_block_hash={})",
                        HashDisplay(op.block_user_data),
                    );

                    runtime_subscription.in_progress_paraheads.push({
                        let relay_chain_sync = self.relay_chain_sync.clone();
                        let subscription_id = runtime_subscription.relay_chain_subscribe_all.id();
                        let block_hash = *op.block_user_data;
                        let async_op_id = op.id;
                        let relay_chain_block_number_bytes = self.relay_chain_block_number_bytes;
                        let parachain_id = self.parachain_id;
                        Box::pin(async move {
                            (
                                async_op_id,
                                parahead(
                                    &relay_chain_sync,
                                    relay_chain_block_number_bytes,
                                    subscription_id,
                                    parachain_id,
                                    &block_hash,
                                )
                                .await,
                            )
                        })
                    });
                }
            }
        }
    }

    async fn process_parahead_fetch_result(
        &mut self,
        async_op_id: async_tree::AsyncOpId,
        parahead_result: Result<Vec<u8>, ParaheadError>,
    ) {
        let runtime_subscription = match &mut self.subscription_state {
            ParachainBackgroundState::NotSubscribed { .. } => return,
            ParachainBackgroundState::Subscribed(s) => s,
        };

        match parahead_result {
            Ok(parahead) => {
                log::debug!(
                    target: &self.log_target,
                    "ParaheadFetchOperations => Parahead(hash={}, relay_blocks={})",
                    HashDisplay(blake2_rfc::blake2b::blake2b(32, b"", &parahead).as_bytes()),
                    runtime_subscription.async_tree.async_op_blocks(async_op_id).map(|b| HashDisplay(b)).join(",")
                );

                // Unpin the relay blocks whose parahead is now known.
                for block in runtime_subscription
                    .async_tree
                    .async_op_finished(async_op_id, Some(parahead))
                {
                    let hash = runtime_subscription.async_tree.block_user_data(block);
                    runtime_subscription
                        .relay_chain_subscribe_all
                        .unpin_block(hash)
                        .await;
                }
            }
            Err(ParaheadError::ObsoleteSubscription) => {
                // The relay chain runtime service has some kind of gap or issue and has discarded
                // the runtime.
                // Destroy the subscription and recreate the channels.
                log::debug!(target: &self.log_target, "Subscriptions <= Reset");
                self.subscription_state = ParachainBackgroundState::NotSubscribed {
                    all_subscriptions: Vec::new(),
                    subscribe_future: {
                        let relay_chain_sync = self.relay_chain_sync.clone();
                        Box::pin(async move {
                            relay_chain_sync
                                .subscribe_all(
                                    "parachain-sync",
                                    32,
                                    NonZeroUsize::new(usize::max_value()).unwrap(),
                                )
                                .await
                        })
                    },
                };
            }
            Err(error) => {
                // Several chains initially didn't support parachains, and have later been
                // upgraded to support them. Similarly, the parachain might not have had a core on
                // the relay chain until recently. For these reasons, errors when the relay chain
                // is not near head of the chain are most likely normal and do not warrant logging
                // an error.
                if self
                    .relay_chain_sync
                    .is_near_head_of_chain_heuristic()
                    .await
                    && !error.is_network_problem()
                {
                    log::error!(
                        target: &self.log_target,
                        "Failed to fetch the parachain head from relay chain blocks {}: {}",
                        runtime_subscription.async_tree.async_op_blocks(async_op_id).map(|b| HashDisplay(b)).join(", "),
                        error
                    );
                }

                log::debug!(
                    target: &self.log_target,
                    "ParaheadFetchOperations => Error(relay_blocks={}, error={:?})",
                    runtime_subscription.async_tree.async_op_blocks(async_op_id).map(|b| HashDisplay(b)).join(","),
                    error
                );

                runtime_subscription
                    .async_tree
                    .async_op_failure(async_op_id, &self.platform.now());
            }
        }
    }

    async fn advance_and_report_notifications(&mut self) {
        let runtime_subscription = match &mut self.subscription_state {
            ParachainBackgroundState::NotSubscribed { .. } => return,
            ParachainBackgroundState::Subscribed(s) => s,
        };

        while let Some(update) = runtime_subscription.async_tree.try_advance_output() {
            match update {
                async_tree::OutputUpdate::Finalized {
                    async_op_user_data: new_finalized_parahead,
                    former_finalized_async_op_user_data: former_finalized_parahead,
                    pruned_blocks,
                    ..
                } if *new_finalized_parahead != former_finalized_parahead => {
                    debug_assert!(new_finalized_parahead.is_some());

                    // If this is the first time (in this loop) a finalized parahead is known,
                    // any `SubscribeAll` message that has been answered beforehand was
                    // answered in a dummy way with a potentially obsolete finalized header.
                    // For this reason, we reset all subscriptions to force all subscribers to
                    // re-subscribe.
                    if former_finalized_parahead.is_none() {
                        runtime_subscription.all_subscriptions.clear();
                    }

                    let hash = header::hash_from_scale_encoded_header(
                        new_finalized_parahead.as_ref().unwrap(),
                    );

                    self.obsolete_finalized_parahead = new_finalized_parahead.clone().unwrap();

                    if let Ok(header) =
                        header::decode(&self.obsolete_finalized_parahead, self.block_number_bytes)
                    {
                        debug_assert!(
                            former_finalized_parahead.is_none()
                                || header.number == self.sync_sources.finalized_block_height()
                                || header.number == self.sync_sources.finalized_block_height() + 1
                        );

                        self.sync_sources.set_finalized_block_height(header.number);
                        // TODO: what about an `else`? does sync_sources leak if the block can't be decoded?
                    }

                    // Must unpin the pruned blocks if they haven't already been unpinned.
                    for (_, hash, pruned_block_parahead) in pruned_blocks {
                        if pruned_block_parahead.is_none() {
                            runtime_subscription
                                .relay_chain_subscribe_all
                                .unpin_block(&hash)
                                .await;
                        }
                    }

                    log::debug!(
                        target: &self.log_target,
                        "Subscriptions <= ParablockFinalized(hash={})",
                        HashDisplay(&hash)
                    );

                    let best_block_hash = runtime_subscription
                        .async_tree
                        .output_best_block_index()
                        .map(|(_, parahead)| {
                            header::hash_from_scale_encoded_header(parahead.as_ref().unwrap())
                        })
                        .unwrap_or(hash);
                    runtime_subscription.reported_best_parahead_hash = Some(best_block_hash);

                    // Elements in `all_subscriptions` are removed one by one and
                    // inserted back if the channel is still open.
                    for index in (0..runtime_subscription.all_subscriptions.len()).rev() {
                        let sender = runtime_subscription.all_subscriptions.swap_remove(index);
                        let notif = super::Notification::Finalized {
                            hash,
                            best_block_hash,
                        };
                        if sender.try_send(notif).is_ok() {
                            runtime_subscription.all_subscriptions.push(sender);
                        }
                    }
                }
                async_tree::OutputUpdate::Finalized { .. }
                | async_tree::OutputUpdate::BestBlockChanged { .. } => {
                    // Do not report anything to subscriptions if no finalized parahead is
                    // known yet.
                    let finalized_parahead = match runtime_subscription
                        .async_tree
                        .output_finalized_async_user_data()
                    {
                        Some(p) => p,
                        None => continue,
                    };

                    // Calculate hash of the parablock corresponding to the new best relay
                    // chain block.
                    let parahash = header::hash_from_scale_encoded_header(
                        runtime_subscription
                            .async_tree
                            .output_best_block_index()
                            .map(|(_, b)| b.as_ref().unwrap())
                            .unwrap_or(finalized_parahead),
                    );

                    if runtime_subscription.reported_best_parahead_hash.as_ref() != Some(&parahash)
                    {
                        runtime_subscription.reported_best_parahead_hash = Some(parahash);

                        // The networking service needs to be kept up to date with what the local
                        // node considers as the best block.
                        if let Ok(header) =
                            header::decode(finalized_parahead, self.block_number_bytes)
                        {
                            self.network_service
                                .set_local_best_block(
                                    self.network_chain_id,
                                    parahash,
                                    header.number,
                                )
                                .await;
                        }

                        log::debug!(
                            target: &self.log_target,
                            "Subscriptions <= BestBlockChanged(hash={})",
                            HashDisplay(&parahash)
                        );

                        // Elements in `all_subscriptions` are removed one by one and
                        // inserted back if the channel is still open.
                        for index in (0..runtime_subscription.all_subscriptions.len()).rev() {
                            let sender = runtime_subscription.all_subscriptions.swap_remove(index);
                            let notif = super::Notification::BestBlockChanged { hash: parahash };
                            if sender.try_send(notif).is_ok() {
                                runtime_subscription.all_subscriptions.push(sender);
                            }
                        }
                    }
                }
                async_tree::OutputUpdate::Block(block) => {
                    // `block` borrows `async_tree`. We need to mutably access `async_tree`
                    // below, so deconstruct `block` beforehand.
                    let is_new_best = block.is_new_best;
                    let scale_encoded_header: Vec<u8> = block.async_op_user_data.clone().unwrap();
                    let parahash = header::hash_from_scale_encoded_header(&scale_encoded_header);
                    let block_index = block.index;

                    // Do not report anything to subscriptions if no finalized parahead is
                    // known yet.
                    let finalized_parahead = match runtime_subscription
                        .async_tree
                        .output_finalized_async_user_data()
                    {
                        Some(p) => p,
                        None => continue,
                    };

                    // Do not report the new block if it has already been reported in the
                    // past. This covers situations where the parahead is identical to the
                    // relay chain's parent's parahead, but also situations where multiple
                    // sibling relay chain blocks have the same parahead.
                    if *finalized_parahead == scale_encoded_header
                        || runtime_subscription
                            .async_tree
                            .input_output_iter_unordered()
                            .filter(|item| item.id != block_index)
                            .filter_map(|item| item.async_op_user_data)
                            .any(|item| item.as_ref() == Some(&scale_encoded_header))
                    {
                        // While the parablock has already been reported, it is possible that
                        // it becomes the new best block while it wasn't before, in which
                        // case we should send a notification.
                        if is_new_best
                            && runtime_subscription.reported_best_parahead_hash.as_ref()
                                != Some(&parahash)
                        {
                            runtime_subscription.reported_best_parahead_hash = Some(parahash);

                            // The networking service needs to be kept up to date with what the
                            // local node considers as the best block.
                            if let Ok(header) =
                                header::decode(finalized_parahead, self.block_number_bytes)
                            {
                                self.network_service
                                    .set_local_best_block(
                                        self.network_chain_id,
                                        parahash,
                                        header.number,
                                    )
                                    .await;
                            }

                            log::debug!(
                                target: &self.log_target,
                                "Subscriptions <= BestBlockChanged(hash={})",
                                HashDisplay(&parahash)
                            );

                            // Elements in `all_subscriptions` are removed one by one and
                            // inserted back if the channel is still open.
                            for index in (0..runtime_subscription.all_subscriptions.len()).rev() {
                                let sender =
                                    runtime_subscription.all_subscriptions.swap_remove(index);
                                let notif =
                                    super::Notification::BestBlockChanged { hash: parahash };
                                if sender.try_send(notif).is_ok() {
                                    runtime_subscription.all_subscriptions.push(sender);
                                }
                            }
                        }

                        continue;
                    }

                    log::debug!(
                        target: &self.log_target,
                        "Subscriptions <= NewParablock(hash={})",
                        HashDisplay(&parahash)
                    );

                    if is_new_best {
                        runtime_subscription.reported_best_parahead_hash = Some(parahash);
                    }

                    let parent_hash = header::hash_from_scale_encoded_header(
                        runtime_subscription
                            .async_tree
                            .parent(block_index)
                            .map(|idx| {
                                runtime_subscription
                                    .async_tree
                                    .block_async_user_data(idx)
                                    .unwrap()
                                    .as_ref()
                                    .unwrap()
                            })
                            .unwrap_or(finalized_parahead),
                    );

                    // Elements in `all_subscriptions` are removed one by one and
                    // inserted back if the channel is still open.
                    for index in (0..runtime_subscription.all_subscriptions.len()).rev() {
                        let sender = runtime_subscription.all_subscriptions.swap_remove(index);
                        let notif = super::Notification::Block(super::BlockNotification {
                            is_new_best,
                            parent_hash,
                            scale_encoded_header: scale_encoded_header.clone(),
                        });
                        if sender.try_send(notif).is_ok() {
                            runtime_subscription.all_subscriptions.push(sender);
                        }
                    }
                }
            }
        }
    }

    fn process_relay_chain_notification(
        &mut self,
        relay_chain_notif: runtime_service::Notification,
    ) {
        let runtime_subscription = match &mut self.subscription_state {
            ParachainBackgroundState::NotSubscribed { .. } => return,
            ParachainBackgroundState::Subscribed(s) => s,
        };

        match relay_chain_notif {
            runtime_service::Notification::Finalized {
                hash,
                best_block_hash,
                ..
            } => {
                log::debug!(
                    target: &self.log_target,
                    "RelayChain => Finalized(hash={})",
                    HashDisplay(&hash)
                );

                let finalized = runtime_subscription
                    .async_tree
                    .input_output_iter_unordered()
                    .find(|b| *b.user_data == hash)
                    .unwrap()
                    .id;
                let best = runtime_subscription
                    .async_tree
                    .input_output_iter_unordered()
                    .find(|b| *b.user_data == best_block_hash)
                    .unwrap()
                    .id;
                runtime_subscription
                    .async_tree
                    .input_finalize(finalized, best);
            }
            runtime_service::Notification::Block(block) => {
                let hash = header::hash_from_scale_encoded_header(&block.scale_encoded_header);

                log::debug!(
                    target: &self.log_target,
                    "RelayChain => Block(hash={}, parent_hash={})",
                    HashDisplay(&hash),
                    HashDisplay(&block.parent_hash)
                );

                let parent = runtime_subscription
                    .async_tree
                    .input_output_iter_unordered()
                    .find(|b| *b.user_data == block.parent_hash)
                    .map(|b| b.id); // TODO: check if finalized
                runtime_subscription.async_tree.input_insert_block(
                    hash,
                    parent,
                    false,
                    block.is_new_best,
                );
            }
            runtime_service::Notification::BestBlockChanged { hash } => {
                log::debug!(
                    target: &self.log_target,
                    "RelayChain => BestBlockChanged(hash={})",
                    HashDisplay(&hash)
                );

                // If the block isn't found in `async_tree`, assume that it is equal to the
                // finalized block (that has left the tree already).
                let node_idx = runtime_subscription
                    .async_tree
                    .input_output_iter_unordered()
                    .find(|b| *b.user_data == hash)
                    .map(|b| b.id);
                runtime_subscription
                    .async_tree
                    .input_set_best_block(node_idx);
            }
        };
    }

    fn set_new_subscription(
        &mut self,
        relay_chain_subscribe_all: runtime_service::SubscribeAll<TPlat>,
    ) {
        // Subscription finished.
        log::debug!(
            target: &self.log_target,
            "RelayChain => NewSubscription(finalized_hash={})",
            HashDisplay(&header::hash_from_scale_encoded_header(
                &relay_chain_subscribe_all.finalized_block_scale_encoded_header
            ))
        );
        log::debug!(target: &self.log_target, "ParaheadFetchOperations <= Clear");

        let async_tree = {
            let mut async_tree =
                async_tree::AsyncTree::<TPlat::Instant, [u8; 32], _>::new(async_tree::Config {
                    finalized_async_user_data: None,
                    retry_after_failed: Duration::from_secs(5),
                    blocks_capacity: 32,
                });
            let finalized_hash = header::hash_from_scale_encoded_header(
                &relay_chain_subscribe_all.finalized_block_scale_encoded_header,
            );
            let finalized_index = async_tree.input_insert_block(finalized_hash, None, false, true);
            async_tree.input_finalize(finalized_index, finalized_index);
            for block in relay_chain_subscribe_all.non_finalized_blocks_ancestry_order {
                let hash = header::hash_from_scale_encoded_header(&block.scale_encoded_header);
                let parent = async_tree
                    .input_output_iter_unordered()
                    .find(|b| *b.user_data == block.parent_hash)
                    .map(|b| b.id)
                    .unwrap_or(finalized_index);
                async_tree.input_insert_block(hash, Some(parent), false, block.is_new_best);
            }
            async_tree
        };

        self.subscription_state =
            ParachainBackgroundState::Subscribed(ParachainBackgroundTaskAfterSubscription {
                all_subscriptions: match &mut self.subscription_state {
                    ParachainBackgroundState::NotSubscribed {
                        all_subscriptions, ..
                    } => mem::take(all_subscriptions),
                    _ => unreachable!(),
                },
                relay_chain_subscribe_all: relay_chain_subscribe_all.new_blocks,
                reported_best_parahead_hash: None,
                async_tree,
                in_progress_paraheads: stream::FuturesUnordered::new(),
                next_start_parahead_fetch: future::Either::Right(future::pending()),
            });
    }
}

async fn parahead<TPlat: PlatformRef>(
    relay_chain_sync: &Arc<runtime_service::RuntimeService<TPlat>>,
    relay_chain_block_number_bytes: usize,
    subscription_id: runtime_service::SubscriptionId,
    parachain_id: u32,
    block_hash: &[u8; 32],
) -> Result<Vec<u8>, ParaheadError> {
    // For each relay chain block, call `ParachainHost_persisted_validation_data` in
    // order to know where the parachains are.
    let precall = match relay_chain_sync
        .pinned_block_runtime_access(subscription_id, block_hash)
        .await
    {
        Ok(p) => p,
        Err(runtime_service::PinnedBlockRuntimeAccessError::ObsoleteSubscription) => {
            return Err(ParaheadError::ObsoleteSubscription)
        }
    };

    let (runtime_call_lock, virtual_machine) = precall
        .start(
            para::PERSISTED_VALIDATION_FUNCTION_NAME,
            para::persisted_validation_data_parameters(
                parachain_id,
                para::OccupiedCoreAssumption::TimedOut,
            ),
            6,
            Duration::from_secs(10),
            NonZeroU32::new(2).unwrap(),
        )
        .await
        .map_err(ParaheadError::Call)?;

    // TODO: move the logic below in the `para` module

    let mut runtime_call = match runtime_host::run(runtime_host::Config {
        virtual_machine,
        function_to_call: para::PERSISTED_VALIDATION_FUNCTION_NAME,
        parameter: para::persisted_validation_data_parameters(
            parachain_id,
            para::OccupiedCoreAssumption::TimedOut,
        ),
        max_log_level: 0,
        storage_main_trie_changes: Default::default(),
        calculate_trie_changes: false,
    }) {
        Ok(vm) => vm,
        Err((err, prototype)) => {
            runtime_call_lock.unlock(prototype);
            return Err(ParaheadError::StartError(err));
        }
    };

    let output = loop {
        match runtime_call {
            runtime_host::RuntimeHostVm::Finished(Ok(success)) => {
                let output = success.virtual_machine.value().as_ref().to_owned();
                runtime_call_lock.unlock(success.virtual_machine.into_prototype());
                break output;
            }
            runtime_host::RuntimeHostVm::Finished(Err(error)) => {
                runtime_call_lock.unlock(error.prototype);
                return Err(ParaheadError::Runtime(error.detail));
            }
            runtime_host::RuntimeHostVm::StorageGet(get) => {
                let storage_value = {
                    let child_trie = get.child_trie();
                    runtime_call_lock
                        .storage_entry(child_trie.as_ref().map(|c| c.as_ref()), get.key().as_ref())
                };
                let storage_value = match storage_value {
                    Ok(v) => v,
                    Err(err) => {
                        runtime_call_lock
                            .unlock(runtime_host::RuntimeHostVm::StorageGet(get).into_prototype());
                        return Err(ParaheadError::Call(err));
                    }
                };
                runtime_call =
                    get.inject_value(storage_value.map(|(val, ver)| (iter::once(val), ver)));
            }
            runtime_host::RuntimeHostVm::NextKey(nk) => {
                let next_key = {
                    let child_trie = nk.child_trie();
                    runtime_call_lock.next_key(
                        child_trie.as_ref().map(|c| c.as_ref()),
                        &nk.key().collect::<Vec<_>>(),
                        nk.or_equal(),
                        &nk.prefix().collect::<Vec<_>>(),
                        nk.branch_nodes(),
                    )
                };
                let next_key = match next_key {
                    Ok(v) => v,
                    Err(err) => {
                        runtime_call_lock
                            .unlock(runtime_host::RuntimeHostVm::NextKey(nk).into_prototype());
                        return Err(ParaheadError::Call(err));
                    }
                };
                runtime_call = nk.inject_key(next_key.map(|k| k.iter().copied()));
            }
            runtime_host::RuntimeHostVm::ClosestDescendantMerkleValue(mv) => {
                let merkle_value = {
                    let child_trie = mv.child_trie();
                    runtime_call_lock.closest_descendant_merkle_value(
                        child_trie.as_ref().map(|c| c.as_ref()),
                        &mv.key().collect::<Vec<_>>(),
                    )
                };
                let merkle_value = match merkle_value {
                    Ok(v) => v,
                    Err(err) => {
                        runtime_call_lock.unlock(
                            runtime_host::RuntimeHostVm::ClosestDescendantMerkleValue(mv)
                                .into_prototype(),
                        );
                        return Err(ParaheadError::Call(err));
                    }
                };
                runtime_call = mv.inject_merkle_value(merkle_value);
            }
            runtime_host::RuntimeHostVm::SignatureVerification(sig) => {
                runtime_call = sig.verify_and_resume();
            }
            runtime_host::RuntimeHostVm::OffchainStorageSet(req) => {
                // Do nothing.
                runtime_call = req.resume();
            }
            runtime_host::RuntimeHostVm::Offchain(req) => {
                runtime_call_lock
                    .unlock(runtime_host::RuntimeHostVm::Offchain(req).into_prototype());
                return Err(ParaheadError::OffchainWorkerHostFunction);
            }
        }
    };

    // Try decode the result of the runtime call.
    // If this fails, it indicates an incompatibility between smoldot and the relay chain.
    match para::decode_persisted_validation_data_return_value(
        &output,
        relay_chain_block_number_bytes,
    ) {
        Ok(Some(pvd)) => Ok(pvd.parent_head.to_vec()),
        Ok(None) => Err(ParaheadError::NoCore),
        Err(error) => Err(ParaheadError::InvalidRuntimeOutput(error)),
    }
}

/// Error that can happen when fetching the parachain head corresponding to a relay chain block.
#[derive(Debug, derive_more::Display)]
enum ParaheadError {
    /// Error while performing call request over the network.
    #[display(fmt = "Error while performing call request over the network: {_0}")]
    Call(runtime_service::RuntimeCallError),
    /// Error while starting virtual machine to verify call proof.
    #[display(fmt = "Error while starting virtual machine to verify call proof: {_0}")]
    StartError(host::StartErr),
    /// Error during the execution of the virtual machine to verify call proof.
    #[display(fmt = "Error during the call proof verification: {_0}")]
    Runtime(runtime_host::ErrorDetail),
    /// Parachain doesn't have a core in the relay chain.
    NoCore,
    /// Error while decoding the output of the call.
    ///
    /// This indicates some kind of incompatibility between smoldot and the relay chain.
    #[display(fmt = "Error while decoding the output of the call: {_0}")]
    InvalidRuntimeOutput(para::Error),
    /// Runtime has called an offchain worker host function.
    OffchainWorkerHostFunction,
    /// Runtime service subscription is no longer valid.
    ObsoleteSubscription,
}

impl ParaheadError {
    /// Returns `true` if this is caused by networking issues, as opposed to a consensus-related
    /// issue.
    fn is_network_problem(&self) -> bool {
        match self {
            ParaheadError::Call(err) => err.is_network_problem(),
            ParaheadError::StartError(_) => false,
            ParaheadError::Runtime(_) => false,
            ParaheadError::NoCore => false,
            ParaheadError::InvalidRuntimeOutput(_) => false,
            ParaheadError::OffchainWorkerHostFunction => false,
            ParaheadError::ObsoleteSubscription => false,
        }
    }
}
