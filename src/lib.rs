pub mod daemonize;
pub mod ext;
pub mod ipc;
pub mod peer;

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

capnp::generated_code!(pub mod server_capnp);
capnp::generated_code!(pub mod wallet_capnp);
