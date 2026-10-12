// No console window in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ui;

use eframe::egui;
use gloorpflorp::settings::{self, Settings};

fn main() {
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
            .with_min_inner_size([440.0, 320.0]),
        ..Default::default()
    };
    let dev = dev_overrides();
    // winit re-raises a UI-thread panic after its loop ends; catch it so a
    // crash still reaches the close-and-exit below (the panic hook logged it).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eframe::run_native(
            "GloorpFlorp",
            options,
            Box::new(move |cc| {
                Ok(Box::new(ui::GloorpApp::new(
                    cc, settings, notice, path, dev,
                )?))
            }),
        )
    }));
    let ok = matches!(result, Ok(Ok(())));
    if let Ok(Err(e)) = &result {
        log::error!("the window couldn't be shown: {e}");
        let e = e.to_string();
        show_error(&format!(
            "GloorpFlorp couldn't open its window: {}. Update your graphics driver and try again. \
             Details are in %APPDATA%\\GloorpFlorp\\log.txt.",
            e.trim().trim_end_matches('.')
        ));
    }
    // Children first, then end at once: a normal exit stops our threads
    // mid-call and then runs shutdown code in the loaded libraries (graphics
    // driver, audio), which can hang and keep this process (and so any mpv
    // or FFmpeg) alive. The engine already saved the settings, and the log
    // file isn't buffered.
    gf_sys::close_children();
    log::info!("GloorpFlorp closed");
    gf_sys::exit_now(if ok { 0 } else { 1 })
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
