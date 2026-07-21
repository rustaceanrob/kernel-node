mod common;

use std::time::Duration;

use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use bitcoin::Amount;
use common::{fund_silent_payment, start_bitcoind, TestNode};

const SYNC_TIMEOUT: Duration = Duration::from_secs(60);

#[test]
fn receives_a_silent_payment() {
    let core = start_bitcoind();
    let p2p = core.params.p2p_socket.unwrap();
    println!("step 1/5: bitcoind started on regtest");

    let node = TestNode::start_connected(p2p);
    println!("step 2/5: node started and connected to Core");

    let secp = Secp256k1::new();
    let scan_key = SecretKey::from_slice(&[0x42; 32]).unwrap();
    let spend_key = SecretKey::from_slice(&[0x43; 32]).unwrap();
    let scan_hex = scan_key.secret_bytes().to_lower_hex_string();
    let spend_pub_hex = spend_key
        .x_only_public_key(&secp)
        .0
        .serialize()
        .to_lower_hex_string();
    node.import_keys(&scan_hex, &spend_pub_hex);
    let sp_address = node.receive_address();
    println!("step 3/5: imported scan keys, receive address {sp_address}");

    let amount = Amount::from_sat(100_000_000);
    fund_silent_payment(&core, &sp_address, amount);
    println!(
        "step 4/5: broadcast a {} sat silent payment to the node",
        amount.to_sat()
    );

    let balance = node.wait_for_balance(amount, SYNC_TIMEOUT);
    assert_eq!(balance, amount);
    println!("step 5/5: node scanned the payment, balance = {balance}");

    node.stop();
}
