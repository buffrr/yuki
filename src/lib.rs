//! Kyoto is a conservative, private, and vetted Bitcoin client built in accordance
//! with the [BIP157](https://github.com/bitcoin/bips/blob/master/bip-0157.mediawiki) and [BIP158](https://github.com/bitcoin/bips/blob/master/bip-0158.mediawiki)
//! standards. _Conservative_, as in Kyoto makes very little assumptions about the underlying memory requirements of the
//! device running the software. _Private_, as in the Bitcoin nodes that serve Kyoto nodes data do not know what transactions the
//! client is querying for, only the entire Bitcoin block. _Vetted_, as in the dependencies of the core library are meant to remain limited,
//! rigorously tested, and absolutely necessary.
//!
//! # Example usage
//!
//! ```no_run
//! use std::str::FromStr;
//! use std::collections::HashSet;
//! use kyoto::{NodeBuilder, Log, Event, Client, Address, Network, HeaderCheckpoint, BlockHash};
//!
//! #[tokio::main]
//! async fn main() {
//!     // Add third-party logging
//!     let subscriber = tracing_subscriber::FmtSubscriber::new();
//!     tracing::subscriber::set_global_default(subscriber).unwrap();
//!     // Add Bitcoin scripts to scan the blockchain for
//!     let address = Address::from_str("tb1q9pvjqz5u5sdgpatg3wn0ce438u5cyv85lly0pc")
//!         .unwrap()
//!         .require_network(Network::Signet)
//!         .unwrap()
//!         .into();
//!     let mut addresses = HashSet::new();
//!     addresses.insert(address);
//!     // Start the scan after a specified header
//!     let checkpoint = HeaderCheckpoint::closest_checkpoint_below_height(170_000, Network::Signet);
//!     // Create a new node builder
//!     let builder = NodeBuilder::new(Network::Signet);
//!     // Add node preferences and build the node/client
//!     let (node, client) = builder
//!         // The Bitcoin scripts to monitor
//!         .add_scripts(addresses)
//!         // Only scan blocks strictly after an anchor checkpoint
//!         .anchor_checkpoint(checkpoint)
//!         // The number of connections we would like to maintain
//!         .required_peers(2)
//!         .build()
//!         .unwrap();
//!     // Run the node and wait for the sync message;
//!     tokio::task::spawn(async move { node.run().await });
//!     // Split the client into components that send messages and listen to messages
//!     let Client { requester, mut log_rx, warn_rx: _, mut event_rx } = client;
//!     // Sync with the single script added
//!     loop {
//!         tokio::select! {
//!             log = log_rx.recv() => {
//!                 if let Some(log) = log {
//!                     match log {
//!                         Log::Debug(d) => tracing::info!("{d}"),
//!                         _ => (),
//!                     }
//!                 }
//!             }
//!             event = event_rx.recv() => {
//!                 if let Some(event) = event {
//!                     match event {
//!                         Event::Synced(_) => {
//!                             tracing::info!("Sync complete!");
//!                             break;
//!                         },
//!                         _ => (),
//!                     }
//!                 }
//!             }
//!         }
//!     }
//!     requester.shutdown().await;
//! }
//! ```
//!
//! # Getting started
//!
//! The [`core`] module documentation is likely the best place to start when developing an application with Kyoto.
//!
//! # Features
//!
//! `database`: use the default `rusqlite` database implementations. Default and recommend feature.
//!
//! `filter-control`: check filters and request blocks directly. Recommended for silent payments or strict chain ordering implementations.
//!
//! `tor` *No MSRV guarantees*: connect to nodes over the Tor network.

pub mod chain;
pub mod core;
pub mod db;

mod filters;
mod network;
mod prelude;
pub mod rpc;
pub mod app;

use filters::Filter;

use std::net::{IpAddr, SocketAddr};

#[cfg(feature = "tor")]
pub use arti_client::{TorClient, TorClientConfig};
#[cfg(feature = "tor")]
use tor_rtcompat::PreferredRuntime;

// Re-exports
#[doc(inline)]
pub use chain::checkpoints::{
    HeaderCheckpoint, MAINNET_HEADER_CP, SIGNET_HEADER_CP, TESTNET4_HEADER_CP,
};

#[cfg(feature = "database")]
#[doc(inline)]
pub use db::sqlite::{headers::SqliteHeaderDb, peers::SqlitePeerDb};

#[doc(inline)]
pub use db::traits::{HeaderStore, PeerStore};

#[doc(inline)]
pub use tokio::sync::mpsc::Receiver;
#[doc(inline)]
pub use tokio::sync::mpsc::UnboundedReceiver;

#[doc(inline)]
pub use {
    crate::core::builder::NodeBuilder,
    crate::core::client::{Client, Requester},
    crate::core::error::{ClientError, NodeError},
    crate::core::messages::{Event, Log, Progress, RejectPayload, SyncUpdate, Warning},
    crate::core::node::{Node, NodeState},
};

#[doc(inline)]
pub use bitcoin::{
    block::Header, p2p::address::AddrV2, p2p::message_network::RejectReason, p2p::ServiceFlags,
    Address, Block, BlockHash, FeeRate, Network, ScriptBuf, Transaction, Txid,
};
use bitcoin::Work;

pub extern crate tokio;

/// A Bitcoin [`Transaction`] with additional context.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct IndexedTransaction {
    /// The Bitcoin transaction.
    pub transaction: Transaction,
    /// The height of the block in the chain that includes this transaction.
    pub height: u32,
    /// The hash of the block.
    pub hash: BlockHash,
}

#[allow(dead_code)]
impl IndexedTransaction {
    pub(crate) fn new(transaction: Transaction, height: u32, hash: BlockHash) -> Self {
        Self {
            transaction,
            height,
            hash,
        }
    }
}

/// A block [`Header`] that was disconnected from the chain of most work along with its previous height.
#[derive(Debug, Clone, Copy)]
pub struct DisconnectedHeader {
    /// The height where this header used to be in the chain.
    pub height: u32,
    /// The reorganized header.
    pub header: Header,
}

impl DisconnectedHeader {
    pub(crate) fn new(height: u32, header: Header) -> Self {
        Self { height, header }
    }
}

/// A Bitcoin [`Block`] with associated height.
#[derive(Debug, Clone)]
pub struct IndexedBlock {
    /// The height or index in the chain.
    pub height: u32,
    /// The Bitcoin block with some matching script.
    pub block: Block,
}

#[cfg(feature = "filter-control")]
/// A compact block filter with associated height.
#[derive(Debug, Clone)]
pub struct IndexedFilter {
    height: u32,
    filter: Filter,
}

#[cfg(feature = "filter-control")]
impl IndexedFilter {
    fn new(height: u32, filter: Filter) -> Self {
        Self { height, filter }
    }

    /// The height in the chain.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Return the [`BlockHash`] associated with this filer
    pub fn block_hash(&self) -> &BlockHash {
        self.filter.block_hash()
    }

    /// Does the filter contain a positive match for any of the provided scripts
    pub fn contains_any<'a>(&'a mut self, scripts: impl Iterator<Item = &'a ScriptBuf>) -> bool {
        self.filter
            .contains_any(scripts)
            .expect("vec reader is infallible")
    }
}

/// Broadcast a [`Transaction`] to a set of connected peers.
#[derive(Debug, Clone)]
pub struct TxBroadcast {
    /// The presumably valid Bitcoin transaction.
    pub tx: Transaction,
    /// The strategy for how this transaction should be shared with the network.
    pub broadcast_policy: TxBroadcastPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockchainInfo {
    pub chain: String,
    pub blocks: u32,
    pub headers: u32,
    #[serde(rename = "bestblockhash")]
    pub best_blockhash: Option<BlockHash>,
    #[serde(rename = "mediantime")]
    pub median_time: i64,
    #[serde(rename = "chainwork")]
    pub chain_work: Work,
    #[serde(rename = "pruneheight")]
    pub prune_height: Option<u32>,
    pub pruned: bool,
    pub filters: u32,
    #[serde(rename = "filterheaders")]
    pub filter_headers: u32,
    #[serde(rename = "blockqueue")]
    pub block_queue: QueueBlocksStatus,
    pub checkpoint: HeaderCheckpoint,
}

impl TxBroadcast {
    /// Prepare a transaction for broadcast with associated broadcast strategy.
    pub fn new(tx: Transaction, broadcast_policy: TxBroadcastPolicy) -> Self {
        Self {
            tx,
            broadcast_policy,
        }
    }

    /// Prepare a transaction to be broadcasted to a random connection.
    pub fn random_broadcast(tx: Transaction) -> Self {
        Self {
            tx,
            broadcast_policy: TxBroadcastPolicy::RandomPeer,
        }
    }
}

/// The strategy for how this transaction should be shared with the network.
#[derive(Debug, Default, Clone)]
pub enum TxBroadcastPolicy {
    /// Broadcast the transaction to all peers at the same time.
    AllPeers,
    /// Broadcast the transaction to a single random peer, optimal for user privacy.
    #[default]
    RandomPeer,
}

/// A peer on the Bitcoin P2P network
///
/// # Building peers
///
/// ```rust
/// use std::net::{IpAddr, Ipv4Addr};
/// use kyoto::{TrustedPeer, ServiceFlags, AddrV2};
///
/// let local_host = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
/// let mut trusted = TrustedPeer::from_ip(local_host);
/// // Optionally set the known services of the peer later.
/// trusted.set_services(ServiceFlags::P2P_V2);
///
/// let local_host = Ipv4Addr::new(0, 0, 0, 0);
/// // Or construct a trusted peer directly.
/// let trusted = TrustedPeer::new(AddrV2::Ipv4(local_host), None, ServiceFlags::P2P_V2);
///
/// // Or implicitly with `into`
/// let local_host = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
/// let trusted: TrustedPeer = (local_host, None).into();
/// ```
#[derive(Debug, Clone)]
pub struct TrustedPeer {
    /// The IP address of the remote node to connect to.
    pub address: AddrV2,
    /// The port to establish a TCP connection. If none is provided, the typical Bitcoin Core port is used as the default.
    pub port: Option<u16>,
    /// The services this peer is known to offer before starting the node.
    pub known_services: ServiceFlags,
}

impl TrustedPeer {
    /// Create a new trusted peer.
    pub fn new(address: AddrV2, port: Option<u16>, services: ServiceFlags) -> Self {
        Self {
            address,
            port,
            known_services: services,
        }
    }

    /// Create a new peer from a TorV3 service and port.
    #[cfg(feature = "tor")]
    pub fn new_from_tor_v3(
        public_key: [u8; 32],
        port: Option<u16>,
        services: ServiceFlags,
    ) -> Self {
        let address = AddrV2::TorV3(public_key);
        Self {
            address,
            port,
            known_services: services,
        }
    }

    /// Create a new trusted peer using the default port for the network.
    pub fn from_ip(ip_addr: impl Into<IpAddr>) -> Self {
        let address = match ip_addr.into() {
            IpAddr::V4(ip) => AddrV2::Ipv4(ip),
            IpAddr::V6(ip) => AddrV2::Ipv6(ip),
        };
        Self {
            address,
            port: None,
            known_services: ServiceFlags::NONE,
        }
    }

    /// Create a new peer from a known address and port.
    pub fn from_socket_addr(socket_addr: impl Into<SocketAddr>) -> Self {
        let socket_addr: SocketAddr = socket_addr.into();
        let address = match socket_addr {
            SocketAddr::V4(ip) => AddrV2::Ipv4(*ip.ip()),
            SocketAddr::V6(ip) => AddrV2::Ipv6(*ip.ip()),
        };
        Self {
            address,
            port: Some(socket_addr.port()),
            known_services: ServiceFlags::NONE,
        }
    }

    /// Create a new peer from a TorV3 service.
    #[cfg(feature = "tor")]
    pub fn from_tor_v3(public_key: [u8; 32]) -> Self {
        let address = AddrV2::TorV3(public_key);
        Self {
            address,
            port: None,
            known_services: ServiceFlags::NONE,
        }
    }

    /// The IP address of the trusted peer.
    pub fn address(&self) -> AddrV2 {
        self.address.clone()
    }

    /// A recommended port to connect to, if there is one.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// The services this peer is known to offer.
    pub fn services(&self) -> ServiceFlags {
        self.known_services
    }

    /// Set the known services for this trusted peer.
    pub fn set_services(&mut self, services: ServiceFlags) {
        self.known_services = services;
    }
}

impl From<(IpAddr, Option<u16>)> for TrustedPeer {
    fn from(value: (IpAddr, Option<u16>)) -> Self {
        let address = match value.0 {
            IpAddr::V4(ip) => AddrV2::Ipv4(ip),
            IpAddr::V6(ip) => AddrV2::Ipv6(ip),
        };
        TrustedPeer::new(address, value.1, ServiceFlags::NONE)
    }
}

impl From<TrustedPeer> for (AddrV2, Option<u16>) {
    fn from(value: TrustedPeer) -> Self {
        (value.address(), value.port())
    }
}

impl From<IpAddr> for TrustedPeer {
    fn from(value: IpAddr) -> Self {
        TrustedPeer::from_ip(value)
    }
}

impl From<SocketAddr> for TrustedPeer {
    fn from(value: SocketAddr) -> Self {
        TrustedPeer::from_socket_addr(value)
    }
}

/// How to connect to peers on the peer-to-peer network
#[derive(Default, Clone)]
#[non_exhaustive]
pub enum ConnectionType {
    /// Version one peer-to-peer connections
    #[default]
    ClearNet,
    /// Connect to peers over Tor
    #[cfg(feature = "tor")]
    Tor(TorClient<PreferredRuntime>),
}

/// Configure how many peers will be stored.
#[derive(Debug, Default, Clone)]
pub enum PeerStoreSizeConfig {
    /// Add new peers to the store regardless of the current size. For memory-limited [`PeerStore`]
    /// implementations, consider using a bounded size.
    #[default]
    Unbounded,
    /// Bound the size of the [`PeerStore`]. When set, no new peers will be requested if the database
    /// has at least this amount of peers.
    Limit(u32),
}

/// Select the category of messages for the node to emit.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Send `Log::Debug` messages. These messages are intended for debugging or troubleshooting
    /// node operation.
    #[default]
    Debug,
    /// Omit `Log::Debug` messages, including their memory allocations. Ideal for a production
    /// application that uses minimal logging.
    Warning,
}

macro_rules! log {
    ($dialog:expr, $expr:expr) => {
        match $dialog.log_level {
            crate::LogLevel::Debug => $dialog.send_dialog($expr).await,
            crate::LogLevel::Warning => (),
        }
    };
}

pub(crate) use log;
use serde::{Deserialize, Serialize};
use crate::core::node::QueueBlocksStatus;
