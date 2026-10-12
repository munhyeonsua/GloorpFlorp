//! The lobby server. Settings come from environment variables (see the
//! README's "Lobby server" section):
//!
//! - `LOBBY_LISTEN`: address and port to listen on (default `0.0.0.0:47900`)
//! - `LOBBY_DATA`: folder for the member list and chat (default `lobby-data`)
//! - `LOBBY_TAILSCALE_SOCKET`: tailscaled's socket, to show who is online
//!   (default `/var/run/tailscale/tailscaled.sock`)

use std::net::TcpListener;
use std::path::PathBuf;

use simplelog::{Config, LevelFilter, SimpleLogger};

fn main() -> std::io::Result<()> {
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());
    let var = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
    let listen = var("LOBBY_LISTEN", "0.0.0.0:47900");
    let cfg = gf_lobby::server::Config {
        data_dir: PathBuf::from(var("LOBBY_DATA", "lobby-data")),
        tailscale_socket: Some(PathBuf::from(var(
            "LOBBY_TAILSCALE_SOCKET",
            "/var/run/tailscale/tailscaled.sock",
        ))),
    };
    let listener =
        TcpListener::bind(&listen).inspect_err(|e| log::error!("can't listen on {listen}: {e}"))?;
    gf_lobby::server::serve(listener, cfg).inspect_err(|e| log::error!("{e}"))
}
