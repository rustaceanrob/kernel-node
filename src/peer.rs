use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    ops::Deref,
    sync::{mpsc, Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use bitcoin::{absolute::Decodable, consensus::encode};

use bitcoin::Network;
use bitcoin_messages::{
    message::{NetworkMessage, RawNetworkMessage},
    message_blockdata::{GetBlocksMessage, Inventory},
    message_network::VersionMessage,
    Address, ServiceFlags,
};
use bitcoinkernel::{ChainstateManager, Context};
use log::{debug, info};
use p2p::p2p::{
    self as bitcoin_messages,
    message::{InventoryPayload, V1MessageHeader},
    message_network::UserAgent,
    NetworkExt, ProtocolVersion,
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
/// StartConnection
///        │
///        │ Addr
///        ▼
///    Handshake -------- Verack, Version
///        |         ▲  |
///        │         |__|
///        |
///        │ Verack +   
///        │ Version    
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
    StartConnection,
    Handshake(Handshake),
    AwaitingInv,
    AwaitingBlock(AwaitingBlock),
}

pub struct Handshake {
    pub got_ack: bool,
    pub peer_height: i32,
}

pub struct AwaitingBlock {
    pub peer_inventory: HashSet<bitcoin::BlockHash>,
    pub block_buffer: HashMap<bitcoin::BlockHash /*prev */, bitcoinkernel::Block>,
}

fn create_version_message(addr: Address, height: i32) -> NetworkMessage {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let version_message = VersionMessage::new(
        PROTOCOL_VERSION,
        ServiceFlags::WITNESS,
        timestamp,
        addr,
        // addr_from is unused.
        Address::new(&SocketAddr::from(([0, 0, 0, 0], 0)), ServiceFlags::NONE),
        0,
        UserAgent::from_nonstandard("kernel-node"),
        height,
    );

    NetworkMessage::Version(version_message)
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
        PeerStateMachine::StartConnection => match event {
            NetworkMessage::Addr(addrs) => (
                PeerStateMachine::Handshake(Handshake {
                    got_ack: false,
                    peer_height: 0,
                }),
                vec![create_version_message(
                    addrs.0[0].1.clone(),
                    node_state.chainman.active_chain().height(),
                )],
            ),
            _ => panic!("This should be controlled by the user, so no way to reach here."),
        },
        PeerStateMachine::Handshake(mut handshake_state) => {
            match event {
                NetworkMessage::Verack => {
                    debug!("Received verack");
                    handshake_state.got_ack = true;
                }
                NetworkMessage::Version(version) => {
                    debug!("Received the peer's version");
                    handshake_state.peer_height = version.start_height;
                }
                message => debug!("Ignoring message: {:?}", message),
            };
            if handshake_state.got_ack && handshake_state.peer_height > 0 {
                let mut messages = vec![NetworkMessage::Verack];
                let our_height = node_state.chainman.active_chain().height();
                if our_height < handshake_state.peer_height {
                    let our_best = node_state.get_tip_state().block_hash;
                    messages.push(create_getblocks_message(our_best));
                }
                debug!("Moving to AwaitingInv.");
                (PeerStateMachine::AwaitingInv, messages)
            } else {
                debug!("Looping to Handshake");
                (PeerStateMachine::Handshake(handshake_state), vec![])
            }
        }
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
    pub stream: Arc<TcpStream>,
    network: Network,
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
        let stream = Arc::new(TcpStream::connect(socket_addr)?);
        let addr = Address::new(&socket_addr, ServiceFlags::WITNESS);
        info!("Connected to {:?}", addr);

        let (state_machine, mut messages) = process_message(
            PeerStateMachine::StartConnection,
            NetworkMessage::Addr(bitcoin_messages::message::AddrPayload(vec![(
                0,
                addr.clone(),
            )])),
            node_state,
        );
        let mut peer = BitcoinPeer {
            addr,
            stream,
            network,
            state_machine,
        };
        for message in messages.drain(..) {
            peer.send_message(message).unwrap();
        }
        Ok(peer)
    }

    pub fn send_message(&mut self, msg: NetworkMessage) -> std::io::Result<()> {
        let raw_msg = RawNetworkMessage::new(self.network.default_network_magic(), msg);
        let bytes = encode::serialize(&raw_msg);
        (&*self.stream).write_all(&bytes)?;
        Ok(())
    }

    fn receive_message(&mut self) -> std::io::Result<RawNetworkMessage> {
        let mut message = vec![0; 24];
        self.stream.deref().read_exact(&mut message)?;
        let message_header = V1MessageHeader::consensus_decode(&mut message.as_slice()).unwrap();
        let mut rest = vec![0; message_header.length as usize];
        self.stream.deref().read_exact(&mut rest)?;
        message.extend(rest);
        match RawNetworkMessage::consensus_decode(&mut message.as_slice()) {
            Ok(raw_msg) => Ok(raw_msg),
            Err(bitcoin::consensus::encode::Error::Io(err)) => {
                let io_err: std::io::Error = err.into();
                if io_err.kind() == std::io::ErrorKind::ConnectionAborted
                    || io_err.kind() == std::io::ErrorKind::BrokenPipe
                    || io_err.kind() == std::io::ErrorKind::UnexpectedEof
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "Connection was shutdown",
                    ));
                }
                Err(io_err)
            }
            Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            )),
        }
    }

    pub fn receive_and_process_message(
        &mut self,
        node_state: &mut NodeState,
    ) -> std::io::Result<()> {
        let msg = self.receive_message()?;
        let old_state = std::mem::take(&mut self.state_machine);
        let (peer_state_machine, mut messages) =
            process_message(old_state, msg.into_payload(), node_state);
        self.state_machine = peer_state_machine;
        for message in messages.drain(..) {
            self.send_message(message)?;
        }
        Ok(())
    }
}
