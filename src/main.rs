use std::{
    io::{Cursor, Write},
    net::{SocketAddr, TcpStream},
    sync::Once,
    time::{SystemTime, UNIX_EPOCH},
};

use bitcoin::{
    consensus::{encode, Decodable},
    hashes::Hash,
    network::{
        constants::ServiceFlags,
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::{GetBlocksMessage, Inventory},
        message_network::VersionMessage,
        Address,
    },
    BlockHash, Network,
};
use libbitcoinkernel_sys::{
    BlockIndex, BlockManagerOptions, ChainType, ChainstateLoadOptions, ChainstateManager,
    ChainstateManagerOptions, Context, ContextBuilder, KernelNotificationInterfaceCallbackHolder,
    Log, Logger,
};
use log::info;

fn create_context() -> Context {
    ContextBuilder::new()
        .chain_type(ChainType::SIGNET)
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
        let raw_msg = RawNetworkMessage {
            magic: self.network.magic(),
            payload: msg,
        };
        let bytes = encode::serialize(&raw_msg);
        self.stream.write_all(&bytes)?;
        Ok(())
    }

    fn receive_message(&mut self) -> std::io::Result<NetworkMessage> {
        let raw_msg = RawNetworkMessage::consensus_decode(&mut self.stream)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        Ok(raw_msg.payload)
    }
}

// Version message for handshake
fn create_version_message(addr: SocketAddr) -> NetworkMessage {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    NetworkMessage::Version(VersionMessage::new(
        ServiceFlags::NONE,
        timestamp,
        Address::new(&addr, ServiceFlags::NONE),
        Address::new(&addr, ServiceFlags::NONE),
        0,
        "kernel-node".to_string(),
        0,
    ))
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
        .map(|hash| Inventory::Block(hash))
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

    // Initial handshake
    peer.send_message(create_version_message(addr))?;
    info!("Sent version message");

    // Basic message handling loop
    loop {
        match peer.receive_message() {
            Ok(msg) => match msg {
                NetworkMessage::Version(_) => {
                    info!("Received version");
                    peer.send_message(NetworkMessage::Verack)?;
                }
                NetworkMessage::Verack => {
                    info!("Received verack - handshake complete");

                    let genesis_index = chainman.get_block_index_genesis();
                    let hash = get_block_hash(genesis_index, &chainman).unwrap();

                    let getblocks = create_getblocks_message(hash);
                    peer.send_message(getblocks)?;
                    info!("Requested initial blocks starting at {}", hash);
                }
                NetworkMessage::Inv(inventory) => {
                    info!("Received inventory with {} items", inventory.len());

                    let block_hashes: Vec<BlockHash> = inventory
                        .iter()
                        .filter_map(|inv| match inv {
                            Inventory::Block(hash) => Some(*hash),
                            _ => None,
                        })
                        .collect();
                    info!("Received {} block hashes", block_hashes.len());

                    if !block_hashes.is_empty() {
                        info!("Requesting {} blocks", block_hashes.len());
                        peer.send_message(create_getdata_message(block_hashes))?;
                    }
                }
                NetworkMessage::Block(bitcoin_block) => {
                    info!("Received block: {} from {}", bitcoin_block.block_hash(), addr);
                    let kernel_block = bitcoin_block_to_kernel_block(&bitcoin_block);
                    let res = chainman.process_block(&kernel_block);
                    info!("Process block result: {:?}", res);
                    if res.is_ok() {
                        let height = chainman
                            .get_block_index_by_hash(bitcoin_block.block_hash().into_inner())
                            .unwrap()
                            .info()
                            .height;
                        info!("Processed block at height: {}", height);
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
    START.call_once(|| {
        setup_logging();
    });
    let context = create_context();
    let data_dir = "/home/drgrid/.kernel-node";
    let blocks_dir = data_dir.to_owned() + "/blocks";
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

    run_connection(Network::Signet, chainman).await
}
