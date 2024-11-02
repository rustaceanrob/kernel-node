use std::{
    collections::HashMap,
    fs,
    io::{Cursor, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::Once,
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
use clap::{Parser, ValueEnum};
use home::home_dir;
use libbitcoinkernel_sys::{
    BlockIndex, BlockManagerOptions, ChainType, ChainstateLoadOptions, ChainstateManager,
    ChainstateManagerOptions, Context, ContextBuilder, KernelNotificationInterfaceCallbackHolder,
    Log, Logger,
};
use log::{debug, info, warn};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Which Bitcoin network to use
    #[arg(value_enum, short, long, default_value = "signet")]
    network: BitcoinNetwork,

    /// Data directory for blockchain and configuration
    #[arg(long, default_value = "~/.kernel-node")]
    datadir: String,
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

impl From<BitcoinNetwork> for libbitcoinkernel_sys::ChainType {
    fn from(network: BitcoinNetwork) -> Self {
        match network {
            BitcoinNetwork::Mainnet => libbitcoinkernel_sys::ChainType::MAINNET,
            BitcoinNetwork::Testnet => libbitcoinkernel_sys::ChainType::TESTNET,
            BitcoinNetwork::Signet => libbitcoinkernel_sys::ChainType::SIGNET,
            BitcoinNetwork::Regtest => libbitcoinkernel_sys::ChainType::REGTEST,
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

fn create_context(chain_type: ChainType) -> Context {
    ContextBuilder::new()
        .chain_type(chain_type)
        .kn_callbacks(Box::new(KernelNotificationInterfaceCallbackHolder {
            kn_block_tip: Box::new(|_state, _block_index| {}),
            kn_header_tip: Box::new(|_state, _height, _timestamp, _presync| {}),
            kn_progress: Box::new(|_title, _progress, _resume_possible| {}),
            kn_warning_set: Box::new(|_warning, _message| {}),
            kn_warning_unset: Box::new(|_warning| {}),
            kn_flush_error: Box::new(|_message| {}),
            kn_fatal_error: Box::new(|_message| {}),
        }))
        .build()
        .unwrap()
}

struct KernelLog {}

impl Log for KernelLog {
    fn log(&self, message: &str) {
        log::info!(
            target: "libbitcoinkernel", 
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

struct BitcoinPeer {
    stream: TcpStream,
    network: Network,
}

impl BitcoinPeer {
    fn new(addr: SocketAddr, network: Network) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        Ok(BitcoinPeer { stream, network })
    }

    fn send_message(&mut self, msg: NetworkMessage) -> std::io::Result<()> {
        let raw_msg = RawNetworkMessage::new(self.network.magic(), msg);
        let bytes = encode::serialize(&raw_msg);
        self.stream.write_all(&bytes)?;
        Ok(())
    }

    fn receive_message(&mut self) -> std::io::Result<NetworkMessage> {
        let raw_msg = RawNetworkMessage::consensus_decode(&mut self.stream)
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

fn deserialize_block(raw_data: Vec<u8>) -> Result<bitcoin::Block, encode::Error> {
    let mut cursor = Cursor::new(raw_data);
    encode::Decodable::consensus_decode(&mut cursor)
}

fn get_block_hash(
    index: BlockIndex,
    chainman: &ChainstateManager,
) -> Result<BlockHash, encode::Error> {
    Ok(deserialize_block(chainman.read_block_data(&index).unwrap().into())?.block_hash())
}

fn bitcoin_block_to_kernel_block(block: &bitcoin::Block) -> libbitcoinkernel_sys::Block {
    let ser_block = encode::serialize(block);
    libbitcoinkernel_sys::Block::try_from(ser_block.as_slice()).unwrap()
}

async fn run_connection(network: Network, chainman: ChainstateManager<'_>) -> std::io::Result<()> {
    let addr: SocketAddr = "127.0.0.1:38333".parse().unwrap();
    let mut peer = BitcoinPeer::new(addr, network)?;
    info!("Connected to peer");

    let height = chainman.get_block_index_tip().info().height;
    // Initial handshake
    peer.send_message(create_version_message(addr, height))?;
    info!("Sent version message");

    // Out of order block handling
    let mut pending_blocks: HashMap<
        BlockHash,                   /*prev */
        libbitcoinkernel_sys::Block, /*block */
    > = HashMap::new();
    let mut n_requested_blocks = 0;

    // Basic message handling loop
    loop {
        match peer.receive_message() {
            Ok(msg) => match msg {
                NetworkMessage::Version(version) => {
                    info!("Received version: {:?}", version);
                    peer.send_message(NetworkMessage::Verack)?;
                }
                NetworkMessage::Verack => {
                    info!("Received verack - handshake complete");

                    let tip_index = chainman.get_block_index_tip();
                    let tip_hash = get_block_hash(tip_index, &chainman).unwrap();

                    let getblocks = create_getblocks_message(tip_hash);
                    peer.send_message(getblocks)?;
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
                        peer.send_message(create_getdata_message(block_hashes))?;
                    }
                }
                NetworkMessage::Block(bitcoin_block) => {
                    n_requested_blocks -= 1;
                    debug!(
                        "Received block: {} from {}",
                        bitcoin_block.block_hash(),
                        addr
                    );
                    let tip = chainman.get_block_index_tip();
                    if bitcoin_block.header.prev_blockhash
                        != get_block_hash(tip, &chainman).unwrap()
                    {
                        debug!("This block is out of order!");
                    }

                    let kernel_block = bitcoin_block_to_kernel_block(&bitcoin_block);
                    match chainman.process_block(&kernel_block) {
                        Ok(()) => {}
                        Err(err) => {
                            debug!("Process block error: {:?}", err);
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
                                tip_index.info().height
                            );
                            let tip_hash = get_block_hash(tip_index, &chainman).unwrap();
                            if let Some(kernel_block) = pending_blocks.remove(&tip_hash) {
                                match chainman.process_block(&kernel_block) {
                                    Ok(()) => {
                                        let height = chainman
                                            .get_block_index_by_hash(
                                                bitcoin_block.block_hash().to_byte_array(),
                                            )
                                            .unwrap()
                                            .info()
                                            .height;
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
                                chainman.import_blocks().unwrap();
                                pending_blocks.clear();
                                let tip_index = chainman.get_block_index_tip();
                                let tip_hash = get_block_hash(tip_index, &chainman).unwrap();
                                let getblocks = create_getblocks_message(tip_hash);
                                peer.send_message(getblocks)?;
                            }
                        }
                    }
                }
                NetworkMessage::Ping(nonce) => {
                    peer.send_message(NetworkMessage::Pong(nonce))?;
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

    Ok(())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    START.call_once(|| {
        setup_logging();
    });
    let context = create_context(args.network.into());
    let data_dir = args.get_data_dir();
    let blocks_dir = data_dir.clone() + "/blocks";
    let chainman = ChainstateManager::new(
        ChainstateManagerOptions::new(&context, &data_dir).unwrap(),
        BlockManagerOptions::new(&context, &blocks_dir).unwrap(),
        &context,
    )
    .unwrap();

    chainman
        .load_chainstate(ChainstateLoadOptions::new())
        .unwrap();
    chainman.import_blocks().unwrap();

    info!("Bitcoin kernel initialized");

    run_connection(args.network.into(), chainman).await
}
