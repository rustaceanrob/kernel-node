use clap::Parser;
use kernel_node::echo_capnp::echo;
use tokio::net::UnixStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    commands: Commands,
}

#[derive(Debug, Clone, clap::Subcommand)]
enum Commands {
    /// Echo a message back to you.
    Echo(Echo),
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
    rt.block_on(tokio::task::LocalSet::new().run_until(async move {
        let stream = UnixStream::connect("./node.sock")
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
        let client: echo::Client =
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
        }
    }))
}
