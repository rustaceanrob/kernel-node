pub mod daemonize;
pub mod ext;
pub mod ipc;
pub mod peer;

capnp::generated_code!(pub mod server_capnp);
capnp::generated_code!(pub mod wallet_capnp);
