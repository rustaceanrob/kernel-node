use std::{
    collections::HashMap,
    fs,
    io::Cursor,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Once,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use bitcoin::{
    consensus::{encode, Decodable},
    hashes::Hash,
    p2p::{
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::{GetBlocksMessage, Inventory},
        message_network::VersionMessage,
        Address, ServiceFlags,
    },
    BlockHash, Network,
};
use bitcoinkernel::{
    BlockIndex, BlockManagerOptions, ChainType, ChainstateLoadOptions, ChainstateManager,
    ChainstateManagerOptions, Context, ContextBuilder, KernelNotificationInterfaceCallbackHolder,
    Log, Logger, SynchronizationState,
};
use clap::{Parser, ValueEnum};
use home::home_dir;
use log::{debug, error, info, warn};
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

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum BitcoinNetwork {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

impl From<BitcoinNetwork> for bitcoin::Network {
    fn from(network: BitcoinNetwork) -> Self {
        match network {
            BitcoinNetwork::Mainnet => Network::Bitcoin,
            BitcoinNetwork::Testnet => Network::Testnet,
            BitcoinNetwork::Signet => Network::Signet,
            BitcoinNetwork::Regtest => Network::Regtest,
        }
    }
}

impl From<BitcoinNetwork> for ChainType {
    fn from(network: BitcoinNetwork) -> Self {
        match network {
            BitcoinNetwork::Mainnet => ChainType::MAINNET,
            BitcoinNetwork::Testnet => ChainType::TESTNET,
            BitcoinNetwork::Signet => ChainType::SIGNET,
            BitcoinNetwork::Regtest => ChainType::REGTEST,
        }
    }
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

fn create_context(chain_type: ChainType, shutdown_tx: broadcast::Sender<()>) -> Context {
    let shutdown_triggered = Arc::new(AtomicBool::new(false));
    let shutdown_triggered_clone = Arc::clone(&shutdown_triggered);
    let shutdown_tx_clone = shutdown_tx.clone();
    ContextBuilder::new()
        .chain_type(chain_type)
        .kn_callbacks(Box::new(KernelNotificationInterfaceCallbackHolder {
            kn_block_tip: Box::new(|state, block_hash| {
                match state {
                    SynchronizationState::INIT_DOWNLOAD => info!("Received new block tip {:?} during IBD.", block_hash),
                    SynchronizationState::POST_INIT => info!("Received new block {:?}", block_hash),
                    SynchronizationState::INIT_REINDEX => info!("Moved new block tip {:?} during reindex.", block_hash),
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
        .unwrap()
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
    stream: TcpStream,
    network: Network,
}

impl BitcoinPeer {
    async fn new(addr: SocketAddr, network: Network) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        Ok(BitcoinPeer { stream, network })
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

// Version message for handshake
fn create_version_message(addr: SocketAddr, height: i32) -> NetworkMessage {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let mut version_message = VersionMessage::new(
        ServiceFlags::WITNESS,
        timestamp,
        Address::new(&addr, ServiceFlags::WITNESS),
        Address::new(&addr, ServiceFlags::WITNESS),
        0,
        "kernel-node".to_string(),
        height,
    );
    version_message.version = 70015;

    NetworkMessage::Version(version_message)
}

// GetBlocks message to request block inventories
fn create_getblocks_message(known_block_hash: BlockHash) -> NetworkMessage {
    NetworkMessage::GetBlocks(GetBlocksMessage {
        version: 70015,
        locator_hashes: vec![known_block_hash],
        stop_hash: BlockHash::all_zeros(),
    })
}

// GetData message to request specific blocks
fn create_getdata_message(block_hashes: Vec<BlockHash>) -> NetworkMessage {
    let inventory: Vec<Inventory> = block_hashes
        .into_iter()
        .map(|hash| Inventory::WitnessBlock(hash))
        .collect();

    NetworkMessage::GetData(inventory)
}

fn get_block_hash(index: BlockIndex) -> BlockHash {
    BlockHash::from_byte_array(index.info().hash)
}

fn bitcoin_block_to_kernel_block(block: &bitcoin::Block) -> bitcoinkernel::Block {
    let ser_block = encode::serialize(block);
    bitcoinkernel::Block::try_from(ser_block.as_slice()).unwrap()
}

async fn run_connection(
    network: Network,
    connect: Option<SocketAddr>,
    chainman: ChainstateManager<'_>,
    context: &Context,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let addr = if let Some(addr) = connect {
        addr
    } else {
        let seeds = get_seeds(network);
        info!("These are the seeds we are going to use: {:?}", seeds);
        let addresses = resolve_seeds(seeds);
        info!("These are the resolved addresses: {:?}", addresses);
        addresses[0]
    };
    let mut peer = BitcoinPeer::new(addr, network).await?;
    info!("Connected to peer");

    let height = chainman.get_block_index_tip().height();
    // Initial handshake
    peer.send_message(create_version_message(addr, height))
        .await?;
    info!("Sent version message");

    // Out of order block handling
    let mut pending_blocks: HashMap<
        BlockHash,            /*prev */
        bitcoinkernel::Block, /*block */
    > = HashMap::new();
    let mut n_requested_blocks = 0;

    tokio::select! {
        result = async {
            // Basic message handling loop
            loop {
                match peer.receive_message().await {
                    Ok(msg) => match msg {
                        NetworkMessage::Version(version) => {
                            info!("Received version: {:?}", version);
                            peer.send_message(NetworkMessage::Verack).await.unwrap();
                        }
                        NetworkMessage::Verack => {
                            info!("Received verack - handshake complete");

                            let tip_index = chainman.get_block_index_tip();
                            let tip_hash = get_block_hash(tip_index);

                            let getblocks = create_getblocks_message(tip_hash);
                            peer.send_message(getblocks).await.unwrap();
                            info!("Requested initial blocks starting at {}", tip_hash);
                        }
                        NetworkMessage::Inv(inventory) => {
                            debug!("Received inventory with {} items", inventory.len());
                            let block_hashes: Vec<BlockHash> = inventory
                                .iter()
                                .filter_map(|inv| match inv {
                                    Inventory::Block(hash) => Some(*hash),
                                    _ => None,
                                })
                                .collect();

                            if !block_hashes.is_empty() {
                                n_requested_blocks += block_hashes.len();
                                debug!("Requesting {} blocks", block_hashes.len());
                                peer.send_message(create_getdata_message(block_hashes)).await.unwrap();
                            }
                        }
                        NetworkMessage::Block(bitcoin_block) => {
                            n_requested_blocks -= 1;
                            info!(
                                "Received block: {} from {}",
                                bitcoin_block.block_hash(),
                                addr
                            );
                            let tip = chainman.get_block_index_tip();
                            if bitcoin_block.header.prev_blockhash != get_block_hash(tip) {
                                debug!("This block is out of order!");
                            }

                            let kernel_block = bitcoin_block_to_kernel_block(&bitcoin_block);
                            match chainman.process_block(&kernel_block) {
                                Ok(()) => {}
                                Err(err) => {
                                    warn!("Process block error: {:?}", err);
                                    pending_blocks
                                        .insert(bitcoin_block.header.prev_blockhash, kernel_block);
                                    debug!("n_requested_blocks: {}", n_requested_blocks);
                                }
                            };
                            if n_requested_blocks == 0 {
                                while !pending_blocks.is_empty() {
                                    let tip_index = chainman.get_block_index_tip();
                                    info!(
                                        "Attempting to dump the pending blocks, current height: {}.",
                                        tip_index.height()
                                    );
                                    let tip_hash = get_block_hash(tip_index);
                                    if let Some(kernel_block) = pending_blocks.remove(&tip_hash) {
                                        match chainman.process_block(&kernel_block) {
                                            Ok(()) => {
                                                let height = chainman
                                                    .get_block_index_by_hash(bitcoinkernel::BlockHash {
                                                        hash: bitcoin_block.block_hash().to_byte_array(),
                                                    })
                                                    .unwrap()
                                                    .height();
                                                info!("Processed block at height: {}", height);
                                            }
                                            Err(err) => {
                                                warn!(
                                                    "Cannot retry again after process block error: {:?}",
                                                    err
                                                );
                                            }
                                        };
                                    } else {
                                        pending_blocks.clear();
                                        let tip_index = chainman.get_block_index_tip();
                                        let tip_hash = get_block_hash(tip_index);
                                        let getblocks = create_getblocks_message(tip_hash);
                                        peer.send_message(getblocks).await.unwrap();
                                    }
                                }
                            }
                        }
                        NetworkMessage::Ping(nonce) => {
                            peer.send_message(NetworkMessage::Pong(nonce)).await.unwrap();
                            info!("Received ping, sending pong");
                        }
                        _ => {
                            info!("Received other message: {:?}", msg);
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
            context.interrupt();
            info!("Received interrupt signal, shutting down...");
            return Ok(());
        }
        _ = shutdown_rx.recv() => {
            context.interrupt();
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
        &context,
    )
    .unwrap();

    chainman.load_chainstate(ChainstateLoadOptions::new()).unwrap();
    chainman.import_blocks().unwrap();

    info!("Bitcoin kernel initialized");

    let connect: Option<SocketAddr> = if let Some(connect) = args.connect {
        Some(connect.parse().unwrap())
    } else {
        None
    };

    if shutdown_rx.try_recv().is_ok() {
        info!("Shutting down!");
        return Ok(())
    }

    run_connection(
        args.network.into(),
        connect,
        chainman,
        &context,
        shutdown_rx,
    )
    .await
}
