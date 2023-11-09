use std::{sync::Arc, time::Duration};

use futures_util::{stream::FuturesUnordered, StreamExt};
use log::{error, trace};
use rand::{rngs::OsRng, seq::SliceRandom, RngCore};
use smol::lock::{Mutex, RwLock};

use karyons_core::{async_utils::timeout, utils::decode, Executor};

use karyons_net::{Conn, Endpoint};

use crate::{
    io_codec::IOCodec,
    message::{
        get_msg_payload, FindPeerMsg, NetMsg, NetMsgCmd, PeerMsg, PeersMsg, PingMsg, PongMsg,
        ShutdownMsg,
    },
    monitor::{ConnEvent, DiscoveryEvent, Monitor},
    net::{ConnectionSlots, Connector, Listener},
    routing_table::RoutingTable,
    utils::version_match,
    Config, Error, PeerID, Result,
};

/// Maximum number of peers that can be returned in a PeersMsg.
pub const MAX_PEERS_IN_PEERSMSG: usize = 10;

pub struct LookupService {
    /// Peer's ID
    id: PeerID,

    /// Routing Table
    table: Arc<Mutex<RoutingTable>>,

    /// Listener
    listener: Arc<Listener>,
    /// Connector
    connector: Arc<Connector>,

    /// Outbound slots.
    outbound_slots: Arc<ConnectionSlots>,

    /// Resolved listen endpoint
    listen_endpoint: Option<RwLock<Endpoint>>,

    /// Holds the configuration for the P2P network.
    config: Arc<Config>,

    /// Responsible for network and system monitoring.
    monitor: Arc<Monitor>,
}

impl LookupService {
    /// Creates a new lookup service
    pub fn new(
        id: &PeerID,
        table: Arc<Mutex<RoutingTable>>,
        config: Arc<Config>,
        monitor: Arc<Monitor>,
    ) -> Self {
        let inbound_slots = Arc::new(ConnectionSlots::new(config.lookup_inbound_slots));
        let outbound_slots = Arc::new(ConnectionSlots::new(config.lookup_outbound_slots));

        let listener = Listener::new(inbound_slots.clone(), monitor.clone());
        let connector = Connector::new(
            config.lookup_connect_retries,
            outbound_slots.clone(),
            monitor.clone(),
        );

        let listen_endpoint = config
            .listen_endpoint
            .as_ref()
            .map(|endpoint| RwLock::new(endpoint.clone()));

        Self {
            id: id.clone(),
            table,
            listener,
            connector,
            outbound_slots,
            listen_endpoint,
            config,
            monitor,
        }
    }

    /// Start the lookup service.
    pub async fn start(self: &Arc<Self>, ex: Executor<'_>) -> Result<()> {
        self.start_listener(ex).await?;
        Ok(())
    }

    /// Set the resolved listen endpoint.
    pub async fn set_listen_endpoint(&self, resolved_endpoint: &Endpoint) {
        if let Some(endpoint) = &self.listen_endpoint {
            *endpoint.write().await = resolved_endpoint.clone();
        }
    }

    /// Shuts down the lookup service.
    pub async fn shutdown(&self) {
        self.connector.shutdown().await;
        self.listener.shutdown().await;
    }

    /// Starts iterative lookup and populate the routing table.
    ///
    /// This method begins by generating a random peer ID and connecting to the
    /// provided endpoint. It then sends a FindPeer message containing the
    /// randomly generated peer ID. Upon receiving peers from the initial lookup,
    /// it starts connecting to these received peers and sends them a FindPeer
    /// message that contains our own peer ID.
    pub async fn start_lookup(&self, endpoint: &Endpoint) -> Result<()> {
        trace!("Lookup started {endpoint}");
        self.monitor
            .notify(&DiscoveryEvent::LookupStarted(endpoint.clone()).into())
            .await;

        let mut random_peers = vec![];
        if let Err(err) = self.random_lookup(endpoint, &mut random_peers).await {
            self.monitor
                .notify(&DiscoveryEvent::LookupFailed(endpoint.clone()).into())
                .await;
            return Err(err);
        };

        let mut peer_buffer = vec![];
        self.self_lookup(&random_peers, &mut peer_buffer).await;

        while peer_buffer.len() < MAX_PEERS_IN_PEERSMSG {
            match random_peers.pop() {
                Some(p) => peer_buffer.push(p),
                None => break,
            }
        }

        for peer in peer_buffer.iter() {
            let mut table = self.table.lock().await;
            let result = table.add_entry(peer.clone().into());
            trace!("Add entry {:?}", result);
        }

        self.monitor
            .notify(&DiscoveryEvent::LookupSucceeded(endpoint.clone(), peer_buffer.len()).into())
            .await;

        Ok(())
    }

    /// Starts a random lookup
    ///
    /// This will perfom lookup on a random generated PeerID
    async fn random_lookup(
        &self,
        endpoint: &Endpoint,
        random_peers: &mut Vec<PeerMsg>,
    ) -> Result<()> {
        for _ in 0..2 {
            let peer_id = PeerID::random();
            let peers = self.connect(&peer_id, endpoint.clone()).await?;
            for peer in peers {
                if random_peers.contains(&peer)
                    || peer.peer_id == self.id
                    || self.table.lock().await.contains_key(&peer.peer_id.0)
                {
                    continue;
                }

                random_peers.push(peer);
            }
        }

        Ok(())
    }

    /// Starts a self lookup
    async fn self_lookup(&self, random_peers: &Vec<PeerMsg>, peer_buffer: &mut Vec<PeerMsg>) {
        let mut tasks = FuturesUnordered::new();
        for peer in random_peers.choose_multiple(&mut OsRng, random_peers.len()) {
            let endpoint = Endpoint::Tcp(peer.addr.clone(), peer.discovery_port);
            tasks.push(self.connect(&self.id, endpoint))
        }

        while let Some(result) = tasks.next().await {
            match result {
                Ok(peers) => peer_buffer.extend(peers),
                Err(err) => {
                    error!("Failed to do self lookup: {err}");
                }
            }
        }
    }

    /// Connects to the given endpoint
    async fn connect(&self, peer_id: &PeerID, endpoint: Endpoint) -> Result<Vec<PeerMsg>> {
        let conn = self.connector.connect(&endpoint).await?;
        let io_codec = IOCodec::new(conn);
        let result = self.handle_outbound(io_codec, peer_id).await;

        self.monitor
            .notify(&ConnEvent::Disconnected(endpoint).into())
            .await;
        self.outbound_slots.remove().await;

        result
    }

    /// Handles outbound connection
    async fn handle_outbound(&self, io_codec: IOCodec, peer_id: &PeerID) -> Result<Vec<PeerMsg>> {
        trace!("Send Ping msg");
        self.send_ping_msg(&io_codec).await?;

        trace!("Send FindPeer msg");
        let peers = self.send_findpeer_msg(&io_codec, peer_id).await?;

        if peers.0.len() >= MAX_PEERS_IN_PEERSMSG {
            return Err(Error::Lookup("Received too many peers in PeersMsg"));
        }

        trace!("Send Peer msg");
        if let Some(endpoint) = &self.listen_endpoint {
            self.send_peer_msg(&io_codec, endpoint.read().await.clone())
                .await?;
        }

        trace!("Send Shutdown msg");
        self.send_shutdown_msg(&io_codec).await?;

        Ok(peers.0)
    }

    /// Start a listener.
    async fn start_listener(self: &Arc<Self>, ex: Executor<'_>) -> Result<()> {
        let addr = match &self.listen_endpoint {
            Some(a) => a.read().await.addr()?.clone(),
            None => return Ok(()),
        };

        let endpoint = Endpoint::Tcp(addr, self.config.discovery_port);

        let selfc = self.clone();
        let callback = |conn: Conn| async move {
            let t = Duration::from_secs(selfc.config.lookup_connection_lifespan);
            timeout(t, selfc.handle_inbound(conn)).await??;
            Ok(())
        };

        self.listener.start(ex, endpoint.clone(), callback).await?;
        Ok(())
    }

    /// Handles inbound connection
    async fn handle_inbound(self: &Arc<Self>, conn: Conn) -> Result<()> {
        let io_codec = IOCodec::new(conn);
        loop {
            let msg: NetMsg = io_codec.read().await?;
            trace!("Receive msg {:?}", msg.header.command);

            if let NetMsgCmd::Shutdown = msg.header.command {
                return Ok(());
            }

            match &msg.header.command {
                NetMsgCmd::Ping => {
                    let (ping_msg, _) = decode::<PingMsg>(&msg.payload)?;
                    if !version_match(&self.config.version.req, &ping_msg.version) {
                        return Err(Error::IncompatibleVersion("system: {}".into()));
                    }
                    self.send_pong_msg(ping_msg.nonce, &io_codec).await?;
                }
                NetMsgCmd::FindPeer => {
                    let (findpeer_msg, _) = decode::<FindPeerMsg>(&msg.payload)?;
                    let peer_id = findpeer_msg.0;
                    self.send_peers_msg(&peer_id, &io_codec).await?;
                }
                NetMsgCmd::Peer => {
                    let (peer, _) = decode::<PeerMsg>(&msg.payload)?;
                    let result = self.table.lock().await.add_entry(peer.clone().into());
                    trace!("Add entry result: {:?}", result);
                }
                c => return Err(Error::InvalidMsg(format!("Unexpected msg: {:?}", c))),
            }
        }
    }

    /// Sends a Ping msg and wait to receive the Pong message.
    async fn send_ping_msg(&self, io_codec: &IOCodec) -> Result<()> {
        trace!("Send Pong msg");

        let mut nonce: [u8; 32] = [0; 32];
        RngCore::fill_bytes(&mut OsRng, &mut nonce);

        let ping_msg = PingMsg {
            version: self.config.version.v.clone(),
            nonce,
        };
        io_codec.write(NetMsgCmd::Ping, &ping_msg).await?;

        let t = Duration::from_secs(self.config.lookup_response_timeout);
        let recv_msg: NetMsg = io_codec.read_timeout(t).await?;

        let payload = get_msg_payload!(Pong, recv_msg);
        let (pong_msg, _) = decode::<PongMsg>(&payload)?;

        if ping_msg.nonce != pong_msg.0 {
            return Err(Error::InvalidPongMsg);
        }

        Ok(())
    }

    /// Sends a Pong msg
    async fn send_pong_msg(&self, nonce: [u8; 32], io_codec: &IOCodec) -> Result<()> {
        trace!("Send Pong msg");
        io_codec.write(NetMsgCmd::Pong, &PongMsg(nonce)).await?;
        Ok(())
    }

    /// Sends a FindPeer msg and wait to receivet the Peers msg.
    async fn send_findpeer_msg(&self, io_codec: &IOCodec, peer_id: &PeerID) -> Result<PeersMsg> {
        trace!("Send FindPeer msg");
        io_codec
            .write(NetMsgCmd::FindPeer, &FindPeerMsg(peer_id.clone()))
            .await?;

        let t = Duration::from_secs(self.config.lookup_response_timeout);
        let recv_msg: NetMsg = io_codec.read_timeout(t).await?;

        let payload = get_msg_payload!(Peers, recv_msg);
        let (peers, _) = decode(&payload)?;

        Ok(peers)
    }

    /// Sends a Peers msg.
    async fn send_peers_msg(&self, peer_id: &PeerID, io_codec: &IOCodec) -> Result<()> {
        trace!("Send Peers msg");
        let table = self.table.lock().await;
        let entries = table.closest_entries(&peer_id.0, MAX_PEERS_IN_PEERSMSG);
        let peers: Vec<PeerMsg> = entries.into_iter().map(|e| e.into()).collect();
        drop(table);
        io_codec.write(NetMsgCmd::Peers, &PeersMsg(peers)).await?;
        Ok(())
    }

    /// Sends a Peer msg.
    async fn send_peer_msg(&self, io_codec: &IOCodec, endpoint: Endpoint) -> Result<()> {
        trace!("Send Peer msg");
        let peer_msg = PeerMsg {
            addr: endpoint.addr()?.clone(),
            port: *endpoint.port()?,
            discovery_port: self.config.discovery_port,
            peer_id: self.id.clone(),
        };
        io_codec.write(NetMsgCmd::Peer, &peer_msg).await?;
        Ok(())
    }

    /// Sends a Shutdown msg.
    async fn send_shutdown_msg(&self, io_codec: &IOCodec) -> Result<()> {
        trace!("Send Shutdown msg");
        io_codec.write(NetMsgCmd::Shutdown, &ShutdownMsg(0)).await?;
        Ok(())
    }
}