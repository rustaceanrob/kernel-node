mod common;

use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use common::TestNode;
use silentpayments::SilentPaymentAddress;

#[test]
fn imports_keys_over_the_cli() {
    let secp = Secp256k1::new();
    let node = TestNode::start();

    let scan = SecretKey::from_slice(&[0x11; 32]).unwrap();
    let spend = SecretKey::from_slice(&[0x12; 32]).unwrap();
    let scan_hex = scan.secret_bytes().to_lower_hex_string();
    let spend_pub_hex = spend
        .x_only_public_key(&secp)
        .0
        .serialize()
        .to_lower_hex_string();

    node.import_keys(&scan_hex, &spend_pub_hex);

    let address = node.receive_address();
    SilentPaymentAddress::try_from(address.as_str())
        .expect("import should yield a valid silent payment address");

    node.stop();
}
