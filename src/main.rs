use std::{
    collections::HashMap,
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
use log::{debug, info, warn};

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

    let mut pending_blocks: HashMap<
        BlockHash,                   /*prev */
        libbitcoinkernel_sys::Block, /*block */
    > = HashMap::new();
    let mut n_requested_blocks = 0;

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

                    let tip_index = chainman.get_block_index_tip();
                    let tip_hash = get_block_hash(tip_index, &chainman).unwrap();

                    let getblocks = create_getblocks_message(tip_hash);
                    peer.send_message(getblocks)?;
                    info!("Requested initial blocks starting at {}", tip_hash);
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
                        n_requested_blocks += block_hashes.len();
                        info!("Requesting {} blocks", block_hashes.len());
                        peer.send_message(create_getdata_message(block_hashes))?;
                    }
                }
                NetworkMessage::Block(bitcoin_block) => {
                    n_requested_blocks -= 1;
                    let tip = chainman.get_block_index_tip();
                    debug!(
                        "Received block: {} with prev: {} on tip: {} from {}",
                        bitcoin_block.block_hash(),
                        bitcoin_block.header.prev_blockhash,
                        get_block_hash(tip, &chainman).unwrap(),
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
                        Ok(()) => {
                            let height = chainman
                                .get_block_index_by_hash(bitcoin_block.block_hash().into_inner())
                                .unwrap()
                                .info()
                                .height;
                            info!("Processed block at height: {}", height);
                        }
                        Err(err) => {
                            debug!("Process block error: {:?}", err);
                            pending_blocks
                                .insert(bitcoin_block.header.prev_blockhash, kernel_block);
                            debug!("n_requested_blocks: {}", n_requested_blocks);
                        }
                    };
                    if n_requested_blocks == 0 {
                        while !pending_blocks.is_empty() {
                            info!("Attempting to dump the pending blocks.");
                            let tip_index = chainman.get_block_index_tip();
                            let tip_hash = get_block_hash(tip_index, &chainman).unwrap();
                            if let Some(kernel_block) = pending_blocks.remove(&tip_hash) {
                                match chainman.process_block(&kernel_block) {
                                    Ok(()) => {
                                        let height = chainman
                                            .get_block_index_by_hash(
                                                bitcoin_block.block_hash().into_inner(),
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
