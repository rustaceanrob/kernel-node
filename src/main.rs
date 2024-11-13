use std::{
    fs,
    io::Cursor,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Once,
    },
};

pub mod kernel_util;
mod peer;

use crate::kernel_util::BitcoinNetwork;
use bitcoin::{
    consensus::{encode, Decodable},
    hashes::Hash,
    p2p::{
        message::{NetworkMessage, RawNetworkMessage},
        Address, ServiceFlags,
    },
    BlockHash, Network,
};
use bitcoinkernel::{
    register_validation_interface, unregister_validation_interface, BlockManagerOptions, ChainType,
    ChainstateLoadOptions, ChainstateManager, ChainstateManagerOptions, Context, ContextBuilder,
    KernelNotificationInterfaceCallbackHolder, Log, Logger, SynchronizationState,
    ValidationInterfaceCallbackHolder, ValidationInterfaceWrapper,
};
use clap::Parser;
use home::home_dir;
use kernel_util::bitcoin_block_to_kernel_block;
use log::{debug, error, info, warn};
use peer::{process_message, NodeState, PeerStateMachine};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{net::TcpStream, sync::broadcast};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Which Bitcoin network to use
    #[arg(value_enum, short, long, default_value = "signet")]
    network: BitcoinNetwork,

    /// Data directory for blockchain and configuration
    #[arg(long, default_value = "~/.kernel-node")]
    datadir: String,

    /// Connect only to this node (format: ip:port or hostname:port)
    #[arg(long)]
    connect: Option<String>,
}

impl Args {
    fn get_data_dir(&self) -> String {
        let path = if self.datadir.starts_with("~/") {
            if let Some(mut home) = home_dir() {
                home.push(&self.datadir[2..]);
                home
            } else {
                PathBuf::from(&self.datadir)
            }
        } else {
            PathBuf::from(&self.datadir)
        };

        // Create directories if they don't exist
        fs::create_dir_all(&path).unwrap();

        // Get canonical (full) path
        path.canonicalize().unwrap().to_str().unwrap().to_string()
    }
}

fn create_context(chain_type: ChainType, shutdown_tx: broadcast::Sender<()>) -> Arc<Context> {
    let shutdown_triggered = Arc::new(AtomicBool::new(false));
    let shutdown_triggered_clone = Arc::clone(&shutdown_triggered);
    let shutdown_tx_clone = shutdown_tx.clone();
    Arc::new(ContextBuilder::new()
        .chain_type(chain_type)
        .kn_callbacks(Box::new(KernelNotificationInterfaceCallbackHolder {
            kn_block_tip: Box::new(|state, block_hash| {
                let hash = BlockHash::from_byte_array(block_hash.hash);
                match state {
                    SynchronizationState::INIT_DOWNLOAD => info!("Received new block tip {} during IBD.", hash),
                    SynchronizationState::POST_INIT => info!("Received new block {}", hash),
                    SynchronizationState::INIT_REINDEX => info!("Moved new block tip {} during reindex.", hash),
                };
            }),
            kn_header_tip: Box::new(|state, height, timestamp, presync| {
                match state {
                    SynchronizationState::INIT_DOWNLOAD => info!("Received new header tip during IBD at height {} and time {}. Presync mode: {}", height, timestamp, presync),
                    SynchronizationState::POST_INIT => info!("Received new header tip at height {} and time {}. Presync mode: {}", height, timestamp, presync),
                    SynchronizationState::INIT_REINDEX => info!("Moved to new header tip during reindex at height {} and time {}. Presync mode: {}", height, timestamp, presync),
                }
            }),
            kn_progress: Box::new(|title, progress, resume_possible| {
                warn!("Made progress {}: {}. Can resume: {}", title, progress, resume_possible)
            }),
            kn_warning_set: Box::new(|_warning, _message| {}),
            kn_warning_unset: Box::new(|_warning| {}),
            kn_flush_error: Box::new(move |message| {
                if !shutdown_triggered.swap(true, Ordering::SeqCst) {
                    shutdown_tx.send(()).expect("failed to send shutdown signal");
                }
                error!("Fatal flush error encountered: {}", message);
            }),
            kn_fatal_error: Box::new(move |message| {
                error!("Fatal error encountered: {}", message);
                if !shutdown_triggered_clone.swap(true, Ordering::SeqCst) {
                    shutdown_tx_clone.send(()).expect("failed to send shutdown signal");
                }
            }),
        }))
        .build()
        .unwrap())
}

struct KernelLog {}

impl Log for KernelLog {
    fn log(&self, message: &str) {
        log::info!(
            target: "bitcoinkernel", 
            "{}", message.strip_suffix("\r\n").or_else(|| message.strip_suffix('\n')).unwrap_or(message));
    }
}

static START: Once = Once::new();
static mut GLOBAL_LOG_CALLBACK_HOLDER: Option<Logger<KernelLog>> = None;

fn setup_logging() {
    let mut builder = env_logger::Builder::from_default_env();
    builder.filter(None, log::LevelFilter::Info).init();

    unsafe { GLOBAL_LOG_CALLBACK_HOLDER = Some(Logger::new(KernelLog {}).unwrap()) };
}

fn setup_validation_interface(node_state: &NodeState)-> ValidationInterfaceWrapper {
    let validation_interface =
        ValidationInterfaceWrapper::new(Box::new(ValidationInterfaceCallbackHolder {
            block_checked: Box::new(move |_block, _mode, _result| {
                log::info!("got validation callback for successfully checking a block.");
            }),
        }));
    register_validation_interface(&validation_interface, &node_state.context).unwrap();
    validation_interface
}

const SIGNET_SEEDS: &[&str] = &[
    "seed.signet.bitcoin.sprovoost.nl.",
    "seed.signet.achownodes.xyz.",
];

const MAINNET_SEEDS: &[&str] = &[
    "seed.bitcoin.sipa.be.",
    "dnsseed.bluematt.me.",
    "dnsseed.bitcoin.dashjr-list-of-p2p-nodes.us.",
    "seed.bitcoin.jonasschnelli.ch.",
    "seed.btc.petertodd.net.",
    "seed.bitcoin.sprovoost.nl.",
    "dnsseed.emzy.de.",
    "seed.bitcoin.wiz.biz.",
    "seed.mainnet.achownodes.xyz.",
];

const TESTNET_SEEDS: &[&str] = &[
    "testnet-seed.bitcoin.jonasschnelli.ch.",
    "seed.tbtc.petertodd.net.",
    "seed.testnet.bitcoin.sprovoost.nl.",
    "testnet-seed.bluematt.me.",
    "seed.testnet.achownodes.xyz.",
];

fn get_seeds(network: Network) -> &'static [&'static str] {
    match network {
        Network::Bitcoin => MAINNET_SEEDS,
        Network::Testnet => TESTNET_SEEDS,
        Network::Signet => SIGNET_SEEDS,
        Network::Regtest => panic!("Regtest does not support seed nodes, use -connect instead"),
        _ => panic!("not supported."),
    }
}

fn resolve_seeds(seeds: &[&str]) -> Vec<SocketAddr> {
    let mut addresses = Vec::new();
    for seed in seeds {
        if let Ok(ips) = dns_lookup::lookup_host(seed) {
            for ip in ips {
                addresses.push(SocketAddr::new(ip, 38333));
            }
        }
    }
    addresses
}

struct BitcoinPeer {
    addr: Address,
    stream: TcpStream,
    network: Network,
}

impl BitcoinPeer {
    async fn new(addr: SocketAddr, network: Network) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        Ok(BitcoinPeer {
            addr: Address::new(&addr, ServiceFlags::WITNESS),
            stream,
            network,
        })
    }

    async fn send_message(&mut self, msg: NetworkMessage) -> std::io::Result<()> {
        let raw_msg = RawNetworkMessage::new(self.network.magic(), msg);
        let bytes = encode::serialize(&raw_msg);
        self.stream.write_all(&bytes).await?;
        Ok(())
    }

    async fn receive_message(&mut self) -> std::io::Result<NetworkMessage> {
        // First read the header, then the payload. Do this, because we cannot read all at once in an async context.
        let mut header_buf = [0u8; 24];
        self.stream.read_exact(&mut header_buf).await?;
        let payload_len = u32::from_le_bytes([
            header_buf[16],
            header_buf[17],
            header_buf[18],
            header_buf[19],
        ]) as usize;

        let mut payload_buf = vec![0u8; payload_len];
        self.stream.read_exact(&mut payload_buf).await?;

        let raw_msg = RawNetworkMessage::consensus_decode(&mut Cursor::new(
            [header_buf.as_slice(), payload_buf.as_slice()].concat(),
        ))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        Ok(raw_msg.payload().clone())
    }
}

async fn run_connection(
    network: Network,
    connect: Option<SocketAddr>,
    mut node_state: NodeState,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let addr = if let Some(addr) = connect {
        addr
    } else {
        let seeds = get_seeds(network);
        info!("These are the dns seeds we are going to use: {:?}", seeds);
        let addresses = resolve_seeds(seeds);
        debug!("These are the resolved addresses from the dns seeds: {:?}", addresses);
        addresses[0]
    };
    let mut peer = BitcoinPeer::new(addr, network).await?;
    info!("Connected to peer");

    // Initial handshake
    let (mut peer_state_machine, mut messages) = process_message(
        PeerStateMachine::StartConnection,
        NetworkMessage::Addr(vec![(0, peer.addr.clone())]),
        &mut node_state,
    );
    for message in messages.drain(..) {
        peer.send_message(message).await.unwrap();
    }

    info!("Sent version message - starting event loop.");
    debug!("Why aren't you printing?");

    tokio::select! {
        result = async {
            // Basic message handling loop
            loop {
                match peer.receive_message().await {
                    Ok(msg) => {
                        (peer_state_machine, messages) = process_message(peer_state_machine, msg, &mut node_state);
                        for message in messages.drain(..) {
                            peer.send_message(message).await.unwrap();
                        }
                    },
                    Err(e) => {
                        info!("Error receiving message: {}", e);
                        break;
                    }
                }
            }
        } => result,
        _ = tokio::signal::ctrl_c() => {
            node_state.context.interrupt();
            info!("Received interrupt signal, shutting down...");
            return Ok(());
        }
        _ = shutdown_rx.recv() => {
            node_state.context.interrupt();
            info!("Received shutdown signal, shutting down...");
            return Ok(());
        }
    }
    info!("exiting.");
    Ok(())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    START.call_once(|| {
        setup_logging();
    });
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel(32);
    let context = create_context(args.network.into(), shutdown_tx);
    let data_dir = args.get_data_dir();
    let blocks_dir = data_dir.clone() + "/blocks";
    let chainman = ChainstateManager::new(
        ChainstateManagerOptions::new(&context, &data_dir).unwrap(),
        BlockManagerOptions::new(&context, &blocks_dir).unwrap(),
        Arc::clone(&context)
    )
    .unwrap();

    let mut node_state = NodeState {
        best_block: BlockHash::all_zeros(),
        height: 0,
        chainman,
        context: Arc::clone(&context),
    };

    let validation_interface = setup_validation_interface(&node_state);

    if let Err(err) = node_state.chainman.load_chainstate(ChainstateLoadOptions::new()) {
        error!("Error loading chainstate: {}", err);
        return Ok(());
    }
    if let Err(err) = node_state.chainman.import_blocks() {
        error!("Error importing blocks: {}", err);
        return Ok(());
    }

    let tip_index = node_state.chainman.get_block_index_tip();
    let height = tip_index.height();
    let hash = tip_index.block_hash();
    drop(tip_index);
    node_state.best_block = BlockHash::from_byte_array(hash.hash);
    node_state.height = height;

    info!("Bitcoin kernel initialized");

    let connect: Option<SocketAddr> = if let Some(connect) = args.connect {
        Some(connect.parse().unwrap())
    } else {
        None
    };

    if shutdown_rx.try_recv().is_ok() {
        info!("Shutting down!");
        return Ok(());
    }

    run_connection(
        args.network.into(),
        connect,
        node_state,
        shutdown_rx,
    )
    .await?;

    unregister_validation_interface(&validation_interface, &context).unwrap();

    Ok(())
}
