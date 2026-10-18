use std::sync::{mpsc, Arc, Mutex};

use ::wallet::silentpayments::Wallet;
use bitcoin::Transaction;
use bitcoinkernel::ChainstateManager;

mod chain;
mod wallet;

pub use chain::ChainIpcInterface;
pub use wallet::WalletIpcInterface;

capnp::generated_code!(pub mod server_capnp);
capnp::generated_code!(pub mod wallet_capnp);
capnp::generated_code!(pub mod chain_capnp);

pub struct IpcInterface {
    tx: mpsc::Sender<()>,
    broadcast_tx: mpsc::SyncSender<Transaction>,
    state: Arc<Mutex<Wallet>>,
    chainman: Arc<ChainstateManager>,
}

impl IpcInterface {
    pub fn new(
        tx: mpsc::Sender<()>,
        broadcast_tx: mpsc::SyncSender<Transaction>,
        state: Arc<Mutex<Wallet>>,
        chainman: Arc<ChainstateManager>,
    ) -> Self {
        Self {
            tx,
            broadcast_tx,
            state,
            chainman,
        }
    }
}

impl server_capnp::server::Server for IpcInterface {
    async fn echo(
        self: capnp::capability::Rc<Self>,
        params: server_capnp::server::EchoParams,
        mut results: server_capnp::server::EchoResults,
    ) -> Result<(), capnp::Error> {
        let request = params.get()?.get_msg()?;
        let msg = request.to_string()?;
        results.get().set_reply(msg);
        Ok(())
    }

    async fn shutdown(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::ShutdownParams,
        _: server_capnp::server::ShutdownResults,
    ) -> Result<(), capnp::Error> {
        self.tx
            .send(())
            .map_err(|_| capnp::Error::failed("could not shutdown server.".to_string()))?;
        Ok(())
    }

    async fn make_wallet(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::MakeWalletParams,
        mut results: server_capnp::server::MakeWalletResults,
    ) -> Result<(), capnp::Error> {
        let client: wallet_capnp::wallet::Client = capnp_rpc::new_client(WalletIpcInterface::new(
            self.state.clone(),
            self.broadcast_tx.clone(),
        ));
        results.get().set_wallet(client);
        Ok(())
    }

    async fn make_chain(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::MakeChainParams,
        mut results: server_capnp::server::MakeChainResults,
    ) -> Result<(), capnp::Error> {
        let client: chain_capnp::chain::Client =
            capnp_rpc::new_client(ChainIpcInterface::new(self.chainman.clone()));
        results.get().set_chain(client);
        Ok(())
    }
}
