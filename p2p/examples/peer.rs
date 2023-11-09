use std::sync::Arc;

use clap::Parser;
use easy_parallel::Parallel;
use smol::{channel, future, Executor};

use karyons_net::{Endpoint, Port};

use karyons_p2p::{Backend, Config, PeerID};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Optional list of bootstrap peers to start the seeding process.
    #[arg(short)]
    bootstrap_peers: Vec<Endpoint>,

    /// Optional list of peer endpoints for manual connections.
    #[arg(short)]
    peer_endpoints: Vec<Endpoint>,

    /// Optional endpoint for accepting incoming connections.
    #[arg(short)]
    listen_endpoint: Option<Endpoint>,

    /// Optional TCP/UDP port for the discovery service.
    #[arg(short)]
    discovery_port: Option<Port>,

    /// Optional user id
    #[arg(long)]
    userid: Option<String>,
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    let peer_id = match cli.userid {
        Some(userid) => PeerID::new(userid.as_bytes()),
        None => PeerID::random(),
    };

    // Create the configuration for the backend.
    let config = Config {
        listen_endpoint: cli.listen_endpoint,
        peer_endpoints: cli.peer_endpoints,
        bootstrap_peers: cli.bootstrap_peers,
        discovery_port: cli.discovery_port.unwrap_or(0),
        ..Default::default()
    };

    // Create a new Backend
    let backend = Backend::new(peer_id, config);

    let (ctrlc_s, ctrlc_r) = channel::unbounded();
    let handle = move || ctrlc_s.try_send(()).unwrap();
    ctrlc::set_handler(handle).unwrap();

    let (signal, shutdown) = channel::unbounded::<()>();

    // Create a new Executor
    let ex = Arc::new(Executor::new());

    let task = async {
        // Run the backend
        backend.run(ex.clone()).await.unwrap();

        // Wait for ctrlc signal
        ctrlc_r.recv().await.unwrap();

        // Shutdown the backend
        backend.shutdown().await;

        drop(signal);
    };

    // Run four executor threads.
    Parallel::new()
        .each(0..4, |_| future::block_on(ex.run(shutdown.recv())))
        .finish(|| future::block_on(task));
}