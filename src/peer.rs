use std::{
    collections::{HashMap, HashSet},
    fmt,
    net::SocketAddr,
    sync::{mpsc, Arc, Mutex},
};

use bitcoin::Network;
use bitcoinkernel::{ChainstateManager, Context};
use log::{debug, info};
use p2p::{
    handshake::ConnectionConfig,
    net::{ConnectionExt, ConnectionReader, ConnectionWriter, TimeoutParams},
    p2p_message_types::{
        message::{InventoryPayload, NetworkMessage},
        message_blockdata::{GetBlocksMessage, Inventory},
        message_network::UserAgent,
        Address, ProtocolVersion, ServiceFlags,
    },
};

use crate::bitcoin_block_to_kernel_block;

const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::INVALID_CB_NO_BAN_VERSION;

#[derive(Clone)]
pub struct TipState {
    pub block_hash: bitcoin::BlockHash,
}

pub struct NodeState {
    pub block_tx: mpsc::SyncSender<bitcoinkernel::Block>,
    pub tip_state: Arc<Mutex<TipState>>,
    pub context: Arc<Context>,
    pub chainman: Arc<ChainstateManager>,
}

impl NodeState {
    pub fn set_tip_state(&self, block_hash: bitcoin::BlockHash) {
        let mut state = self.tip_state.lock().unwrap();
        state.block_hash = block_hash;
    }

    pub fn get_tip_state(&self) -> TipState {
        let state = self.tip_state.lock().unwrap();
        state.clone()
    }
}

/// State Machine for setting up a connection and getting blocks from a peer
///
///       [*]
///        │
///        ▼
///   AwaitingInv
///       ▲ |
/// Block | | Inv
///       | ▼
///   AwaitingBlock
///       │ ▲
///       │ │
///       └─┘
///      Block
#[derive(Default)]
pub enum PeerStateMachine {
    #[default]
    AwaitingInv,
    AwaitingBlock(AwaitingBlock),
}

pub struct AwaitingBlock {
    pub peer_inventory: HashSet<bitcoin::BlockHash>,
    pub block_buffer: HashMap<bitcoin::BlockHash /*prev */, bitcoinkernel::Block>,
}

fn create_getblocks_message(known_block_hash: bitcoin::BlockHash) -> NetworkMessage {
    NetworkMessage::GetBlocks(GetBlocksMessage {
        version: PROTOCOL_VERSION,
        locator_hashes: vec![known_block_hash],
        stop_hash: bitcoin::BlockHash::GENESIS_PREVIOUS_BLOCK_HASH,
    })
}

fn create_getdata_message(block_hashes: &[bitcoin::BlockHash]) -> NetworkMessage {
    let inventory: Vec<Inventory> = block_hashes
        .iter()
        .map(|hash| Inventory::WitnessBlock(*hash))
        .collect();

    NetworkMessage::GetData(InventoryPayload(inventory))
}

pub fn process_message(
    state_machine: PeerStateMachine,
    event: NetworkMessage,
    node_state: &mut NodeState,
) -> (PeerStateMachine, Vec<NetworkMessage>) {
    // Always process the ping first as a special case.
    if let NetworkMessage::Ping(nonce) = event {
        info!("Received ping, responding pong.");
        return (state_machine, vec![NetworkMessage::Pong(nonce)]);
    }

    match state_machine {
        PeerStateMachine::AwaitingInv => match event {
            NetworkMessage::Inv(inventory) => {
                debug!("Received inventory with {} items", inventory.0.len());
                let block_hashes: Vec<bitcoin::BlockHash> = inventory
                    .0
                    .iter()
                    .filter_map(|inv| match inv {
                        Inventory::Block(hash) => Some(*hash),
                        _ => None,
                    })
                    .collect();

                if !block_hashes.is_empty() {
                    debug!("Requesting {} blocks", block_hashes.len());
                    (
                        PeerStateMachine::AwaitingBlock(AwaitingBlock {
                            peer_inventory: block_hashes.iter().cloned().collect(),
                            block_buffer: HashMap::new(),
                        }),
                        vec![create_getdata_message(&block_hashes)],
                    )
                } else {
                    (PeerStateMachine::AwaitingInv, vec![])
                }
            }
            message => {
                debug!("Ignoring message: {:?}", message);
                (PeerStateMachine::AwaitingInv, vec![])
            }
        },
        PeerStateMachine::AwaitingBlock(mut block_state) => match event {
            NetworkMessage::Block(block) => {
                let block = block.assume_checked(None);
                let prev_blockhash = block.header().prev_blockhash;
                block_state.peer_inventory.remove(&block.block_hash());
                block_state
                    .block_buffer
                    .insert(prev_blockhash, bitcoin_block_to_kernel_block(&block));

                while let Some(next_block) = block_state
                    .block_buffer
                    .remove(&node_state.get_tip_state().block_hash)
                {
                    if let Err(err) = node_state.block_tx.send(next_block) {
                        debug!("Encountered error on block send: {}", err);
                        return (PeerStateMachine::AwaitingBlock(block_state), vec![]);
                    }
                }

                // If all to be expected blocks were received, clear any
                // remaining blocks in the buffer and request a fresh batch of
                // blocks.
                if block_state.peer_inventory.is_empty() {
                    block_state.block_buffer.clear();
                    let our_best = node_state.get_tip_state().block_hash;
                    (
                        PeerStateMachine::AwaitingInv,
                        vec![create_getblocks_message(our_best)],
                    )
                } else {
                    (PeerStateMachine::AwaitingBlock(block_state), vec![])
                }
            }
            message => {
                debug!("Ignoring message: {:?}", message);
                (PeerStateMachine::AwaitingBlock(block_state), vec![])
            }
        },
    }
}

pub struct BitcoinPeer {
    addr: Address,
    writer: ConnectionWriter,
    reader: ConnectionReader,
    state_machine: PeerStateMachine,
}

impl fmt::Display for BitcoinPeer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.addr)
    }
}

impl BitcoinPeer {
    pub fn new(
        socket_addr: SocketAddr,
        network: Network,
        node_state: &mut NodeState,
    ) -> std::io::Result<Self> {
        let height = node_state.chainman.active_chain().height();
        let conf = ConnectionConfig::new()
            .change_network(network)
            .our_height(height)
            .set_service_requirement(ServiceFlags::NETWORK)
            .offer_services(ServiceFlags::WITNESS)
            .user_agent(UserAgent::from_nonstandard("kernel-node"));
        let (writer, reader, _) = conf
            .open_connection(socket_addr, TimeoutParams::new())
            .unwrap();

        let addr = Address::new(&socket_addr, ServiceFlags::WITNESS);
        info!("Connected to {:?}", addr);
        let state_machine = PeerStateMachine::AwaitingInv;
        let peer = BitcoinPeer {
            addr,
            writer,
            reader,
            state_machine,
        };
        let our_best = node_state.get_tip_state().block_hash;
        let getblocks = create_getblocks_message(our_best);
        peer.send_message(getblocks).unwrap();
        Ok(peer)
    }

    pub fn send_message(&self, msg: NetworkMessage) -> std::io::Result<()> {
        self.writer.send_message(msg).unwrap();
        Ok(())
    }

    fn receive_message(&mut self) -> std::io::Result<NetworkMessage> {
        Ok(self.reader.read_message().unwrap().unwrap())
    }

    pub fn receive_and_process_message(
        &mut self,
        node_state: &mut NodeState,
    ) -> std::io::Result<()> {
        let msg = self.receive_message()?;
        let old_state = std::mem::take(&mut self.state_machine);
        let (peer_state_machine, mut messages) = process_message(old_state, msg, node_state);
        self.state_machine = peer_state_machine;
        for message in messages.drain(..) {
            self.send_message(message)?;
        }
        Ok(())
    }
}
