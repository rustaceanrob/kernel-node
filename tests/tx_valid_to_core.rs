mod common;

use std::time::Duration;

use bitcoin::Amount;
use common::{fund_silent_payment, start_bitcoind, wait_for_core_mempool, TestNode};

const SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

#[test]
fn node_transaction_is_valid_to_core() {
    let core = start_bitcoind();
    let p2p = core.params.p2p_socket.unwrap();
    println!("step 1/6: bitcoind started on regtest");

    let node = TestNode::start_connected(p2p, Some(common::random_signing_keys()));
    println!("step 2/6: spend-capable node started and connected to Core");

    let sp_address = node.receive_address();
    println!("step 3/6: node receive address {sp_address}");

    let funded = Amount::from_sat(100_000_000);
    fund_silent_payment(&core, &sp_address, funded);
    println!(
        "step 4/6: broadcast a {} sat silent payment to the node",
        funded.to_sat()
    );

    let balance = node.wait_for_balance(funded, SYNC_TIMEOUT);
    println!("step 5/6: node scanned the payment, balance = {balance}");

    let destination = core.client.new_address().unwrap();
    let out = node.cli(&[
        "wallet",
        "send-to-address",
        &destination.to_string(),
        "50000000",
        "5",
    ]);
    assert!(
        out.status.success(),
        "send-to-address failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let txid = stdout.split_whitespace().last().expect("txid in output");

    wait_for_core_mempool(&core, txid, ACCEPT_TIMEOUT);
    println!("step 6/6: node built {txid} and Core accepted it into the mempool");

    node.stop();
}
