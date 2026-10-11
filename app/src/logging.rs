//! Logging to `<app dir>/log.txt` (the previous run's log is kept as
//! `log.old.txt`), plus the terminal in debug builds, and a panic hook that
//! writes panics to the log.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use simplelog::{
    ColorChoice, CombinedLogger, ConfigBuilder, LevelFilter, SharedLogger, TermLogger,
    TerminalMode, WriteLogger,
};

/// Module prefixes of GloorpFlorp's own crates (logged in detail).
const OURS: [&str; 3] = ["gf_", "gloorpflorp", "GloorpFlorp"];

fn config(ours: bool) -> simplelog::Config {
    let mut b = ConfigBuilder::new();
    b.set_time_format_rfc3339()
        .set_thread_level(LevelFilter::Off);
    for prefix in OURS {
        if ours {
            b.add_filter_allow_str(prefix);
        } else {
            b.add_filter_ignore_str(prefix);
        }
    }
    // Fails on some systems (e.g. multi-threaded Unix); UTC is fine then.
    let _ = b.set_time_offset_to_local();
    b.build()
}

fn open_log(dir: &Path) -> std::io::Result<(File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("log.txt");
    if path.exists() {
        let old = dir.join("log.old.txt");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&path, &old);
    }
    // Append mode: the two loggers below share this file through cloned handles.
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    Ok((file, path))
}

/// Starts logging. Our crates log from `Debug` up, everything else (window,
/// graphics libraries) only warnings and errors. Never fails: without a log
/// file the app still runs. Call once, early in `main`.
pub fn init(dir: Option<&Path>) {
    let mut loggers: Vec<Box<dyn SharedLogger>> = Vec::new();
    let mut file_error = None;
    let mut log_path = None;
    match dir.map(open_log) {
        Some(Ok((file, path))) => {
            if let Ok(other) = file.try_clone() {
                loggers.push(WriteLogger::new(LevelFilter::Warn, config(false), other));
            }
            loggers.push(WriteLogger::new(LevelFilter::Debug, config(true), file));
            log_path = Some(path);
        }
        Some(Err(e)) => file_error = Some(e.to_string()),
        None => file_error = Some("no user folder found".into()),
    }
    if cfg!(debug_assertions) {
        loggers.push(TermLogger::new(
            LevelFilter::Debug,
            config(true),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ));
    }
    let _ = CombinedLogger::init(loggers);
    install_panic_hook();
    log::info!(
        "GloorpFlorp {} starting ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    match (log_path, file_error) {
        (Some(p), _) => log::info!("logging to {}", p.display()),
        (None, Some(e)) => log::warn!("couldn't open the log file: {e}"),
        (None, None) => {}
    }
}

/// Logs every panic (with its thread and location), then runs the default hook.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!("panic in thread '{name}': {info}\n{backtrace}");
        default(info);
    }));
}
