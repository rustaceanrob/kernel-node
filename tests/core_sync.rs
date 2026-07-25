mod common;

use std::time::Duration;

use common::{start_bitcoind, TestNode};

const SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const BLOCKS: usize = 10;

#[test]
fn follows_bitcoin_core_chain() {
    let core = start_bitcoind();
    let address = core.client.new_address().expect("new address");
    core.client
        .generate_to_address(BLOCKS, &address)
        .expect("mine blocks");

    let core_height = core.client.get_block_count().expect("block count").0;
    let core_hash = core.client.best_block_hash().expect("best block hash");
    assert_eq!(core_height, BLOCKS as u64);

    let p2p = core.params.p2p_socket.expect("bitcoind p2p socket");
    let node = TestNode::start_connected(p2p, None);

    node.wait_for_tip(core_height, core_hash, SYNC_TIMEOUT);

    node.stop();
}
