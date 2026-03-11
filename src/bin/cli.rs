use clap::Parser;
use kernel_node::kernel_util::DirnameExt;
use kernel_node::server_capnp::server;
use tokio::net::UnixStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const DEFAULT_DATA_DIR: &str = "~/.kernel-node/";

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(flatten)]
    opts: Opts,
    #[command(subcommand)]
    commands: Commands,
}

#[derive(Debug, Clone, clap::Args)]
struct Opts {
    /// Path to the data directory.
    #[arg(long, short)]
    datadir: Option<String>,
}

#[derive(Debug, Clone, clap::Subcommand)]
enum Commands {
    /// Echo a message to yourself.
    Echo(Echo),
    /// Terminate the server.
    Stop,
}

#[derive(Debug, Clone, clap::Args)]
struct Echo {
    /// The message to echo.
    message: String,
}

fn main() {
    let cli = Args::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let datadir_path = cli.opts.datadir.unwrap_or(DEFAULT_DATA_DIR.data_dir());
    let sock_file = datadir_path + "/node.sock";
    rt.block_on(tokio::task::LocalSet::new().run_until(async move {
        let stream = UnixStream::connect(sock_file)
            .await
            .expect("Could not connect to unix socket. Is `node` running?");
        let (reader, writer) = stream.into_split();
        let buf_reader = futures::io::BufReader::new(reader.compat());
        let buf_writer = futures::io::BufWriter::new(writer.compat_write());
        let network = capnp_rpc::twoparty::VatNetwork::new(
            buf_reader,
            buf_writer,
            capnp_rpc::rpc_twoparty_capnp::Side::Client,
            Default::default(),
        );
        let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
        let client: server::Client =
            rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
        tokio::task::spawn_local(rpc_system);
        match cli.commands {
            Commands::Echo(echo) => {
                let mut echo_req = client.echo_request();
                println!("Sending... {}", echo.message);
                echo_req.get().set_msg(echo.message);
                let result = echo_req.send().promise.await.unwrap();
                let result = result
                    .get()
                    .unwrap()
                    .get_reply()
                    .unwrap()
                    .to_string()
                    .unwrap();
                println!("{result}");
            }
            Commands::Stop => {
                let shutdown_req = client.shutdown_request();
                shutdown_req.send().promise.await.unwrap();
                println!("Kernel node stopping...");
            }
        }
    }))
}
