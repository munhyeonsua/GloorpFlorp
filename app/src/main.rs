// No console window in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ui;

use eframe::egui;
use gloorpflorp::settings::{self, Settings};

fn main() -> anyhow::Result<()> {
    gloorpflorp::logging::init(settings::app_dir().as_deref());
    let path = settings::settings_path();
    let (settings, notice) = match &path {
        Some(p) => Settings::load(p),
        None => {
            log::warn!("no user folder found; settings won't be saved");
            (Settings::default(), None)
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GloorpFlorp")
            .with_app_id("GloorpFlorp")
            .with_inner_size([460.0, 760.0])
            .with_min_inner_size([360.0, 320.0]),
        ..Default::default()
    };
    let dev = dev_overrides();
    let result = eframe::run_native(
        "GloorpFlorp",
        options,
        Box::new(move |cc| {
            Ok(Box::new(ui::GloorpApp::new(
                cc, settings, notice, path, dev,
            )?))
        }),
    );
    if let Err(e) = &result {
        log::error!("the window couldn't be shown: {e}");
    }
    log::info!("GloorpFlorp closed");
    result.map_err(|e| anyhow::anyhow!("GloorpFlorp's window couldn't be shown: {e}"))
}

/// Development only: `GLOORP_DEV_IP=127.0.0.1` hosts and joins on that
/// address instead of Tailscale, `GLOORP_DEV_PORT` replaces the port, so two
/// copies can talk on one PC.
fn dev_overrides() -> gloorpflorp::EngineOptions {
    let ip = std::env::var("GLOORP_DEV_IP")
        .ok()
        .and_then(|v| v.parse().ok());
    let port = std::env::var("GLOORP_DEV_PORT")
        .ok()
        .and_then(|v| v.parse().ok());
    if ip.is_some() || port.is_some() {
        log::warn!("development overrides active: address {ip:?}, port {port:?}");
    }
    gloorpflorp::EngineOptions {
        bind_ip: ip,
        join_ip: ip,
        port,
        ..Default::default()
    }
}
