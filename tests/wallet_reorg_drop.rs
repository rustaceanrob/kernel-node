mod common;

use std::time::Duration;

use bitcoin::{Amount, BlockHash};
use common::{fund_silent_payment, start_bitcoind, TestNode};

const SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const REORG_TIMEOUT: Duration = Duration::from_secs(60);
const PAYMENT: Amount = Amount::from_sat(100_000_000);
const BRANCH_BLOCKS: usize = 2;

#[test]
fn reorg_drops_a_payment_that_is_not_re_mined() {
    let core = start_bitcoind();
    let p2p = core.params.p2p_socket.unwrap();
    println!("step 1/5: bitcoind started on regtest");

    let node = TestNode::start_connected(p2p, Some(common::random_signing_keys()));
    let sp_address = node.receive_address();
    println!("step 2/5: node started, receive address {sp_address}");

    fund_silent_payment(&core, &sp_address, PAYMENT);
    let funding_height = core.client.get_block_count().unwrap().0;
    let balance = node.wait_for_balance(PAYMENT, SYNC_TIMEOUT);
    assert_eq!(balance, PAYMENT);
    println!("step 3/5: payment mined at height {funding_height}, balance = {balance}");

    let funding_block = core
        .client
        .get_block_hash(funding_height)
        .unwrap()
        .0
        .parse::<BlockHash>()
        .unwrap();
    core.client.invalidate_block(funding_block).unwrap();

    let miner = core.client.new_address().unwrap().to_string();
    for _ in 0..BRANCH_BLOCKS {
        core.client.generate_block(&miner, &[], true).unwrap();
    }
    let tip_height = core.client.get_block_count().unwrap().0;
    let tip_hash = core.client.best_block_hash().unwrap();
    assert!(tip_height > funding_height);
    println!("step 4/5: mined a longer branch to height {tip_height} without the payment");

    node.wait_for_tip(tip_height, tip_hash, REORG_TIMEOUT);
    node.wait_for_balance_of(Amount::ZERO, REORG_TIMEOUT);
    println!("step 5/5: node followed the reorg and the balance returned to zero");

    node.stop();
}
