//! Client: receive, decode, and present a host screen, and send input back.
//!
//! `--connect` reaches a host directly on the LAN. `--signal` plus a device id
//! and password reaches a host through `rd-signal`.

mod app;
mod keys;
mod net;

use std::sync::Arc;
use std::thread;

use anyhow::{bail, Result};
use clap::Parser;
use net::{Remote, Shared, UserEvent};
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
    /// Host address for a direct LAN connection, for example 192.168.1.20:47000.
    #[arg(long)]
    connect: Option<std::net::SocketAddr>,
    /// Signal server, for example `203.0.113.10:47010`.
    #[arg(long)]
    signal: Option<std::net::SocketAddr>,
    /// Device id the host registered.
    #[arg(long)]
    id: Option<String>,
    /// Same password the host registered with.
    #[arg(long)]
    password: Option<String>,
    /// Skip UDP hole punching and relay through the signal server.
    #[arg(long)]
    force_relay: bool,
    /// STUN servers. Empty uses the built-in list. `none` disables STUN.
    #[arg(long, value_delimiter = ',')]
    stun: Vec<String>,
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
    if args.connect.is_some() && args.signal.is_some() {
        bail!("pass either --connect or --signal, not both");
    }
    if let Some(connect) = args.connect {
        thread::spawn(move || {
            if let Err(err) = net::run(connect, net_shared, proxy) {
                tracing::error!(error = %err, "network thread stopped");
            }
        });
    } else if let Some(signal) = args.signal {
        let id = args.id.unwrap_or_default();
        let password = args.password.clone().unwrap_or_default();
        if id.is_empty() || password.is_empty() {
            bail!("--signal requires --id and --password");
        }
        let remote = Remote {
            signal,
            id,
            password,
            force_relay: args.force_relay,
            stun: args.stun,
        };
        thread::spawn(move || {
            if let Err(err) = net::run_remote(remote, net_shared, proxy) {
                tracing::error!(error = %err, "network thread stopped");
            }
        });
    } else {
        bail!("pass --connect HOST:47000, or --signal HOST:47010 --id ID --password PASS");
    }
    let mut app = app::App::new(shared);
    event_loop.run_app(&mut app)?;
    Ok(())
}
