use std::{
    fs,
    net::{Shutdown, SocketAddr},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex, Once,
    },
    thread,
    time::Duration,
};

pub mod kernel_util;
mod peer;

use crate::kernel_util::BitcoinNetwork;
use bitcoin::{hashes::Hash, BlockHash, Network};
use bitcoinkernel::{
    BlockManagerOptions, ChainType, ChainstateLoadOptions, ChainstateManager,
    ChainstateManagerOptions, Context, ContextBuilder, KernelNotificationInterfaceCallbackHolder,
    Log, Logger, SynchronizationState, ValidationInterfaceCallbackHolder, ValidationMode,
};
use clap::Parser;
use home::home_dir;
use kernel_util::bitcoin_block_to_kernel_block;
use log::{debug, error, info, warn};
use peer::{BitcoinPeer, NodeState, TipState};

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

fn create_context(
    chain_type: ChainType,
    shutdown_tx: mpsc::Sender<()>,
    tip_state: &Arc<Mutex<TipState>>,
) -> Arc<Context> {
    let shutdown_triggered = Arc::new(AtomicBool::new(false));
    let shutdown_triggered_clone = Arc::clone(&shutdown_triggered);
    let shutdown_tx_clone = shutdown_tx.clone();
    Arc::new(ContextBuilder::new()
        .chain_type(chain_type)
        .kn_callbacks(Box::new(KernelNotificationInterfaceCallbackHolder {
            kn_block_tip: Box::new(|state, block_hash| {
                let hash = BlockHash::from_byte_array(block_hash.hash);
                match state {
                    SynchronizationState::INIT_DOWNLOAD => debug!("Received new block tip {} during IBD.", hash),
                    SynchronizationState::POST_INIT => info!("Received new block {}", hash),
                    SynchronizationState::INIT_REINDEX => debug!("Moved new block tip {} during reindex.", hash),
                };
            }),
            kn_header_tip: Box::new(|state, height, timestamp, presync| {
                match state {
                    SynchronizationState::INIT_DOWNLOAD => debug!("Received new header tip during IBD at height {} and time {}. Presync mode: {}", height, timestamp, presync),
                    SynchronizationState::POST_INIT => info!("Received new header tip at height {} and time {}. Presync mode: {}", height, timestamp, presync),
                    SynchronizationState::INIT_REINDEX => debug!("Moved to new header tip during reindex at height {} and time {}. Presync mode: {}", height, timestamp, presync),
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
        .validation_interface(setup_validation_interface(tip_state))
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

fn setup_validation_interface(
    tip_state: &Arc<Mutex<TipState>>,
) -> Box<ValidationInterfaceCallbackHolder> {
    let tip_state_clone = Arc::clone(&tip_state);
    Box::new(ValidationInterfaceCallbackHolder {
        block_checked: Box::new(move |block, mode, _result| match mode {
            ValidationMode::VALID => {
                let hash = bitcoin::BlockHash::from_byte_array(block.get_hash().hash);
                log::debug!("Validation interface: Successfully checked block: {}", hash);
                tip_state_clone.lock().unwrap().block_hash = hash;
            }
            _ => error!("Received an invalid block!"),
        }),
    })
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

fn run(
    network: Network,
    connect: Option<SocketAddr>,
    mut node_state: NodeState,
    shutdown_rx: mpsc::Receiver<()>,
    block_rx: mpsc::Receiver<bitcoinkernel::Block>,
) -> std::io::Result<()> {
    let addr = if let Some(addr) = connect {
        addr
    } else {
        let seeds = get_seeds(network);
        info!("These are the dns seeds we are going to use: {:?}", seeds);
        let addresses = resolve_seeds(seeds);
        info!(
            "These are the resolved addresses from the dns seeds: {:?}",
            addresses
        );
        addresses[1]
    };
    let mut peer = BitcoinPeer::new(addr, network, &mut node_state)?;
    info!("Connected to peer");

    let chainman = Arc::clone(&node_state.chainman);
    let context = Arc::clone(&node_state.context);

    let running = Arc::new(AtomicBool::new(true));
    let running_peer = running.clone();
    let running_block = running.clone();
    let connection = Arc::clone(&peer.stream);

    let peer_processing_handler = thread::spawn(move || {
        info!("Starting net processing thread.");
        while running_peer.load(Ordering::SeqCst) {
            if let Err(e) = peer.receive_and_process_message(&mut node_state) {
                if std::io::ErrorKind::ConnectionAborted == e.kind() {
                    debug!("Error processing message: {}", e);
                    break;
                }
                error!("Error processing message: {}", e);
                break;
            }
        }
        info!("Stopping net processing thread.");
    });

    let block_processing_handler = thread::spawn(move || {
        info!("Starting block processing thread.");
        while running_block.load(Ordering::SeqCst) {
            match block_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(block) => {
                    debug!("Validating block.");
                    let (_accepted, _new_block) = chainman.process_block(&block);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        info!("Stopping block processing thread.");
    });

    if let Ok(()) = shutdown_rx.recv() {
        context.interrupt();
        info!("Received shutdown signal, shutting down...");
        running.store(false, Ordering::SeqCst);
        connection.shutdown(Shutdown::Read).unwrap();
    }

    peer_processing_handler.join().unwrap();
    block_processing_handler.join().unwrap();

    info!("exiting.");
    Ok(())
}

fn main() {
    let args = Args::parse();
    START.call_once(|| {
        setup_logging();
    });
    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    let tip_state = Arc::new(Mutex::new(TipState {
        block_hash: BlockHash::all_zeros(),
    }));

    let context = create_context(args.network.into(), shutdown_tx.clone(), &tip_state);

    ctrlc::set_handler(move || shutdown_tx.send(()).unwrap()).unwrap();
    let data_dir = args.get_data_dir();
    let blocks_dir = data_dir.clone() + "/blocks";
    let chainman_opts = ChainstateManagerOptions::new(&context, &data_dir).unwrap();
    chainman_opts.set_worker_threads(16);
    let chainman = Arc::new(
        ChainstateManager::new(
            chainman_opts,
            BlockManagerOptions::new(&context, &blocks_dir).unwrap(),
            ChainstateLoadOptions::new(),
            Arc::clone(&context),
        )
        .unwrap(),
    );

    let (block_tx, block_rx) = mpsc::sync_channel(1);

    let node_state = NodeState {
        block_tx,
        tip_state,
        chainman,
        context: Arc::clone(&context),
    };

    if let Err(err) = node_state.chainman.import_blocks() {
        error!("Error importing blocks: {}", err);
        return;
    }

    let tip_index = node_state.chainman.get_block_index_tip();
    let hash = tip_index.block_hash();
    drop(tip_index);
    node_state.set_tip_state(BlockHash::from_byte_array(hash.hash));

    info!("Bitcoin kernel initialized");

    let connect: Option<SocketAddr> = if let Some(connect) = args.connect {
        Some(connect.parse().unwrap())
    } else {
        None
    };

    if shutdown_rx.try_recv().is_ok() {
        info!("Shutting down!");
        return;
    }

    run(
        args.network.into(),
        connect,
        node_state,
        shutdown_rx,
        block_rx,
    )
    .unwrap();
}
