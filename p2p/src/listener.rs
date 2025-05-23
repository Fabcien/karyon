use std::{future::Future, sync::Arc};

use log::{debug, error, info};

use karyon_core::{
    async_runtime::Executor,
    async_util::{TaskGroup, TaskResult},
    crypto::KeyPair,
};

use karyon_net::{tcp, tls, Endpoint};

use crate::{
    codec::NetMsgCodec,
    message::NetMsg,
    monitor::{ConnEvent, Monitor},
    slots::ConnectionSlots,
    tls_config::tls_server_config,
    ConnRef, Error, ListenerRef, Result,
};

/// Responsible for creating inbound connections with other peers.
pub struct Listener {
    /// Identity Key pair
    key_pair: KeyPair,

    /// Managing spawned tasks.
    task_group: TaskGroup,

    /// Manages available inbound slots.
    connection_slots: Arc<ConnectionSlots>,

    /// Enables secure connection.
    enable_tls: bool,

    /// Responsible for network and system monitoring.
    monitor: Arc<Monitor>,
}

impl Listener {
    /// Creates a new Listener
    pub fn new(
        key_pair: &KeyPair,
        connection_slots: Arc<ConnectionSlots>,
        enable_tls: bool,
        monitor: Arc<Monitor>,
        ex: Executor,
    ) -> Arc<Self> {
        Arc::new(Self {
            key_pair: key_pair.clone(),
            connection_slots,
            task_group: TaskGroup::with_executor(ex),
            enable_tls,
            monitor,
        })
    }

    /// Starts a listener on the given `endpoint`. For each incoming connection
    /// that is accepted, it invokes the provided `callback`, and pass the
    /// connection to the callback.
    ///
    /// Returns the resloved listening endpoint.
    pub async fn start<Fut>(
        self: &Arc<Self>,
        endpoint: Endpoint,
        // https://github.com/rust-lang/rfcs/pull/2132
        callback: impl FnOnce(ConnRef) -> Fut + Clone + Send + 'static,
    ) -> Result<Endpoint>
    where
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let listener = match self.listen(&endpoint).await {
            Ok(listener) => {
                self.monitor
                    .notify(ConnEvent::Listening(endpoint.clone()))
                    .await;
                listener
            }
            Err(err) => {
                error!("Failed to listen on {endpoint}: {err}");
                self.monitor.notify(ConnEvent::ListenFailed(endpoint)).await;
                return Err(err);
            }
        };

        let resolved_endpoint = listener.local_endpoint()?;

        info!("Start listening on {resolved_endpoint}");

        self.task_group.spawn(
            {
                let this = self.clone();
                async move { this.listen_loop(listener, callback).await }
            },
            |_| async {},
        );
        Ok(resolved_endpoint)
    }

    /// Shuts down the listener
    pub async fn shutdown(&self) {
        self.task_group.cancel().await;
    }

    async fn listen_loop<Fut>(
        self: Arc<Self>,
        listener: karyon_net::Listener<NetMsg, Error>,
        callback: impl FnOnce(ConnRef) -> Fut + Clone + Send + 'static,
    ) where
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        loop {
            // Wait for an available inbound slot.
            self.connection_slots.wait_for_slot().await;
            let result = listener.accept().await;

            let (conn, endpoint) = match result {
                Ok(c) => {
                    let endpoint = match c.peer_endpoint() {
                        Ok(ep) => ep,
                        Err(err) => {
                            self.monitor.notify(ConnEvent::AcceptFailed).await;
                            error!("Failed to accept a new connection: {err}");
                            continue;
                        }
                    };

                    self.monitor
                        .notify(ConnEvent::Accepted(endpoint.clone()))
                        .await;
                    (c, endpoint)
                }
                Err(err) => {
                    error!("Failed to accept a new connection: {err}");
                    self.monitor.notify(ConnEvent::AcceptFailed).await;
                    continue;
                }
            };

            self.connection_slots.add();

            let on_disconnect = {
                let this = self.clone();
                |res| async move {
                    if let TaskResult::Completed(Err(err)) = res {
                        debug!("Inbound connection dropped: {err}");
                    }
                    this.monitor.notify(ConnEvent::Disconnected(endpoint)).await;
                    this.connection_slots.remove().await;
                }
            };

            let callback = callback.clone();
            self.task_group.spawn(callback(conn), on_disconnect);
        }
    }

    async fn listen(&self, endpoint: &Endpoint) -> Result<ListenerRef> {
        if self.enable_tls {
            if !endpoint.is_tcp() && !endpoint.is_tls() {
                return Err(Error::UnsupportedEndpoint(endpoint.to_string()));
            }

            let tls_config = tls::ServerTlsConfig {
                tcp_config: Default::default(),
                server_config: tls_server_config(&self.key_pair)?,
            };
            let l = tls::listen(endpoint, tls_config, NetMsgCodec::new()).await?;
            Ok(Box::new(l))
        } else {
            if !endpoint.is_tcp() {
                return Err(Error::UnsupportedEndpoint(endpoint.to_string()));
            }

            let l = tcp::listen(endpoint, tcp::TcpConfig::default(), NetMsgCodec::new()).await?;
            Ok(Box::new(l))
        }
    }
}
