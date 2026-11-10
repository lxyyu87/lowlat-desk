use std::net::SocketAddr;

use anyhow::Result;
use clap::Parser;
use rd_signal::SignalServer;

#[derive(Debug, Parser)]
#[command(
    name = "rd-signal",
    version,
    about = "Rendezvous and UDP relay for lowlat-desk"
)]
struct Args {
    /// TCP and UDP listen address. Open both protocols on this port.
    #[arg(long, default_value = "0.0.0.0:47010")]
    listen: SocketAddr,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if let Err(err) = run(Args::parse()) {
        eprintln!("rd-signal: {err:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    let server = SignalServer::spawn(args.listen)?;
    tracing::info!(addr = %server.local_addr(), "signal server listening");
    // spawn returns after the background thread starts. Park the process.
    loop {
        std::thread::park();
    }
}
