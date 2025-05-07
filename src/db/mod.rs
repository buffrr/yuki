//! Traits and structures that define the data persistence required for a node.
//!
//! All nodes require a [`HeaderStore`](traits::HeaderStore) and a [`PeerStore`](traits::PeerStore). Unless
//! your application dependency tree is particularly strict, SQL-based storage will be sufficient for the majority of
//! applications.

use bitcoin::key::rand::distributions::Standard;
use bitcoin::key::rand::prelude::Distribution;
use bitcoin::key::rand::{thread_rng, Rng};
use bitcoin::p2p::address::AddrV2;
use bitcoin::p2p::ServiceFlags;

/// Errors a database backend may produce.
pub mod error;
/// Persistence traits defined with SQL Lite to store data between sessions.
#[cfg(feature = "database")]
pub mod sqlite;
/// Traits that define the header and peer databases.
pub mod traits;

/// A peer that will be saved to the [`traits::PeerStore`].
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedPeer {
    /// Canonical IP address of this peer.
    pub addr: AddrV2,
    /// The port believed to be listening for connections.
    pub port: u16,
    /// The services this peer may offer.
    pub services: ServiceFlags,
    /// A new, tried, or banned status.
    pub status: PeerStatus,
}

impl PersistedPeer {
    /// Build a new peer with known fields
    pub fn new(addr: AddrV2, port: u16, services: ServiceFlags, status: PeerStatus) -> Self {
        Self {
            addr,
            port,
            services,
            status,
        }
    }
}

impl From<PersistedPeer> for (AddrV2, u16) {
    fn from(value: PersistedPeer) -> Self {
        (value.addr, value.port)
    }
}

/// The status of a peer in the database.
#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum PeerStatus {
    /// A peer was gossiped via DNS or the peer-to-peer network.
    Gossiped,
    /// The node successfully connected to this peer.
    Tried,
    /// A connected peer responded with faulty or malicious behavior.
    Ban,
}

impl Distribution<PeerStatus> for Standard {
    fn sample<R: bitcoin::key::rand::Rng + ?Sized>(&self, rng: &mut R) -> PeerStatus {
        match rng.gen_range(0..=1) {
            0 => PeerStatus::Gossiped,
            _ => PeerStatus::Tried,
        }
    }
}

impl PeerStatus {
    pub(crate) fn random() -> PeerStatus {
        let mut rng = thread_rng();
        rng.r#gen()
    }
}
