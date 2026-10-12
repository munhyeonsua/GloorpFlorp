// No console window in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ui;

use std::sync::Arc;

use eframe::egui_wgpu::WgpuSetup;
use eframe::{egui, wgpu};
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
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GloorpFlorp")
            .with_app_id("GloorpFlorp")
            .with_inner_size([460.0, 760.0])
            .with_min_inner_size([440.0, 320.0]),
        ..Default::default()
    };
    // wgpu's default memory blocks reserve ~245 MB of video memory for this
    // small window (~65 MB with MemoryUsage), which a game on an 8 GB card
    // misses.
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        let base = setup.device_descriptor.clone();
        setup.device_descriptor = Arc::new(move |adapter| wgpu::DeviceDescriptor {
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..base(adapter)
        });
    }
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
        let e = e.to_string();
        show_error(&format!(
            "GloorpFlorp couldn't open its window: {}. Update your graphics driver and try again. \
             Details are in %APPDATA%\\GloorpFlorp\\log.txt.",
            e.trim().trim_end_matches('.')
        ));
    }
    log::info!("GloorpFlorp closed");
    result.map_err(|e| anyhow::anyhow!("GloorpFlorp's window couldn't be shown: {e}"))
}

/// A message box: without a window (and, in release builds, without a
/// console) there is nowhere else to say what went wrong.
#[cfg(windows)]
fn show_error(text: &str) {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let text = HSTRING::from(text);
    let title = HSTRING::from("GloorpFlorp");
    // SAFETY: both are NUL-terminated wide strings that outlive the call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_error(text: &str) {
    eprintln!("{text}");
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
