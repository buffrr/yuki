use std::collections::{BTreeMap, VecDeque};
use bitcoin::BlockHash;
use bitcoin::p2p::message_filter::GetCFilters;
use tokio::time::Instant;
use crate::chain::chain::AdvanceKind;
use crate::core::messages::{DownloadSender};

#[derive(Debug)]
pub(crate) struct DownloadQueue {
    blocks: VecDeque<DownloadRequest>,
    filters: BTreeMap<u32, DownloadRequest>,
}

pub(crate) struct QueueBatch {
  pub(crate) requests: Vec<DownloadRequest>,
  pub(crate) kind: QueueBatchKind
}

pub(crate) enum QueueBatchKind {
    Block(Vec<BlockHash>),
    Filter(GetCFilters)
}

impl DownloadQueue {
    pub(crate) fn new() -> Self {
        Self {
            blocks: VecDeque::new(),
            filters: BTreeMap::new(),
        }
    }

    pub(crate) fn add(&mut self, request: impl Into<DownloadRequest>) {
        let request: DownloadRequest = request.into();
        match request.kind {
            AdvanceKind::Blocks =>  {
                if self.contains_block(&request.hash) {
                    return;
                }
                self.blocks.push_back(request)
            },
            AdvanceKind::Filters => {
                if self.filters.contains_key(&request.height) &&
                    self.filters.iter().any(|(_, req)| req.hash.eq(&request.hash)) {
                    return;
                }
                self.filters.insert(request.height, request);
            }
        }
    }

    pub(crate) fn contains_block(&self, block: &BlockHash) -> bool {
        self.blocks.iter().any(|req| req.hash.eq(block))
    }

    /// Pops the next peer request if we have a combination of block and filter requests
    /// we'll prioritize filters.
    pub(crate) fn pop(&mut self, capacity: usize) -> Option<QueueBatch> {
        if self.complete() {
            return None;
        }

        let mut requests = Vec::with_capacity(capacity);
        let mut expected_next: Option<u32> = None;
        let mut start_height = 0;
        let mut stop_hash = None;

        while requests.len() < capacity {
            let height = match self.filters.iter().next() {
                None => break,
                Some((height, _)) => *height,
            };
            if expected_next.is_some_and(|expected| expected != height) {
                break;
            }
            let (h, dr) = match self.filters.pop_first() {
                None => break,
                Some(pair) => pair,
            };

            if requests.is_empty() {
                start_height = h;
            }
            stop_hash = Some(dr.hash);
            requests.push(dr);
            expected_next = Some(h + 1);
        }

        if !requests.is_empty() {
            let kind = QueueBatchKind::Filter(GetCFilters {
                filter_type: 0,           // basic
                start_height,
                stop_hash: stop_hash.unwrap(),
            });
            return Some(QueueBatch { requests, kind });
        }

        // 2) Fallback: drain up to `capacity` blocks
        let mut block_reqs = Vec::with_capacity(capacity);
        let mut hashes     = Vec::with_capacity(capacity);

        for _ in 0..capacity {
            match self.blocks.pop_front() {
                Some(dr) => {
                    hashes.push(dr.hash);
                    block_reqs.push(dr);
                }
                None => break,
            }
        }

        if !block_reqs.is_empty() {
            let kind = QueueBatchKind::Block(hashes);
            return Some(QueueBatch {
                requests: block_reqs,
                kind,
            });
        }

        None
    }

    /// Returns true when there is no batch in flight and the internal queue is empty.
    pub(crate) fn complete(&self) -> bool {
        self.blocks.is_empty() && self.filters.is_empty()
    }

    /// Removes any requests from the queue and the current batch that match any hash in `hashes`.
    pub(crate) fn remove(&mut self, hashes: &[BlockHash]) {
        self.blocks.retain(|req| !hashes.contains(&req.hash));
        self.filters.retain(|_, v| !hashes.contains(&v.hash));
    }
}

#[derive(Debug)]
pub struct DownloadRequest {
    pub(crate) hash: BlockHash,
    pub(crate) kind: AdvanceKind,
    pub(crate) height: u32,
    pub(crate) sender: Option<DownloadSender>,
    pub(crate) downloading_since: Option<Instant>,
}
