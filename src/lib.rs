use log::error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

pub mod daemonize;
pub mod ext;
pub mod ipc;
pub mod net;

pub mod logging {
    pub struct Category;

    impl Category {
        pub const KERNEL: &str = "kernel";
        pub const NET: &str = "net";
        pub const WALLET: &str = "wallet";
        pub const IPC: &str = "ipc";
        pub const NODE: &str = "node";
    }
}

pub enum ScanEvent {
    Connected {
        block: bitcoinkernel::Block,
        block_hash: bitcoinkernel::BlockHash,
    },
    Disconnected {
        block: bitcoinkernel::Block,
        block_height: u32,
    },
}

#[derive(Clone)]
pub struct FatalShutdown {
    triggered: Arc<AtomicBool>,
    tx: mpsc::Sender<()>,
}

impl FatalShutdown {
    pub fn new(tx: mpsc::Sender<()>) -> Self {
        Self {
            triggered: Arc::new(AtomicBool::new(false)),
            tx,
        }
    }

    pub fn trigger(&self, target: &str, message: impl std::fmt::Display) {
        error!(target: target, "{}", message);
        if !self.triggered.swap(true, Ordering::SeqCst) {
            self.tx.send(()).expect("failed to send shutdown signal");
        }
    }
}
