use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bitcoin::{key::rand, p2p::{address::AddrV2, ServiceFlags}, Block, BlockHash, FeeRate, Network};
use bitcoin::p2p::message_filter::CFilter;
use rand::{rngs::StdRng, seq::IteratorRandom, SeedableRng};
use tokio::{
    sync::{
        mpsc::{self, Sender},
        Mutex,
    },
    task::JoinHandle,
};
use tokio::time::Instant;
use crate::{chain::HeightMonitor, db::{traits::PeerStore, PeerStatus, PersistedPeer}, network::{
    dns::DnsResolver,
    error::PeerError,
    peer::Peer,
    traits::{ClearNetConnection, NetworkConnector},
}, prelude::{default_port_from_network, Median, Netgroup}, ConnectionType, IndexedBlock, IndexedFilter, PeerStoreSizeConfig, TrustedPeer};
use crate::chain::block_queue::{DownloadQueue, DownloadRequest, QueueBatchKind};
use crate::chain::chain::AdvanceKind;
use crate::core::messages::DownloadKind;
use crate::filters::Filter;
use super::{
    channel_messages::{CombinedAddr, MainThreadMessage, PeerThreadMessage},
    dialog::Dialog,
    error::PeerManagerError,
    messages::Warning,
    PeerId, PeerTimeoutConfig,
};

const MAX_TRIES: usize = 50;
const MAX_BLOCKS_IN_TRANSIT_PER_PEER: usize = 32;

// We may be a little bit more aggressive compared to Bitcoin core here
// Since we apply this policy to all our peers but the timeout values are more generous
const BLOCK_STALLING_TIMEOUT_DEFAULT: Duration = Duration::from_secs(32);
const BLOCK_STALLING_TIMEOUT_MAX: Duration = Duration::from_secs(60 * 15);

// Preferred peers to connect to based on the user configuration
type Whitelist = Vec<TrustedPeer>;

#[derive(Debug)]
pub enum DownloadResponseKind {
    Block(Block),
    Filter(CFilter),
}

// A peer that is or was connected to the node
#[derive(Debug)]
pub(crate) struct ManagedPeer {
    net_time: i64,
    start_time: Instant,
    address: AddrV2,
    port: u16,
    service_flags: ServiceFlags,
    broadcast_min: FeeRate,
    ptx: Sender<MainThreadMessage>,
    handle: JoinHandle<Result<(), PeerError>>,
    pending_downloads: Vec<DownloadRequest>,
}

// The `PeerMap` manages connections with peers, adds and bans peers, and manages the peer database
#[derive(Debug)]
pub(crate) struct PeerMap<P: PeerStore> {
    current_id: PeerId,
    heights: Arc<Mutex<HeightMonitor>>,
    network: Network,
    mtx: Sender<PeerThreadMessage>,
    map: HashMap<PeerId, ManagedPeer>,
    db: Arc<Mutex<P>>,
    connector: Arc<Mutex<dyn NetworkConnector + Send + Sync>>,
    whitelist: Whitelist,
    dialog: Arc<Dialog>,
    target_db_size: PeerStoreSizeConfig,
    net_groups: HashSet<String>,
    timeout_config: PeerTimeoutConfig,
    dns_resolver: DnsResolver,
    adaptive_stall_timeout: Duration,
    download_queue: DownloadQueue,
    pub(crate) required_services: ServiceFlags,
    pub reset_peers: bool
}

impl ManagedPeer {
    fn stalled(&self, stall_timeout: Duration) -> bool {
        if self.pending_downloads.is_empty() {
            return false;
        }
        self.pending_downloads.iter()
            .take(1)
            .any(|req| req.downloading_since.is_some_and(|at| at.elapsed() > stall_timeout))
    }
}

#[allow(dead_code)]
impl<P: PeerStore> PeerMap<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mtx: Sender<PeerThreadMessage>,
        network: Network,
        db: P,
        whitelist: Whitelist,
        dialog: Arc<Dialog>,
        connection_type: ConnectionType,
        target_db_size: PeerStoreSizeConfig,
        timeout_config: PeerTimeoutConfig,
        height_monitor: Arc<Mutex<HeightMonitor>>,
        dns_resolver: DnsResolver,
    ) -> Self {
        let connector: Arc<Mutex<dyn NetworkConnector + Send + Sync>> = match connection_type {
            ConnectionType::ClearNet => Arc::new(Mutex::new(ClearNetConnection::new())),
            #[cfg(feature = "tor")]
            ConnectionType::Tor(client) => {
                use crate::network::tor::TorConnection;
                Arc::new(Mutex::new(TorConnection::new(client)))
            }
        };
        Self {
            current_id: PeerId(0),
            heights: height_monitor,
            network,
            mtx,
            map: HashMap::new(),
            db: Arc::new(Mutex::new(db)),
            connector,
            whitelist,
            dialog,
            target_db_size,
            net_groups: HashSet::new(),
            timeout_config,
            dns_resolver,
            adaptive_stall_timeout: BLOCK_STALLING_TIMEOUT_DEFAULT,
            download_queue: DownloadQueue::new(),
            required_services: ServiceFlags::NETWORK | ServiceFlags::WITNESS,
            reset_peers: false,
        }
    }

    pub fn queue_blocks(&mut self, reqs: Vec<DownloadRequest>) {
        for req in reqs {
            self.download_queue.add(req)
        }
    }

    pub fn cancel_blocks(&mut self, hashes: &[BlockHash]) {
        self.download_queue.remove(&hashes);
        self.map.iter_mut().for_each(|(_, m)| {
            m.pending_downloads.retain(|req| !hashes.contains(&req.hash))
        })
    }

    // Remove any finished connections & evict stalled peers
    pub async fn clean(&mut self) {
        // Return stalled requests to the queue
        let mut stalled_requests = Vec::new();

        let adaptive_stall_timeout = self.adaptive_stall_timeout;
        let flags = self.required_services;
        let dropped_peers: Vec<PeerId> = self.map.iter()
            .filter(|(_, peer)| peer.handle.is_finished() ||
                peer.stalled(adaptive_stall_timeout) ||
                !peer.service_flags.has(flags))
            .map(|(id, _)| *id)
            .collect();

        for peer_id in &dropped_peers {
            if let Some(peer) = self.map.remove(peer_id) {
                if !peer.handle.is_finished() {
                    self.adaptive_stall_timeout = std::cmp::min(
                        self.adaptive_stall_timeout.mul_f32(2.0),
                        BLOCK_STALLING_TIMEOUT_MAX,
                    );

                    _ = peer.ptx.send(MainThreadMessage::Disconnect).await;
                    peer.handle.abort();
                }
                stalled_requests.extend(peer.pending_downloads);
            }
        }
        self.queue_blocks(stalled_requests);

        let active = self.map.keys().copied().collect::<Vec<PeerId>>();
        let mut height_lock = self.heights.lock().await;
        height_lock.retain(&active);
    }

    // The number of peers with live connections
    pub fn live(&mut self) -> usize {
        self.map
            .values()
            .filter(|peer| !peer.handle.is_finished())
            .count()
    }

    // Get the median time adjustment for the currently connected peers
    pub fn median_time_adjustment(&self) -> i64 {
        let mut time_offsets: Vec<i64> = self.map.values().map(|peer| peer.net_time).collect();
        time_offsets.median()
    }

    // Set the time offset of a connected peer
    pub fn set_offset(&mut self, peer: PeerId, time: i64) {
        if let Some(peer) = self.map.get_mut(&peer) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time went backwards")
                .as_secs();
            peer.net_time = time - now as i64;
        }
    }

    // Set a new timeout duration
    pub fn set_duration(&mut self, duration: Duration) {
        self.timeout_config.response_timeout = duration;
    }

    // Add a new trusted peer to the whitelist
    pub fn add_trusted_peer(&mut self, peer: TrustedPeer) {
        self.whitelist.push(peer);
    }

    // Send out a TCP connection to a new peer and begin tracking the task
    pub async fn dispatch(&mut self, loaded_peer: PersistedPeer) -> Result<(), PeerError> {
        let (ptx, prx) = mpsc::channel::<MainThreadMessage>(32);
        self.current_id.increment();
        let mut peer = Peer::new(
            self.current_id,
            self.network,
            self.mtx.clone(),
            prx,
            loaded_peer.services,
            Arc::clone(&self.dialog),
            self.timeout_config,
        );
        let mut connector = self.connector.lock().await;
        if !connector.can_connect(&loaded_peer.addr) {
            return Err(PeerError::UnreachableSocketAddr);
        }
        crate::log!(
            self.dialog,
            format!("Connecting to {:?}:{}", loaded_peer.addr, loaded_peer.port)
        );
        let (reader, writer) = connector
            .connect(loaded_peer.addr.clone(), loaded_peer.port)
            .await?;

        let handle = tokio::spawn(async move {
            let res = peer.run(reader, writer).await;
            res
        });
        self.map.insert(
            self.current_id,
            ManagedPeer {
                service_flags: loaded_peer.services,
                address: loaded_peer.addr,
                port: loaded_peer.port,
                broadcast_min: FeeRate::BROADCAST_MIN,
                net_time: 0,
                ptx,
                handle,
                pending_downloads: vec![],
                start_time: Instant::now(),
            },
        );
        Ok(())
    }

    // Set the minimum fee rate this peer will accept
    pub fn set_broadcast_min(&mut self, nonce: PeerId, fee_rate: FeeRate) {
        if let Some(peer) = self.map.get_mut(&nonce) {
            peer.broadcast_min = fee_rate;
        }
    }

    // Set the services of a peer
    pub fn set_services(&mut self, nonce: PeerId, flags: ServiceFlags) {
        if let Some(peer) = self.map.get_mut(&nonce) {
            peer.service_flags = flags
        }
    }

    // Set the height of a peer upon receiving the version message
    pub async fn set_height(&mut self, nonce: PeerId, height: u32) {
        let mut height_lock = self.heights.lock().await;
        height_lock.insert(nonce, height);
    }

    // Add one to the height of a peer when receiving inventory
    pub async fn increment_height(&mut self, nonce: PeerId) {
        let mut height_lock = self.heights.lock().await;
        height_lock.increment(nonce);
    }

    // The minimum fee rate to successfully broadcast a transaction to all peers
    pub fn broadcast_min(&self) -> FeeRate {
        self.map
            .values()
            .map(|peer| peer.broadcast_min)
            .max()
            .unwrap_or(FeeRate::BROADCAST_MIN)
    }

    // Send a message to the specified peer
    pub async fn send_message(&mut self, nonce: PeerId, message: MainThreadMessage) {
        if let Some(peer) = self.map.get(&nonce) {
            let _ = peer.ptx.send(message).await;
        }
    }

    // Broadcast to all connected peers, returning if at least one peer received the message.
    pub async fn broadcast(&mut self, message: MainThreadMessage) -> bool {
        let active = self.map.values().filter(|peer| !peer.handle.is_finished());
        let mut sends = Vec::new();
        for peer in active {
            let res = peer.ptx.send(message.clone()).await;
            sends.push(res.is_ok());
        }
        sends.into_iter().any(|res| res)
    }

    pub async fn reset_peers(&mut self, filter_sync: bool) -> bool {
        let flags = match filter_sync {
            true => ServiceFlags::NETWORK | ServiceFlags::WITNESS | ServiceFlags::COMPACT_FILTERS,
            false => ServiceFlags::NETWORK | ServiceFlags::WITNESS
        };
        if flags != self.required_services {
            self.required_services = flags;
            self.reset_peers = true;
            self.dialog.send_dialog("Required services updated, adjusting peers").await;
            self.clean().await;
            return true;
        }
        false
    }

    pub async fn request_next_download_batch(&mut self)  {
        let adaptive_stall_time = self.adaptive_stall_timeout;
        let mut active_peers: Vec<_> = self.map.values_mut()
            .filter(|peer| !peer.handle.is_finished() && !peer.stalled(adaptive_stall_time))
            .filter(|peer| peer.start_time.elapsed() >= Duration::from_secs(10))
            .filter(|peer| peer.service_flags.has(self.required_services))
            .collect();

        // No active eligible peers
        if active_peers.is_empty() {
            return;
        }

        let mut resend = vec![];
        // Distribute blocks to peers, respecting MAX_BLOCKS_IN_TRANSIT_PER_PEER
        for peer in &mut active_peers {
            // Calculate how many more blocks this peer can handle
            let available_capacity = MAX_BLOCKS_IN_TRANSIT_PER_PEER
                .saturating_sub(peer.pending_downloads.len());
            // Skip if peer is already at capacity
            if available_capacity == 0 {
                continue;
            }

            let mut batch = match self.download_queue.pop(available_capacity) {
                None => return,
                Some(batch) => batch
            };

            let now = Instant::now();
            for block in  batch.requests.iter_mut() {
                block.downloading_since = Some(now);
            }

            crate::log!(self.dialog, format!("Requesting {} {} in a single message",
                batch.requests.len(), match &batch.kind {
                QueueBatchKind::Block(_) => "blocks",
                QueueBatchKind::Filter(_) => "filters"})
            );

            let thread_message = match batch.kind {
                QueueBatchKind::Block(blocks) => MainThreadMessage::GetBlock(blocks),
                QueueBatchKind::Filter(filters) => MainThreadMessage::GetFilters(filters)
            };

            match peer.ptx.send(thread_message).await {
                Ok(_) => peer.pending_downloads.extend(batch.requests),
                Err(_) => resend.extend(batch.requests),
            }
        }

        self.queue_blocks(resend)
    }

    // Send to a random peer, returning true if the message was sent.
    pub async fn send_random(&mut self, message: MainThreadMessage) -> Option<PeerId> {
        let mut rng = StdRng::from_entropy();
        if let Some((id, peer)) = self.map.iter().choose(&mut rng) {
            let res = peer.ptx.send(message).await;
            if res.is_ok() {
                return Some(id.clone());
            }
        }
        None
    }

    // Pull a peer from the configuration if we have one. If not, select a random peer from the database,
    // as long as it is not from the same netgroup. If there are no peers in the database, try DNS.
    pub async fn next_peer(&mut self) -> Result<PersistedPeer, PeerManagerError<P::Error>> {
        if let Some(peer) = self.whitelist.pop() {
            crate::log!(self.dialog, "Using a configured peer");
            let port = peer
                .port
                .unwrap_or(default_port_from_network(&self.network));
            let peer =
                PersistedPeer::new(peer.address, port, peer.known_services, PeerStatus::Tried);
            return Ok(peer);
        }
        let current_count = {
            let mut peer_manager = self.db.lock().await;
            peer_manager.num_unbanned().await?
        };
        if self.reset_peers || current_count < 1 {
            self.dialog.send_warning(Warning::EmptyPeerDatabase);
            self.bootstrap().await?;
        }
        let mut peer_manager = self.db.lock().await;
        let mut tries = 0;
        let desired_status = PeerStatus::random();
        while tries < MAX_TRIES {
            let peer = peer_manager.random().await?;
            if self.net_groups.contains(&peer.addr.netgroup())
                || desired_status.ne(&peer.status)
                || !peer.services.has(self.required_services)
            {
                tries += 1;
                continue;
            } else {
                return Ok(peer);
            }
        }
        peer_manager.random().await.map_err(From::from)
    }

    // Do we need peers
    pub async fn need_peers(&mut self) -> Result<bool, PeerManagerError<P::Error>> {
        match self.target_db_size {
            PeerStoreSizeConfig::Unbounded => Ok(true),
            PeerStoreSizeConfig::Limit(limit) => {
                let mut db = self.db.lock().await;
                let num_unbanned = db.num_unbanned().await?;
                Ok(num_unbanned < limit)
            }
        }
    }

    // Add peers to the database that were gossiped over the p2p network
    pub async fn add_gossiped_peers(&mut self, peers: Vec<CombinedAddr>) {
        let mut db = self.db.lock().await;
        for peer in peers {
            if let Err(e) = db
                .update(PersistedPeer::new(
                    peer.addr.clone(),
                    peer.port,
                    peer.services,
                    PeerStatus::Gossiped,
                ))
                .await
            {
                self.dialog.send_warning(Warning::FailedPersistence {
                    warning: format!(
                        "Encountered an error adding {:?}:{} flags: {} ... {e}",
                        peer.addr, peer.port, peer.services
                    ),
                });
            }
        }
    }

    // We tried this peer and successfully connected.
    pub async fn tried(&mut self, nonce: PeerId) {
        if let Some(peer) = self.map.get(&nonce) {
            let mut db = self.db.lock().await;
            if let Err(e) = db
                .update(PersistedPeer::new(
                    peer.address.clone(),
                    peer.port,
                    peer.service_flags,
                    PeerStatus::Tried,
                ))
                .await
            {
                self.dialog.send_warning(Warning::FailedPersistence {
                    warning: format!(
                        "Encountered an error adding {:?}:{} flags: {} ... {e}",
                        peer.address, peer.port, peer.service_flags
                    ),
                });
            }
        }
    }

    pub async fn receive_download(&mut self, nonce: PeerId, kind: DownloadResponseKind) -> Option<(u32, DownloadKind)> {
        let peer = match self.map.get_mut(&nonce) {
            Some(peer) => peer,
            None => return None,
        };
        self.adaptive_stall_timeout = std::cmp::max(
            self.adaptive_stall_timeout.div_f32(2.0),
            BLOCK_STALLING_TIMEOUT_DEFAULT,
        );

        match kind {
            DownloadResponseKind::Block(block) => {
                let block_hash = block.block_hash();
                let position = match peer.pending_downloads
                    .iter().position(|d|  matches!(d.kind, AdvanceKind::Blocks) &&
                    d.hash.eq(&block_hash)) {
                    None => return None,
                    Some(pos) => pos
                };
                let req = peer.pending_downloads.remove(position);
                let res = DownloadKind::Block(IndexedBlock {
                    height: req.height,
                    block,
                });

                let sender = match req.sender {
                    Some(sender) => sender,
                    None => return Some((req.height, res))
                };
                _ = sender.send(Ok(res));
                None
            }
            DownloadResponseKind::Filter(filter) => {
                // TODO: check filter hash
                let block_hash = filter.block_hash;
                let position = match peer.pending_downloads
                    .iter().position(|d|  matches!(d.kind, AdvanceKind::Filters) &&
                    d.hash.eq(&block_hash)) {
                    None => return None,
                    Some(pos) => pos
                };
                let req = peer.pending_downloads.remove(position);
                let res = DownloadKind::Filter(
                    IndexedFilter::new(req.height, Filter::new(filter.filter, block_hash))
                );
                let sender = match req.sender {
                    Some(sender) => sender,
                    None => return Some((req.height, res))
                };
                _ = sender.send(Ok(res));
                None
            }
        }
    }


    // This peer misbehaved in some way.
    pub async fn ban(&mut self, _nonce: PeerId) {
        // if let Some(peer) = self.map.get(&nonce) {
        //     let mut db = self.db.lock().await;
        //     if let Err(e) = db
        //         .update(PersistedPeer::new(
        //             peer.address.clone(),
        //             peer.port,
        //             peer.service_flags,
        //             PeerStatus::Ban,
        //         ))
        //         .await
        //     {
        //         self.dialog.send_warning(Warning::FailedPersistence {
        //             warning: format!(
        //                 "Encountered an error adding {:?}:{} flags: {} ... {e}",
        //                 peer.address, peer.port, peer.service_flags
        //             ),
        //         });
        //     }
        // }
    }

    async fn bootstrap(&mut self) -> Result<(), PeerManagerError<P::Error>> {
        use crate::network::dns::Dns;
        use std::net::IpAddr;
        crate::log!(self.dialog, "Bootstraping peers with DNS");
        let mut db_lock = self.db.lock().await;

        if self.reset_peers {
            db_lock.delete_all().await?;
            self.reset_peers = false;
        }

        let new_peers = Dns::new(self.network, self.dns_resolver)
            .bootstrap(self.required_services)
            .await
            .map_err(|_| PeerManagerError::Dns)?
            .into_iter()
            .map(|ip| match ip {
                IpAddr::V4(ip) => AddrV2::Ipv4(ip),
                IpAddr::V6(ip) => AddrV2::Ipv6(ip),
            })
            .collect::<Vec<AddrV2>>();
        crate::log!(
            self.dialog,
            format!("Adding {} sourced from DNS", new_peers.len())
        );
        // DNS fails if there is an insufficient number of peers
        for peer in new_peers {
            db_lock
                .update(PersistedPeer::new(
                    peer,
                    default_port_from_network(&self.network),
                    ServiceFlags::NONE,
                    PeerStatus::Gossiped,
                ))
                .await
                .map_err(PeerManagerError::Database)?;
        }
        Ok(())
    }
}
