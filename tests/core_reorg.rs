mod common;

use std::time::Duration;

use bitcoin::BlockHash;
use common::{start_bitcoind, TestNode};

const SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const REORG_TIMEOUT: Duration = Duration::from_secs(60);
const INITIAL_BLOCKS: usize = 10;
const FORK_HEIGHT: u64 = 8;
const NEW_BLOCKS: usize = 5;

#[test]
fn follows_bitcoin_core_reorg() {
    let core = start_bitcoind();
    let address = core.client.new_address().expect("new address");
    core.client
        .generate_to_address(INITIAL_BLOCKS, &address)
        .expect("mine initial blocks");

    let initial_height = core.client.get_block_count().expect("block count").0;
    let initial_hash = core.client.best_block_hash().expect("best block hash");

    let p2p = core.params.p2p_socket.expect("bitcoind p2p socket");
    let node = TestNode::start_connected(p2p, None);
    node.wait_for_tip(initial_height, initial_hash, SYNC_TIMEOUT);

    let fork_block = core
        .client
        .get_block_hash(FORK_HEIGHT)
        .expect("block hash at fork height")
        .0
        .parse::<BlockHash>()
        .expect("parse fork block hash");
    core.client
        .invalidate_block(fork_block)
        .expect("invalidate block");
    let reorg_address = core.client.new_address().expect("new address");
    core.client
        .generate_to_address(NEW_BLOCKS, &reorg_address)
        .expect("mine competing branch");

    let reorg_height = core.client.get_block_count().expect("block count").0;
    let reorg_hash = core.client.best_block_hash().expect("best block hash");
    assert!(reorg_height > initial_height);
    assert_ne!(reorg_hash, initial_hash);

    node.wait_for_tip(reorg_height, reorg_hash, REORG_TIMEOUT);

    node.stop();
}
