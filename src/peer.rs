use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use bitcoin::{
    hashes::Hash,
    p2p::{
        message::NetworkMessage,
        message_blockdata::{GetBlocksMessage, Inventory},
        message_network::VersionMessage,
        Address, ServiceFlags,
    },
};
use bitcoinkernel::{ChainstateManager, Context};
use log::{debug, info};

use crate::bitcoin_block_to_kernel_block;

pub struct NodeState {
    pub best_block: bitcoin::BlockHash,
    pub height: i32,
    pub context: Arc<Context>,
    pub chainman: ChainstateManager,
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
pub enum PeerStateMachine {
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
    pub block_buffer: HashMap<bitcoin::BlockHash /*prev */, bitcoin::Block>,
}

fn create_version_message(addr: Address, height: i32) -> NetworkMessage {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let mut version_message = VersionMessage::new(
        ServiceFlags::WITNESS,
        timestamp,
        addr,
        // addr_from is unused.
        Address::new(&SocketAddr::from(([0, 0, 0, 0], 0)), ServiceFlags::NONE),
        0,
        "kernel-node".to_string(),
        height,
    );
    version_message.version = 70015;

    NetworkMessage::Version(version_message)
}

fn create_getblocks_message(known_block_hash: bitcoin::BlockHash) -> NetworkMessage {
    NetworkMessage::GetBlocks(GetBlocksMessage {
        version: 70015,
        locator_hashes: vec![known_block_hash],
        stop_hash: bitcoin::BlockHash::all_zeros(),
    })
}

fn create_getdata_message(block_hashes: &Vec<bitcoin::BlockHash>) -> NetworkMessage {
    let inventory: Vec<Inventory> = block_hashes
        .into_iter()
        .map(|hash| Inventory::WitnessBlock(hash.clone()))
        .collect();

    NetworkMessage::GetData(inventory)
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
                    addrs[0].1.clone(),
                    node_state.height,
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
                let our_height = node_state.height;
                if our_height < handshake_state.peer_height {
                    let our_best = node_state.best_block;
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
                debug!("Received inventory with {} items", inventory.len());
                let block_hashes: Vec<bitcoin::BlockHash> = inventory
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
                let prev_blockhash = block.header.prev_blockhash;
                block_state.peer_inventory.remove(&block.block_hash());
                block_state.block_buffer.insert(prev_blockhash, block);

                while let Some(next_block) = block_state
                    .block_buffer
                    .remove(&node_state.best_block)
                {
                    debug!("Validating block: {}", next_block.block_hash());
                    node_state
                        .chainman
                        .process_block(&bitcoin_block_to_kernel_block(&next_block))
                        .unwrap();
                    node_state.best_block = next_block.block_hash();
                    node_state.height = node_state.chainman.get_block_index_tip().height();
                    debug!("Completed validating the block: {}", next_block.block_hash());
                }

                // If all to be expected blocks were received, clear any
                // remaining blocks in the buffer and request a fresh batch of
                // blocks.
                if block_state.peer_inventory.is_empty() {
                    block_state.block_buffer.clear();
                    let our_best = node_state.best_block;
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
