//! Client for the phase 1 LAN stream.
//!
//! Connects to `rd-host` over UDP, reassembles H.264 access units, decodes
//! them, and presents the latest picture with vsync disabled.

mod app;
mod net;

use std::sync::Arc;
use std::thread;

use anyhow::Result;
use clap::Parser;
use net::{Shared, UserEvent};
use winit::event_loop::EventLoop;

#[derive(Debug, Clone)]
pub struct Presented {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub t_recv: std::time::Instant,
    pub frame_id: u32,
    pub host_capture_to_send_us: u64,
}

#[derive(Debug, Parser)]
#[command(name = "rd-client", version, about = "Watch a lowlat-desk host")]
struct Args {
    /// Host address, for example 192.168.1.20:47000.
    #[arg(long)]
    connect: std::net::SocketAddr,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if let Err(err) = run(Args::parse()) {
        eprintln!("rd-client: {err:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let shared = Arc::new(Shared::new());
    let net_shared = Arc::clone(&shared);
    thread::spawn(move || {
        if let Err(err) = net::run(args.connect, net_shared, proxy) {
            tracing::error!(error = %err, "network thread stopped");
        }
    });
    let mut app = app::App::new(shared);
    event_loop.run_app(&mut app)?;
    Ok(())
}
