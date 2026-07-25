mod common;

use std::time::{Duration, Instant};

use common::{start_bitcoind, TestNode};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[test]
fn connects_to_bitcoin_core() {
    let core = start_bitcoind();
    let p2p = core.params.p2p_socket.unwrap();

    let node = TestNode::start_connected(p2p, None);

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let count = core
            .client
            .get_connection_count()
            .expect("query bitcoind connection count")
            .0;
        if count >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "node did not connect to bitcoind within {CONNECT_TIMEOUT:?}"
        );
        std::thread::sleep(POLL_INTERVAL);
    }

    node.stop();
}
