use bitcoin::{block::Header, p2p::{
    address::AddrV2,
    message_filter::{CFHeaders, CFilter, GetCFHeaders, GetCFilters},
    message_network::VersionMessage,
    ServiceFlags,
}, Block, BlockHash, FeeRate, Transaction, Txid, Wtxid};
use bitcoin::p2p::message_blockdata::Inventory;
use crate::core::messages::{RejectPayload};

use super::PeerId;

#[derive(Debug, Clone)]
pub(crate) enum MainThreadMessage {
    GetAddr,
    GetAddrV2,
    WtxidRelay,
    GetHeaders(GetHeaderConfig),
    GetFilterHeaders(GetCFHeaders),
    GetFilters(GetCFilters),
    GetTx(Vec<Txid>),
    GetBlock(Vec<BlockHash>),
    Disconnect,
    BroadcastTx(Transaction),
    Verack,
}

#[derive(Debug, Clone)]
pub struct GetHeaderConfig {
    pub locators: Vec<BlockHash>,
    pub stop_hash: Option<BlockHash>,
}

pub(crate) struct PeerThreadMessage {
    pub nonce: PeerId,
    pub message: PeerMessage,
}

#[derive(Debug)]
pub(crate) enum PeerMessage {
    Version(VersionMessage),
    Addr(Vec<CombinedAddr>),
    Headers(Vec<Header>),
    FilterHeaders(CFHeaders),
    Filter(CFilter),
    Tx(Transaction),
    Block(Block),
    NewBlocks(Vec<BlockHash>),
    Reject(RejectPayload),
    NotFound(Vec<Inventory>),
    Disconnect,
    Verack,
    Ping(u64),
    #[allow(dead_code)]
    Pong(u64),
    FeeFilter(FeeRate),
    TxRequests(Vec<Wtxid>),
}

#[derive(Debug, Clone)]
pub(crate) struct CombinedAddr {
    pub addr: AddrV2,
    pub port: u16,
    pub services: ServiceFlags,
}

impl CombinedAddr {
    pub(crate) fn new(addr: AddrV2, port: u16) -> Self {
        Self {
            addr,
            port,
            services: ServiceFlags::NONE,
        }
    }

    pub(crate) fn services(&mut self, services: ServiceFlags) {
        self.services = services
    }
}
