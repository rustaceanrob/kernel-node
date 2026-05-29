use std::{
    collections::{HashMap, HashSet},
    fmt,
    net::SocketAddr,
    sync::{mpsc, Arc, Mutex},
};

use bitcoin::{
    hashes::Hash,
    p2p::{
        address::AddrV2Message,
        message::NetworkMessage,
        message_blockdata::{GetHeadersMessage, Inventory},
        Address, ServiceFlags,
    },
};
use bitcoin::{BlockHash, Network};
use bitcoinkernel::{
    core::BlockHashExt, prelude::BlockValidationStateExt, BlockTreeEntry, BlockValidationResult,
    ChainstateManager, Context, ProcessBlockHeaderResult, ValidationMode,
};
use log::{debug, info, warn};
use p2p::{
    handshake::{ConnectionConfig, ProtocolVersion},
    net::{ConnectionExt, ConnectionReader, ConnectionWriter, TimeoutParams},
};

use crate::{
    ext::{CrateBlockExt, CrateHeaderExt},
    logging::Category,
};

const PROTOCOL_VERSION: ProtocolVersion = 70015;
const MAX_LOCATOR_HASHES: usize = 101;
/// Maximum number of inventory items in a single `getdata` message
/// (Bitcoin P2P protocol limit). Exceeding this is a protocol violation
/// and peers will reset the connection.
const MAX_INV_SZ: usize = 50_000;

#[derive(Clone)]
pub struct TipState {
    pub block_hash: bitcoin::BlockHash,
}

impl Default for TipState {
    fn default() -> Self {
        Self {
            block_hash: BlockHash::all_zeros(),
        }
    }
}

pub struct NodeState {
    pub addr_tx: mpsc::Sender<Vec<AddrV2Message>>,
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

/// State Machine for setting up a connection and getting blocks from a peer.
///
/// After the initial headers sync the peer enters `Synced` and relies on
/// BIP-130 announcements: the remote pushes unsolicited `headers` for new
/// tips because we advertised `sendheaders` during the handshake. Legacy
/// `inv` announcements are still accepted as a fallback.
///
/// ```text
///       [*]
///        │
/// AwaitingHeaders ◄──── (non-connecting headers)
///        ▼                      ▲
///     Synced ──────────────────┘
///       ▲ │
///  Block│ │ Headers / Inv
///       │ ▼
///   AwaitingBlock
///       │ ▲
///       │ │
///       └─┘
///      Block
/// ```
#[derive(Default)]
pub enum PeerStateMachine {
    #[default]
    AwaitingHeaders,
    Synced,
    AwaitingBlock(AwaitingBlock),
}

pub struct AwaitingBlock {
    pub peer_inventory: HashSet<bitcoin::BlockHash>,
    pub block_buffer: HashMap<bitcoin::BlockHash /*prev */, bitcoinkernel::Block>,
}

fn build_block_locators(tip: BlockTreeEntry<'_>) -> Vec<BlockHash> {
    let height = tip.height();
    assert!(height >= 0);
    let mut locators = Vec::with_capacity(MAX_LOCATOR_HASHES);
    let mut entry = tip;
    let mut current_height = height as usize;
    let mut step: usize = 1;
    loop {
        let hash = BlockHash::from_byte_array(entry.block_hash().to_bytes());
        locators.push(hash);
        if current_height == 0 || locators.len() >= MAX_LOCATOR_HASHES {
            break;
        }
        if locators.len() > 10 {
            step *= 2;
        }
        let target = current_height.saturating_sub(step);
        while current_height > target {
            match entry.prev() {
                Some(prev) => {
                    entry = prev;
                    current_height -= 1;
                }
                None => break,
            }
        }
    }
    locators
}

fn create_getheaders_message(locator_hashes: Vec<bitcoin::BlockHash>) -> NetworkMessage {
    NetworkMessage::GetHeaders(GetHeadersMessage {
        version: PROTOCOL_VERSION,
        locator_hashes,
        stop_hash: bitcoin::BlockHash::all_zeros(),
    })
}

/// Returns the next batch of block hashes on the best-known header chain that
/// the active chain has not yet caught up to, in ascending height order.
/// Capped at [`MAX_INV_SZ`] so the resulting `getdata` does not exceed the
/// Bitcoin P2P inventory limit. Empty when the active chain matches the best
/// header tip.
fn pending_block_hashes(node_state: &NodeState) -> Vec<bitcoin::BlockHash> {
    let active_height = node_state.chainman.active_chain().height();
    let Some(best) = node_state.chainman.best_entry() else {
        return Vec::new();
    };
    if best.height() <= active_height {
        return Vec::new();
    }
    let remaining = (best.height() - active_height) as usize;
    let batch = remaining.min(MAX_INV_SZ);
    // Highest height to include in this batch (oldest-first download).
    let top_height = active_height + batch as i32;

    let mut entry = best;
    while entry.height() > top_height {
        match entry.prev() {
            Some(prev) => entry = prev,
            None => return Vec::new(),
        }
    }
    let mut hashes = Vec::with_capacity(batch);
    while entry.height() > active_height {
        hashes.push(BlockHash::from_byte_array(entry.block_hash().to_bytes()));
        match entry.prev() {
            Some(prev) => entry = prev,
            None => break,
        }
    }
    hashes.reverse();
    hashes
}

fn create_getdata_message(block_hashes: &[bitcoin::BlockHash]) -> NetworkMessage {
    let inventory: Vec<Inventory> = block_hashes
        .iter()
        .map(|hash| Inventory::WitnessBlock(*hash))
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
        info!(target: Category::NET, "Received ping, responding pong.");
        return (state_machine, vec![NetworkMessage::Pong(nonce)]);
    }

    if let NetworkMessage::AddrV2(payload) = event {
        info!(target: Category::NET, "Received {} net addresses", payload.len());
        // If the address manager has a full queue these net addresses should be dropped.
        let _ = node_state.addr_tx.send(payload);
        return (state_machine, vec![]);
    }

    match state_machine {
        PeerStateMachine::AwaitingHeaders => match event {
            NetworkMessage::Headers(headers) => {
                let msg_len = headers.len();
                for header in headers.into_iter() {
                    let result = node_state.chainman.process_block_header(&header.convert());
                    match result {
                        ProcessBlockHeaderResult::Success(state)
                            if state.mode() == ValidationMode::Valid =>
                        {
                            debug!(target: Category::KERNEL, "Processed header: {}", header.time);
                            continue;
                        }
                        _ => {
                            warn!(target: Category::KERNEL, "Rejected header {}", header.block_hash());
                            break;
                        }
                    }
                }

                if msg_len != 2000 {
                    let pending = pending_block_hashes(node_state);
                    if pending.is_empty() {
                        debug!(target: Category::NET, "Headers sync complete; chain is caught up");
                        return (PeerStateMachine::Synced, vec![]);
                    }
                    debug!(target: Category::NET, "Headers sync complete; requesting {} blocks", pending.len());
                    let getdata = create_getdata_message(&pending);
                    return (
                        PeerStateMachine::AwaitingBlock(AwaitingBlock {
                            peer_inventory: pending.into_iter().collect(),
                            block_buffer: HashMap::new(),
                        }),
                        vec![getdata],
                    );
                }

                let locators = build_block_locators(node_state.chainman.best_entry().unwrap());
                (
                    PeerStateMachine::AwaitingHeaders,
                    vec![create_getheaders_message(locators)],
                )
            }
            message => {
                debug!(target: Category::NET, "Ignoring message: {:?}", message);
                (PeerStateMachine::AwaitingHeaders, vec![])
            }
        },
        PeerStateMachine::Synced => match event {
            NetworkMessage::Headers(headers) => {
                handle_announced_headers(headers, node_state)
            }
            NetworkMessage::Inv(inventory) => {
                debug!(target: Category::NET, "Received inventory with {} items", inventory.len());
                let block_hashes: Vec<bitcoin::BlockHash> = inventory
                    .iter()
                    .filter_map(|inv| match inv {
                        Inventory::Block(hash) => Some(*hash),
                        _ => None,
                    })
                    .collect();

                if !block_hashes.is_empty() {
                    debug!(target: Category::NET, "Requesting {} blocks via inv fallback", block_hashes.len());
                    (
                        PeerStateMachine::AwaitingBlock(AwaitingBlock {
                            peer_inventory: block_hashes.iter().cloned().collect(),
                            block_buffer: HashMap::new(),
                        }),
                        vec![create_getdata_message(&block_hashes)],
                    )
                } else {
                    (PeerStateMachine::Synced, vec![])
                }
            }
            message => {
                debug!(target: Category::NET, "Ignoring message: {:?}", message);
                (PeerStateMachine::Synced, vec![])
            }
        },
        PeerStateMachine::AwaitingBlock(mut block_state) => match event {
            NetworkMessage::Block(block) => {
                let prev_blockhash = block.header.prev_blockhash;
                block_state.peer_inventory.remove(&block.block_hash());
                block_state
                    .block_buffer
                    .insert(prev_blockhash, block.convert());

                while let Some(next_block) = block_state
                    .block_buffer
                    .remove(&node_state.get_tip_state().block_hash)
                {
                    if let Err(err) = node_state.block_tx.send(next_block) {
                        debug!(target: Category::NODE, "Encountered error on block send: {}", err);
                        return (PeerStateMachine::AwaitingBlock(block_state), vec![]);
                    }
                }

                // All expected blocks received; either request the next IBD
                // batch or return to the announcement-driven Synced state.
                if block_state.peer_inventory.is_empty() {
                    block_state.block_buffer.clear();
                    let pending = pending_block_hashes(node_state);
                    if pending.is_empty() {
                        (PeerStateMachine::Synced, vec![])
                    } else {
                        debug!(target: Category::NET, "Requesting next IBD batch of {} blocks", pending.len());
                        let getdata = create_getdata_message(&pending);
                        (
                            PeerStateMachine::AwaitingBlock(AwaitingBlock {
                                peer_inventory: pending.into_iter().collect(),
                                block_buffer: HashMap::new(),
                            }),
                            vec![getdata],
                        )
                    }
                } else {
                    (PeerStateMachine::AwaitingBlock(block_state), vec![])
                }
            }
            message => {
                debug!(target: Category::NET, "Ignoring message: {:?}", message);
                (PeerStateMachine::AwaitingBlock(block_state), vec![])
            }
        },
    }
}

/// Handle an unsolicited `headers` message per BIP-130.
///
/// If the announced headers don't connect to our header tree, fall back to a
/// locator-based `getheaders` to fill the gap. Otherwise request the
/// announced blocks directly with a single `getdata`.
fn handle_announced_headers(
    headers: Vec<bitcoin::block::Header>,
    node_state: &mut NodeState,
) -> (PeerStateMachine, Vec<NetworkMessage>) {
    if headers.is_empty() {
        return (PeerStateMachine::Synced, vec![]);
    }
    debug!(target: Category::NET, "BIP-130 announcement of {} header(s)", headers.len());
    let mut announced_hashes = Vec::with_capacity(headers.len());
    for header in headers.iter() {
        let result = node_state.chainman.process_block_header(&header.convert());
        match result {
            ProcessBlockHeaderResult::Success(state)
                if state.mode() == ValidationMode::Valid =>
            {
                announced_hashes.push(header.block_hash());
            }
            ProcessBlockHeaderResult::Failed(state)
                if state.result() == BlockValidationResult::MissingPrev =>
            {
                debug!(target: Category::NET, "Announced headers do not connect; issuing getheaders");
                let locators = build_block_locators(node_state.chainman.best_entry().unwrap());
                return (
                    PeerStateMachine::AwaitingHeaders,
                    vec![create_getheaders_message(locators)],
                );
            }
            _ => {
                warn!(target: Category::KERNEL, "Rejected announced header {}", header.block_hash());
                return (PeerStateMachine::Synced, vec![]);
            }
        }
    }
    (
        PeerStateMachine::AwaitingBlock(AwaitingBlock {
            peer_inventory: announced_hashes.iter().cloned().collect(),
            block_buffer: HashMap::new(),
        }),
        vec![create_getdata_message(&announced_hashes)],
    )
}

pub struct BitcoinPeer {
    addr: Address,
    writer: Arc<ConnectionWriter>,
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
    ) -> Result<Self, p2p::net::Error> {
        let height = node_state.chainman.active_chain().height();
        let conf = ConnectionConfig::new()
            .change_network(network)
            .our_height(height)
            .request_addr()
            .set_service_requirement(ServiceFlags::NETWORK)
            .offer_services(ServiceFlags::WITNESS)
            .user_agent("/kernel-node:0.1.0/".into());
        let (writer, reader, _) = conf.open_connection(socket_addr, TimeoutParams::new())?;

        let addr = Address::new(&socket_addr, ServiceFlags::WITNESS);
        info!(target: Category::NET, "Connected to {:?}", addr);
        let locators = build_block_locators(node_state.chainman.best_entry().unwrap());
        debug!(target: Category::NET, "Sending headers message...");
        writer.send_message(create_getheaders_message(locators))?;
        let peer = BitcoinPeer {
            addr,
            writer: Arc::new(writer),
            reader,
            state_machine: PeerStateMachine::AwaitingHeaders,
        };
        Ok(peer)
    }

    pub fn writer(&self) -> Arc<ConnectionWriter> {
        Arc::clone(&self.writer)
    }

    fn receive_message(&mut self) -> Result<NetworkMessage, p2p::net::Error> {
        Ok(self
            .reader
            .read_message()?
            .expect("v1 only supported currently"))
    }

    pub fn receive_and_process_message(
        &mut self,
        node_state: &mut NodeState,
    ) -> Result<(), p2p::net::Error> {
        let msg = self.receive_message()?;
        let old_state = std::mem::take(&mut self.state_machine);
        let (peer_state_machine, mut messages) = process_message(old_state, msg, node_state);
        self.state_machine = peer_state_machine;
        for message in messages.drain(..) {
            self.writer.send_message(message)?
        }
        Ok(())
    }
}
