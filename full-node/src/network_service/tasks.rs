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

use crate::{LogCallback, LogLevel};
use core::future::Future;
use futures_lite::future;
use futures_util::StreamExt as _;
use smol::{
    channel,
    future::FutureExt as _,
    io::{AsyncRead, AsyncWrite},
};
use smoldot::{
    libp2p::{
        multiaddr::{Multiaddr, ProtocolRef},
        websocket, with_buffers,
    },
    network::service::{self, CoordinatorToConnection},
};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    pin,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite {}

/// Asynchronous task managing a specific connection.
pub(super) async fn connection_task(
    log_callback: Arc<dyn LogCallback + Send + Sync>,
    address: String,
    socket: impl Future<Output = Result<impl AsyncReadWrite, io::Error>>,
    connection_id: service::ConnectionId,
    mut connection_task: service::SingleStreamConnectionTask<Instant>,
    mut coordinator_to_connection: channel::Receiver<service::CoordinatorToConnection>,
    connection_to_coordinator: channel::Sender<super::ToBackground>,
) {
    // Finishing ongoing connection process.
    let socket = match socket.await.map_err(|_| ()) {
        Ok(s) => s,
        Err(_err) => {
            // TODO: log
            connection_task.reset();
            loop {
                let (task_update, opaque_message) = connection_task.pull_message_to_coordinator();
                let _ = connection_to_coordinator
                    .send(super::ToBackground::FromConnectionTask {
                        connection_id,
                        opaque_message,
                        connection_now_dead: true,
                    })
                    .await;
                if let Some(task_update) = task_update {
                    connection_task = task_update;
                } else {
                    return;
                }
            }
        }
    };

    // The socket is wrapped around an object containing a read buffer and a write buffer and
    // allowing easier usage.
    let mut socket = pin::pin!(with_buffers::WithBuffers::new(socket));

    // Future that sends a message to the coordinator. Only one message is sent to the coordinator
    // at a time. `None` if no message is being sent.
    let mut message_sending = None;

    loop {
        // Because only one message should be sent to the coordinator at a time, and that
        // processing the socket might generate a message, we only process the socket if no
        // message is currently being sent.
        if message_sending.is_none() {
            if let Ok(mut socket_read_write) = socket.as_mut().read_write_access(Instant::now()) {
                let read_bytes_before = socket_read_write.read_bytes;
                let written_bytes_before = socket_read_write.write_bytes_queued;
                let write_closed = socket_read_write.write_bytes_queueable.is_none();

                connection_task.read_write(&mut *socket_read_write);

                if socket_read_write.read_bytes != read_bytes_before
                    || socket_read_write.write_bytes_queued != written_bytes_before
                    || (!write_closed && socket_read_write.write_bytes_queueable.is_none())
                {
                    log_callback.log(
                        LogLevel::Trace,
                        format!(
                            "connection-activity; address={address}; read={}; written={}; wake_up_after={:?}; write_close={:?}",
                            socket_read_write.read_bytes - read_bytes_before,
                            socket_read_write.write_bytes_queued - written_bytes_before,
                            socket_read_write.wake_up_after.map(|w| w
                                .checked_duration_since(socket_read_write.now)
                                .unwrap_or(Duration::new(0, 0))),
                            socket_read_write.write_bytes_queueable.is_none(),
                        ),
                    );
                }
            } else {
                // Error on the socket.
                if !connection_task.is_reset_called() {
                    log_callback.log(
                        LogLevel::Trace,
                        format!("connection-activity; address={}; reset", address),
                    );
                    connection_task.reset();
                }
            }

            // Try pull message to send to the coordinator.

            // Calling this method takes ownership of the task and returns that task if it has
            // more work to do. If `None` is returned, then the entire task is gone and the
            // connection must be abruptly closed, which is what happens when we return from
            // this function.
            let (task_update, opaque_message) = connection_task.pull_message_to_coordinator();
            if let Some(task_update) = task_update {
                connection_task = task_update;
                debug_assert!(message_sending.is_none());
                if let Some(opaque_message) = opaque_message {
                    message_sending = Some(connection_to_coordinator.send(
                        super::ToBackground::FromConnectionTask {
                            connection_id,
                            opaque_message: Some(opaque_message),
                            connection_now_dead: false,
                        },
                    ));
                }
            } else {
                let _ = connection_to_coordinator
                    .send(super::ToBackground::FromConnectionTask {
                        connection_id,
                        opaque_message,
                        connection_now_dead: true,
                    })
                    .await;
                return;
            }
        }

        // Now wait for something interesting to happen before looping again.

        enum WhatHappened {
            CoordinatorMessage(CoordinatorToConnection),
            CoordinatorDead,
            SocketEvent,
            MessageSent,
        }

        let what_happened: WhatHappened = {
            let coordinator_message = async {
                match coordinator_to_connection.next().await {
                    Some(msg) => WhatHappened::CoordinatorMessage(msg),
                    None => WhatHappened::CoordinatorDead,
                }
            };

            let socket_event = {
                // The future returned by `wait_read_write_again` yields when `read_write_access`
                // must be called. Because we only call `read_write_access` when `message_sending`
                // is `None`, we also call `wait_read_write_again` only when `message_sending` is
                // `None`.
                let fut = if message_sending.is_none() {
                    Some(socket.as_mut().wait_read_write_again(|when| async move {
                        smol::Timer::at(when).await;
                    }))
                } else {
                    None
                };
                async {
                    if let Some(fut) = fut {
                        fut.await;
                        WhatHappened::SocketEvent
                    } else {
                        future::pending().await
                    }
                }
            };

            let message_sent = async {
                let result = if let Some(message_sending) = message_sending.as_mut() {
                    message_sending.await
                } else {
                    future::pending().await
                };
                message_sending = None;
                if result.is_ok() {
                    WhatHappened::MessageSent
                } else {
                    WhatHappened::CoordinatorDead
                }
            };

            coordinator_message.or(socket_event).or(message_sent).await
        };

        match what_happened {
            WhatHappened::CoordinatorMessage(message) => {
                connection_task.inject_coordinator_message(&Instant::now(), message);
            }
            WhatHappened::CoordinatorDead => return,
            WhatHappened::SocketEvent => {}
            WhatHappened::MessageSent => {}
        }
    }
}

/// Builds a future that connects to the given multiaddress. Returns an error if the multiaddress
/// protocols aren't supported.
pub(super) fn multiaddr_to_socket(
    addr: &Multiaddr,
) -> Result<impl Future<Output = Result<impl AsyncReadWrite, io::Error>>, ()> {
    let mut iter = addr.iter().fuse();
    let proto1 = iter.next().ok_or(())?;
    let proto2 = iter.next().ok_or(())?;
    let proto3 = iter.next();

    if iter.next().is_some() {
        return Err(());
    }

    // TODO: doesn't support WebSocket secure connections

    // Ensure ahead of time that the multiaddress is supported.
    let (addr, host_if_websocket) = match (&proto1, &proto2, &proto3) {
        (ProtocolRef::Ip4(ip), ProtocolRef::Tcp(port), None) => (
            either::Left(SocketAddr::new(IpAddr::V4((*ip).into()), *port)),
            None,
        ),
        (ProtocolRef::Ip6(ip), ProtocolRef::Tcp(port), None) => (
            either::Left(SocketAddr::new(IpAddr::V6((*ip).into()), *port)),
            None,
        ),
        (ProtocolRef::Ip4(ip), ProtocolRef::Tcp(port), Some(ProtocolRef::Ws)) => {
            let addr = SocketAddr::new(IpAddr::V4((*ip).into()), *port);
            (either::Left(addr), Some(addr.to_string()))
        }
        (ProtocolRef::Ip6(ip), ProtocolRef::Tcp(port), Some(ProtocolRef::Ws)) => {
            let addr = SocketAddr::new(IpAddr::V6((*ip).into()), *port);
            (either::Left(addr), Some(addr.to_string()))
        }

        // TODO: we don't care about the differences between Dns, Dns4, and Dns6
        (
            ProtocolRef::Dns(addr) | ProtocolRef::Dns4(addr) | ProtocolRef::Dns6(addr),
            ProtocolRef::Tcp(port),
            None,
        ) => (either::Right((addr.to_string(), *port)), None),
        (
            ProtocolRef::Dns(addr) | ProtocolRef::Dns4(addr) | ProtocolRef::Dns6(addr),
            ProtocolRef::Tcp(port),
            Some(ProtocolRef::Ws),
        ) => (
            either::Right((addr.to_string(), *port)),
            Some(format!("{}:{}", addr, *port)),
        ),

        _ => return Err(()),
    };

    Ok(async move {
        let tcp_socket = match addr {
            either::Left(socket_addr) => smol::net::TcpStream::connect(socket_addr).await,
            either::Right((dns, port)) => smol::net::TcpStream::connect((&dns[..], port)).await,
        };

        if let Ok(tcp_socket) = &tcp_socket {
            // The Nagle algorithm, implemented in the kernel, consists in buffering the
            // data to be sent out and waiting a bit before actually sending it out, in
            // order to potentially merge multiple writes in a row into one packet. In
            // the implementation below, it is guaranteed that the buffer in `WithBuffers`
            // is filled with as much data as possible before the operating system gets
            // involved. As such, we disable the Nagle algorithm, in order to avoid adding
            // an artificial delay to all sends.
            let _ = tcp_socket.set_nodelay(true);
        }

        match (tcp_socket, host_if_websocket) {
            (Ok(tcp_socket), Some(host)) => {
                websocket::websocket_client_handshake(websocket::Config {
                    tcp_socket,
                    host: &host,
                    url: "/",
                })
                .await
                .map(futures_util::future::Either::Right)
            }
            (Ok(tcp_socket), None) => Ok(futures_util::future::Either::Left(tcp_socket)),
            (Err(err), _) => Err(err),
        }
    })
}
