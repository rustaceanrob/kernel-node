use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::BlockHash;
use bitcoinkernel::{core::BlockHashExt, ChainstateManager};

use super::chain_capnp;

pub struct ChainIpcInterface {
    chainman: Arc<ChainstateManager>,
}

impl ChainIpcInterface {
    pub fn new(chainman: Arc<ChainstateManager>) -> Self {
        Self { chainman }
    }
}

impl chain_capnp::chain::Server for ChainIpcInterface {
    async fn get_tip(
        self: capnp::capability::Rc<Self>,
        _: chain_capnp::chain::GetTipParams,
        mut results: chain_capnp::chain::GetTipResults,
    ) -> Result<(), capnp::Error> {
        let tip = self.chainman.active_chain().tip();
        let height = tip.height();
        let hash = BlockHash::from_byte_array(tip.block_hash().to_bytes());

        let mut r = results.get();
        r.set_height(height as u32);
        r.set_hash(hash.to_string());
        Ok(())
    }
}
