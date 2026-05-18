use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    ops::DerefMut,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex, Once,
    },
    thread::{self, available_parallelism},
    time::{Duration, Instant},
};

use bitcoin::p2p::{
    address::{AddrV2, AddrV2Message},
    ServiceFlags,
};
use bitcoin::{hashes::Hash, BlockHash, Network};
use bitcoinkernel::{
    core::BlockHashExt, prelude::BlockValidationStateExt, ChainType, ChainstateManager,
    ChainstateManagerBuilder, Context, ContextBuilder, Log, Logger, SynchronizationState,
    ValidationMode,
};
use kernel_node::{
    daemonize::Daemonize,
    ext::{ChainExt, DirnameExt, NetworkExt},
    ipc::IpcInterface,
    logging::Category,
    peer::{BitcoinPeer, NodeState, TipState},
    server_capnp::server,
};
use log::{debug, error, info, warn};
use p2p::dns::{BITCOIN_SEEDS, SIGNET_SEEDS, TESTNET3_SEEDS, TESTNET4_SEEDS};
use tokio::net::UnixListener;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use wallet::silentpayments::Wallet;

const TABLE_WIDTH: usize = 16;
const TABLE_SLOT: usize = 16;
const MAX_BUCKETS: usize = 4;

const DNS_RESOLVER: IpAddr = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));

const STALE_BLOCK_DURATION: Duration = Duration::from_secs(60 * 20);

configure_me::include_config!();

fn scan_kernel_block(
    chainman: &ChainstateManager,
    kernel_block: &bitcoinkernel::Block,
    entry: &bitcoinkernel::BlockTreeEntry,
    wallet: &Arc<Mutex<Wallet>>,
) {
    let block_height = entry.height() as u32;

    let spent_outputs = match chainman.read_spent_outputs(entry) {
        Ok(u) => u,
        Err(e) => {
            warn!(target: Category::WALLET, "Reading spent outputs failed at height {block_height}: {e}");
            return;
        }
    };

    let count = wallet
        .lock()
        .unwrap()
        .scan_block(kernel_block, &spent_outputs, block_height);
    if count > 0 {
        info!(
            target: Category::WALLET,
            "Found {} silent payment(s) at height {}",
            count, block_height
        );
    }
}

fn create_context(
    chain_type: ChainType,
    shutdown_tx: mpsc::Sender<()>,
    tip_state: &Arc<Mutex<TipState>>,
    wallet: Arc<Mutex<Wallet>>,
    chainman_holder: Arc<std::sync::OnceLock<Arc<ChainstateManager>>>,
) -> Arc<Context> {
    let shutdown_triggered = Arc::new(AtomicBool::new(false));
    let shutdown_triggered_clone = Arc::clone(&shutdown_triggered);
    let shutdown_tx_clone = shutdown_tx.clone();
    let tip_state_clone = tip_state.clone();
    let wallet_for_disconnect = Arc::clone(&wallet);
    Arc::new(ContextBuilder::new()
        .chain_type(chain_type)
        .with_block_connected_validation(move |block: bitcoinkernel::Block, entry: bitcoinkernel::BlockTreeEntry<'_>| {
            if wallet.lock().unwrap().keys.is_none() {
                return;
            }
            let Some(chainman) = chainman_holder.get() else { return };
            scan_kernel_block(chainman.as_ref(), &block, &entry, &wallet);
        })
        .with_block_disconnected_validation(move |block: bitcoinkernel::Block, entry: bitcoinkernel::BlockTreeEntry<'_>| {
            if wallet_for_disconnect.lock().unwrap().keys.is_none() {
                return;
            }
            let height = entry.height();
            wallet_for_disconnect
                .lock()
                .unwrap()
                .process_disconnect(&block);
            info!(target: Category::WALLET, "Disconnected block at height {}", height);
        })
        .with_block_tip_notification(|state, hash: bitcoinkernel::BlockHash, _| {
                let hash = BlockHash::from_byte_array(hash.into());
                match state {
                    SynchronizationState::InitDownload => debug!(target: Category::KERNEL, "Received new block tip {} during IBD.", hash),
                    SynchronizationState::PostInit => debug!(target: Category::KERNEL, "Received new block {}", hash),
                    SynchronizationState::InitReindex => debug!(target: Category::KERNEL, "Moved new block tip {} during reindex.", hash),
                };
        })
        .with_header_tip_notification(|state, height, timestamp, presync| {
                match state {
                    SynchronizationState::InitDownload => debug!(target: Category::KERNEL, "Received new header tip during IBD at height {} and time {}. Presync mode: {}", height, timestamp, presync),
                    SynchronizationState::PostInit => info!(target: Category::KERNEL, "Received new header tip at height {} and time {}. Presync mode: {}", height, timestamp, presync),
                    SynchronizationState::InitReindex => debug!(target: Category::KERNEL, "Moved to new header tip during reindex at height {} and time {}. Presync mode: {}", height, timestamp, presync),
                }
        })
        .with_progress_notification(|title, progress, resume_possible| {
                warn!(target: Category::KERNEL, "Made progress {}: {}. Can resume: {}", title, progress, resume_possible)
        })
        .with_warning_set_notification(|_warning, _message| {})
        .with_warning_unset_notification(|_warning| {})
        .with_flush_error_notification(move |message| {
                if !shutdown_triggered.swap(true, Ordering::SeqCst) {
                    shutdown_tx.send(()).expect("failed to send shutdown signal");
                }
                error!(target: Category::KERNEL, "Fatal flush error encountered: {}", message);
        })
        .with_fatal_error_notification(move |message| {
                error!(target: Category::KERNEL, "Fatal error encountered: {}", message);
                if !shutdown_triggered_clone.swap(true, Ordering::SeqCst) {
                    shutdown_tx_clone.send(()).expect("failed to send shutdown signal");
                }
        })
        // .with_block_checked_validation(setup_validation_interface(tip_state))
        .with_block_checked_validation(move |block: bitcoinkernel::Block, state: bitcoinkernel::BlockValidationStateRef<'_>| {
            match state.mode() {
                ValidationMode::Valid => {
                    let hash = bitcoin::BlockHash::from_byte_array(block.hash().into());
                    log::debug!(target: Category::KERNEL, "Validation interface: Successfully checked block: {}", hash);
                    tip_state_clone.lock().unwrap().block_hash = hash;
                }
                _ => error!(target: Category::KERNEL, "Received an invalid block!"),
            }
        })
        .build()
        .unwrap())
}

struct KernelLog {}

impl Log for KernelLog {
    fn log(&self, message: &str) {
        log::info!(
            target: Category::KERNEL,
            "{}", message.strip_suffix("\r\n").or_else(|| message.strip_suffix('\n')).unwrap_or(message));
    }
}

static START: Once = Once::new();
static mut GLOBAL_LOG_CALLBACK_HOLDER: Option<Logger> = None;

fn setup_logging() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder.init();

    unsafe { GLOBAL_LOG_CALLBACK_HOLDER = Some(Logger::new(KernelLog {}).unwrap()) };
}

// fn setup_validation_interface(
//     tip_state: &Arc<Mutex<TipState>>,
// ) -> Box<ValidationInterfaceCallbacks> {
//     let tip_state_clone = Arc::clone(&tip_state);
//     Box::new(ValidationInterfaceCallbacks {
//         block_checked: Box::new(move |block, mode, _result| match mode {
//             ValidationMode::Valid => {
//                 let hash = bitcoin::BlockHash::from_byte_array(block.get_hash().hash);
//                 log::debug!("Validation interface: Successfully checked block: {}", hash);
//                 tip_state_clone.lock().unwrap().block_hash = hash;
//             }
//             _ => error!("Received an invalid block!"),
//         }),
//     })
// }

fn resolve_seeds(network: Network) -> Vec<IpAddr> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let format_hostname = |host: &str| format!("{host}:53");
    let seeds: Vec<String> = match network {
        Network::Bitcoin => BITCOIN_SEEDS.into_iter().map(format_hostname).collect(),
        Network::Signet => SIGNET_SEEDS.into_iter().map(format_hostname).collect(),
        Network::Testnet => TESTNET3_SEEDS.into_iter().map(format_hostname).collect(),
        Network::Testnet4 => TESTNET4_SEEDS.into_iter().map(format_hostname).collect(),
        Network::Regtest => Vec::new(),
    };
    let mut results = Vec::new();
    for host in seeds {
        let peers = rt.block_on(async move {
            tokio::net::lookup_host(host)
                .await
                .map(|sockets| sockets.map(|socket| socket.ip()).collect())
                .unwrap_or(Vec::new())
        });
        results.extend(peers);
    }
    results
}

fn run(
    network: Network,
    connect: Option<SocketAddr>,
    mut node_state: NodeState,
    shutdown_rx: mpsc::Receiver<()>,
    addr_rx: mpsc::Receiver<Vec<AddrV2Message>>,
    block_rx: mpsc::Receiver<bitcoinkernel::Block>,
) -> std::io::Result<()> {
    let mut table = addrman::Table::<TABLE_WIDTH, TABLE_SLOT, MAX_BUCKETS>::new();
    match connect {
        Some(connect) => {
            let record = match connect.ip() {
                IpAddr::V4(ipv4) => addrman::Record::new(
                    AddrV2::Ipv4(ipv4),
                    connect.port(),
                    ServiceFlags::NETWORK,
                    &DNS_RESOLVER,
                ),
                IpAddr::V6(ipv6) => addrman::Record::new(
                    AddrV2::Ipv6(ipv6),
                    connect.port(),
                    ServiceFlags::NETWORK,
                    &DNS_RESOLVER,
                ),
            };
            table.add(&record);
        }
        None => {
            let addresses = resolve_seeds(network);
            info!(target: Category::NET, "Resolved {} addresses from DNS seeds", addresses.len());
            for addr in &addresses {
                let record = match addr {
                    IpAddr::V4(ipv4) => addrman::Record::new(
                        AddrV2::Ipv4(*ipv4),
                        network.default_p2p_port(),
                        ServiceFlags::NETWORK,
                        &DNS_RESOLVER,
                    ),
                    IpAddr::V6(ipv6) => addrman::Record::new(
                        AddrV2::Ipv6(*ipv6),
                        network.default_p2p_port(),
                        ServiceFlags::NETWORK,
                        &DNS_RESOLVER,
                    ),
                };
                table.add(&record);
            }
        }
    };

    let chainman = Arc::clone(&node_state.chainman);
    let context = Arc::clone(&node_state.context);
    let addrman = Arc::new(Mutex::new(table));

    let running = Arc::new(AtomicBool::new(true));
    let running_addr = running.clone();
    let running_peer = running.clone();
    let running_block = running.clone();

    let peer_source = Arc::clone(&addrman);
    let kill = Arc::new(Mutex::new(None));
    let writer = Arc::clone(&kill);
    let stale_block_kill = Arc::clone(&kill);

    let peer_processing_handler = thread::spawn(move || {
        info!(target: Category::NODE, "Starting net processing thread.");
        while running_peer.load(Ordering::SeqCst) {
            let addr_lock = peer_source.lock().unwrap();
            let (address, port) = addr_lock.select().unwrap().network_addr();
            let peer = match address {
                AddrV2::Ipv4(ipv4) => BitcoinPeer::new(
                    SocketAddr::V4(SocketAddrV4::new(ipv4, port)),
                    network,
                    &mut node_state,
                ),
                AddrV2::Ipv6(ipv6) => {
                    let socket_adrr = (ipv6, port).into();
                    BitcoinPeer::new(socket_adrr, network, &mut node_state)
                }
                _ => continue,
            };
            let mut peer = match peer {
                Ok(connection) => {
                    let mut writer_lock = writer.lock().unwrap();
                    *writer_lock = Some(connection.writer());
                    connection
                }
                Err(e) => {
                    error!(target: Category::NET, "Could not connect: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            };
            loop {
                if let Err(e) = peer.receive_and_process_message(&mut node_state) {
                    match e {
                        p2p::net::Error::Io(io) => {
                            if io.kind() != std::io::ErrorKind::UnexpectedEof {
                                error!(target: Category::NET, "Unexpected I/O error: {}", io);
                            }
                        }
                        e => error!(target: Category::NET, "Error processing message: {e}"),
                    }
                    break;
                }
            }
        }
        info!(target: Category::NODE, "Stopping net processing thread.");
    });

    let addr_processing_handler = thread::spawn(move || {
        info!(target: Category::NODE, "Starting addr processing thread.");
        while running_addr.load(Ordering::SeqCst) {
            match addr_rx.recv() {
                Ok(payload) => {
                    let mut addr_lock = addrman.lock().unwrap();
                    for address in payload {
                        let record = addrman::Record::new(
                            address.addr,
                            address.port,
                            address.services,
                            &DNS_RESOLVER,
                        );
                        addr_lock.add(&record);
                    }
                }
                Err(_) => break,
            }
        }
        info!(target: Category::NODE, "Stopping addr processing thread.");
    });

    let block_processing_handler = thread::spawn(move || {
        info!(target: Category::NODE, "Starting block processing thread.");
        let mut last_block = Instant::now();
        while running_block.load(Ordering::SeqCst) {
            match block_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(block) => {
                    debug!(target: Category::KERNEL, "Validating block.");
                    last_block = Instant::now();
                    let _ = chainman.process_block(&block);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if last_block.elapsed() > STALE_BLOCK_DURATION {
                        last_block = Instant::now();
                        info!(target: Category::NET, "Potential stale block. Finding a new peer.");
                        let mut peer_lock = stale_block_kill.lock().unwrap();
                        if let Some(conn) = peer_lock.deref_mut() {
                            let _ = conn.shutdown();
                        }
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        info!(target: Category::NODE, "Stopping block processing thread.");
    });

    if let Ok(()) = shutdown_rx.recv() {
        context.interrupt().unwrap();
        let mut peer_lock = kill.lock().unwrap();
        if let Some(conn) = peer_lock.deref_mut() {
            conn.shutdown().unwrap()
        }
        info!(target: Category::NODE, "Received shutdown signal, shutting down...");
        running.store(false, Ordering::SeqCst);
    }

    addr_processing_handler.join().unwrap();
    peer_processing_handler.join().unwrap();
    block_processing_handler.join().unwrap();

    info!(target: Category::NODE, "Exiting.");
    Ok(())
}

fn main() {
    let (config, _) = Config::including_optional_config_files::<&[&str]>(&[]).unwrap_or_exit();
    START.call_once(|| {
        setup_logging();
    });
    if config.daemon {
        let daemonize = Daemonize::new(config.datadir.data_dir());
        info!(target: Category::NODE, "Kernel node starting...");
        daemonize.fork().unwrap();
    }

    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let ipc_shutdown = shutdown_tx.clone();

    let tip_state = Arc::new(Mutex::new(TipState::default()));

    let network = config.network.parse::<Network>().expect("invalid network");
    let wallet = Arc::new(Mutex::new(Wallet::new(network.wallet_network())));
    let chainman_holder: Arc<std::sync::OnceLock<Arc<ChainstateManager>>> =
        Arc::new(std::sync::OnceLock::new());
    let context = create_context(
        network.chain_type(),
        shutdown_tx.clone(),
        &tip_state,
        Arc::clone(&wallet),
        Arc::clone(&chainman_holder),
    );

    let data_dir = config.datadir.data_dir();
    let blocks_dir = data_dir.clone() + "/blocks";
    let chainman_builder = ChainstateManagerBuilder::new(&context, &data_dir, &blocks_dir)
        .unwrap()
        .worker_threads(
            ((available_parallelism().unwrap().get() / 2) + 1)
                .try_into()
                .unwrap(),
        );
    let chainman = Arc::new(chainman_builder.build().unwrap());
    chainman_holder
        .set(Arc::clone(&chainman))
        .ok()
        .expect("chainman holder already set");

    let (block_tx, block_rx) = mpsc::sync_channel(1);
    let (addr_tx, addr_rx) = mpsc::channel();

    let node_state = NodeState {
        addr_tx,
        block_tx,
        tip_state,
        chainman,
        context: Arc::clone(&context),
    };

    if let Err(err) = node_state.chainman.import_blocks() {
        error!(target: Category::KERNEL, "Error importing blocks: {}", err);
        return;
    }

    let tip_index = node_state.chainman.active_chain().tip();
    let hash = tip_index.block_hash();
    node_state.set_tip_state(BlockHash::from_byte_array(hash.to_bytes()));

    info!(target: Category::KERNEL, "Bitcoin kernel initialized");

    let connect = config
        .connect
        .map(|sock| sock.parse::<SocketAddr>().unwrap());

    if shutdown_rx.try_recv().is_ok() {
        info!(target: Category::NODE, "Shutting down!");
        return;
    }

    let wallet_for_ipc = Arc::clone(&wallet);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    std::thread::spawn(move || {
        rt.block_on(async move {
            tokio::task::LocalSet::new()
                .run_until(async move {
                    let sock_file = data_dir + "/node.sock";
                    let _ = std::fs::remove_file(&sock_file);
                    debug!(target: Category::IPC, "Listening for incoming IPC requests");
                    let unix_socket = UnixListener::bind(sock_file).unwrap();
                    loop {
                        let stream = tokio::select! {
                            unix_bind_res = unix_socket.accept() => {
                                unix_bind_res.unwrap().0
                            }
                            _ctrl_c = tokio::signal::ctrl_c() => {
                                info!(target: Category::NODE, "Received shutdown signal");
                                shutdown_tx.clone().send(()).unwrap();
                                return;
                            }
                        };
                        debug!(target: Category::IPC, "Handling inbound IPC call");
                        let state = Arc::clone(&wallet_for_ipc);
                        let (reader, writer) = stream.into_split();
                        let buf_reader = futures::io::BufReader::new(reader.compat());
                        let buf_writer = futures::io::BufWriter::new(writer.compat_write());
                        let network = capnp_rpc::twoparty::VatNetwork::new(
                            buf_reader,
                            buf_writer,
                            capnp_rpc::rpc_twoparty_capnp::Side::Server,
                            Default::default(),
                        );
                        let client: server::Client =
                            capnp_rpc::new_client(IpcInterface::new(ipc_shutdown.clone(), state));
                        let rpc_system =
                            capnp_rpc::RpcSystem::new(Box::new(network), Some(client.client));
                        tokio::task::spawn_local(rpc_system);
                    }
                })
                .await;
        })
    });

    run(network, connect, node_state, shutdown_rx, addr_rx, block_rx).unwrap()
}
