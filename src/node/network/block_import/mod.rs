#![allow(unused)]
use handle::ImportHandle;
use reth_engine_primitives::EngineTypes;
use reth_network::import::{BlockImport, BlockImportOutcome, NewBlockEvent};
use reth_network_peers::PeerId;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives::NodePrimitives;
use service::{BlockMsg, ImportEvent, Outcome};
use std::{
    fmt,
    task::{ready, Context, Poll},
};

use crate::node::network::HlNewBlock;

pub mod handle;
pub mod service;

#[derive(Debug)]
pub struct HlBlockImport {
    handle: ImportHandle,
}

impl HlBlockImport {
    pub fn new(handle: ImportHandle) -> Self {
        Self { handle }
    }
}

impl BlockImport<HlNewBlock> for HlBlockImport {
    fn on_new_block(&mut self, peer_id: PeerId, incoming_block: NewBlockEvent<HlNewBlock>) {
        if let NewBlockEvent::Block(block) = incoming_block {
            let _ = self.handle.send_block(block, peer_id);
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<ImportEvent> {
        match ready!(self.handle.poll_outcome(cx)) {
            Some(outcome) => Poll::Ready(outcome),
            None => Poll::Pending,
        }
    }
}
