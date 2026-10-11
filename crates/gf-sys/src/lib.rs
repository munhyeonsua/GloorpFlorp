//! Operating-system specific pieces, kept in one place so everything else is
//! portable and testable on Linux. On non-Windows systems these functions
//! return empty lists / errors / fakes, as documented on each item.

pub mod keys;

mod capture;
#[cfg(windows)]
mod win;

use std::io::Read;
use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A monitor that can be captured. `index` is the DXGI output index on the
/// primary adapter, which is exactly what FFmpeg's `ddagrab=output_idx=` means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub index: u32,
    /// Friendly text for the UI, e.g. "Monitor 1 (2560×1440, primary)".
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

impl Monitor {
    fn new(index: u32, width: u32, height: u32, primary: bool) -> Monitor {
        let tag = if primary { ", primary" } else { "" };
        Monitor {
            index,
            name: format!("Monitor {} ({width}×{height}{tag})", index + 1),
            width,
            height,
            primary,
        }
    }
}

/// Monitors attached to the primary GPU. Non-Windows: one fake 1920×1080 monitor.
pub fn list_monitors() -> Vec<Monitor> {
    #[cfg(windows)]
    {
        match win::list_monitors() {
            Ok(list) => list,
            Err(e) => {
                log::warn!("couldn't list monitors: {e}");
                Vec::new()
            }
        }
    }
    #[cfg(not(windows))]
    {
        vec![Monitor::new(0, 1920, 1080, true)]
    }
}

/// A running program the user can pick as the only source of game audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioApp {
    pub pid: u32,
    /// Executable name, e.g. "EscapeFromTarkov.exe".
    pub exe: String,
    /// Main window title.
    pub title: String,
}

/// Programs with a visible top-level window (excluding this app), sorted by
/// exe name. Non-Windows: empty.
pub fn list_audio_apps() -> Vec<AudioApp> {
    #[cfg(windows)]
    let mut apps = win::list_audio_apps();
    #[cfg(not(windows))]
    let mut apps: Vec<AudioApp> = Vec::new();
    apps.sort_by_cached_key(|a| (a.exe.to_lowercase(), a.pid));
    apps
}

/// What to capture as "game audio".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameAudioSource {
    /// Everything the PC plays except this app's own sound (so the friend's
    /// voice and game audio we play back never echo back to them).
    AllExceptThisApp,
    /// Only this process and its children.
    Process { pid: u32 },
}

/// Receives interleaved stereo 48 kHz samples on the capture thread.
pub type SampleCallback = Box<dyn FnMut(&[f32]) + Send>;

/// Captures game audio with WASAPI process loopback (Windows 10 2004+).
/// Delivers interleaved stereo f32 samples at 48 kHz, in whatever chunk sizes
/// Windows provides, on a capture thread. Stops when dropped.
///
/// Non-Windows: if the env var `GLOORP_FAKE_AUDIO=1` is set, delivers a quiet
/// 440 Hz test tone at real-time pace (for tests); otherwise returns an error.
/// The env var is honoured on Windows too, so end-to-end tests can run on
/// machines without audio devices.
pub struct LoopbackCapture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackCapture {
    pub fn start(
        source: GameAudioSource,
        on_samples: SampleCallback,
    ) -> anyhow::Result<LoopbackCapture> {
        if std::env::var("GLOORP_FAKE_AUDIO").is_ok_and(|v| v == "1") {
            log::info!("game audio: using the fake 440 Hz test tone");
            return capture::launch("gf-game-audio-fake", move |stop, ready| {
                capture::fake_tone(on_samples, stop, ready)
            });
        }
        #[cfg(windows)]
        {
            log::info!("game audio: starting process loopback for {source:?}");
            capture::launch("gf-game-audio", move |stop, ready| {
                win::loopback_capture(source, on_samples, stop, ready)
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (source, on_samples);
            anyhow::bail!("Game-audio capture only works on Windows.")
        }
    }

    /// Set if the capture thread died (e.g. the chosen program exited).
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                log::warn!("game-audio capture thread didn't stop within 2 s; leaving it behind");
            }
        }
    }
}

/// True for addresses in Tailscale's range, 100.64.0.0/10.
pub fn is_tailscale_ip(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xC0) == 64
}

/// Picks the Tailscale address from `(interface name, IPv4)` pairs, preferring
/// an interface whose name mentions Tailscale.
fn pick_tailscale_ip<'a>(addrs: impl IntoIterator<Item = (&'a str, Ipv4Addr)>) -> Option<Ipv4Addr> {
    let mut fallback = None;
    for (name, ip) in addrs {
        if !is_tailscale_ip(ip) {
            continue;
        }
        if name.to_lowercase().contains("tailscale") {
            return Some(ip);
        }
        fallback.get_or_insert(ip);
    }
    fallback
}

/// First line of `tailscale ip -4` output that is an IPv4 address.
fn parse_tailscale_cli(output: &str) -> Option<Ipv4Addr> {
    output.lines().find_map(|l| l.trim().parse().ok())
}

/// This machine's Tailscale IPv4 address (100.64.0.0/10), found by scanning
/// network interfaces, falling back to running `tailscale ip -4`.
pub fn tailscale_ipv4() -> Option<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            let found = pick_tailscale_ip(ifaces.iter().filter_map(|i| match i.ip() {
                std::net::IpAddr::V4(ip) => Some((i.name.as_str(), ip)),
                std::net::IpAddr::V6(_) => None,
            }));
            if let Some(ip) = found {
                log::debug!("Tailscale address {ip} found on a network interface");
                return Some(ip);
            }
        }
        Err(e) => log::warn!("couldn't list network interfaces: {e}"),
    }
    let mut programs = vec!["tailscale"];
    if cfg!(windows) {
        programs.push(r"C:\Program Files\Tailscale\tailscale.exe");
    }
    for program in programs {
        let mut cmd = Command::new(program);
        cmd.args(["ip", "-4"]);
        if let Some(ip) = run_with_timeout(cmd, Duration::from_secs(3))
            .as_deref()
            .and_then(parse_tailscale_cli)
        {
            log::debug!("Tailscale address {ip} from `{program} ip -4`");
            return Some(ip);
        }
    }
    log::debug!("no Tailscale address found");
    None
}

/// Computer name, used as the default display name.
pub fn computer_name() -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    run_with_timeout(Command::new("hostname"), Duration::from_secs(3))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "My PC".to_string())
}

/// Runs a short command with a hidden console and returns its stdout if it
/// exits successfully within `timeout`; the process is killed otherwise.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Option<String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    hide_console_window(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(64 * 1024).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let out = rx.recv_timeout(timeout).ok();
    let status = match out {
        Some(_) => {
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    _ => break None,
                }
            }
        }
        None => None,
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    if !status?.success() {
        return None;
    }
    String::from_utf8(out?).ok()
}

/// Ensures child processes (ffmpeg, mpv) die when this app exits or crashes.
/// Windows: assigns the child to a job object with KILL_ON_JOB_CLOSE
/// (created once, held for the life of the process). Elsewhere: no-op.
pub fn bind_child_to_app_lifetime(child: &std::process::Child) {
    #[cfg(windows)]
    win::bind_child_to_app_lifetime(child);
    #[cfg(not(windows))]
    let _ = child;
}

/// Windows: sets CREATE_NO_WINDOW so console programs (ffmpeg) don't flash a
/// black window. Elsewhere: no-op.
pub fn hide_console_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_range() {
        assert!(is_tailscale_ip(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_tailscale_ip(Ipv4Addr::new(100, 101, 102, 103)));
        assert!(is_tailscale_ip(Ipv4Addr::new(100, 127, 255, 255)));
        assert!(!is_tailscale_ip(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(!is_tailscale_ip(Ipv4Addr::new(100, 128, 0, 0)));
        assert!(!is_tailscale_ip(Ipv4Addr::new(192, 168, 1, 10)));
        assert!(!is_tailscale_ip(Ipv4Addr::new(10, 64, 0, 1)));
    }

    #[test]
    fn picks_tailscale_interface_first() {
        let a = Ipv4Addr::new(100, 70, 1, 1);
        let b = Ipv4Addr::new(100, 80, 2, 2);
        let lan = Ipv4Addr::new(192, 168, 0, 5);
        assert_eq!(
            pick_tailscale_ip([("eth0", lan), ("wg0", a), ("Tailscale", b)]),
            Some(b)
        );
        assert_eq!(pick_tailscale_ip([("eth0", lan), ("wg0", a)]), Some(a));
        assert_eq!(pick_tailscale_ip([("eth0", lan)]), None);
        assert_eq!(pick_tailscale_ip([("tailscale0", lan)]), None);
    }

    #[test]
    fn parses_cli_output() {
        assert_eq!(
            parse_tailscale_cli("100.101.102.103\n"),
            Some(Ipv4Addr::new(100, 101, 102, 103))
        );
        assert_eq!(
            parse_tailscale_cli("warning: something\r\n 100.64.0.9 \r\n"),
            Some(Ipv4Addr::new(100, 64, 0, 9))
        );
        assert_eq!(parse_tailscale_cli(""), None);
        assert_eq!(parse_tailscale_cli("Tailscale is stopped."), None);
    }

    #[test]
    fn monitor_names() {
        assert_eq!(
            Monitor::new(0, 2560, 1440, true).name,
            "Monitor 1 (2560×1440, primary)"
        );
        assert_eq!(
            Monitor::new(1, 1920, 1080, false).name,
            "Monitor 2 (1920×1080)"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn fake_monitor_list() {
        let m = list_monitors();
        assert_eq!(m, vec![Monitor::new(0, 1920, 1080, true)]);
        assert!(list_audio_apps().is_empty());
    }

    #[test]
    fn computer_name_not_empty() {
        assert!(!computer_name().trim().is_empty());
    }

    #[test]
    fn tailscale_lookup_does_not_hang() {
        let t = Instant::now();
        let ip = tailscale_ipv4();
        assert!(ip.is_none_or(is_tailscale_ip));
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn timeout_kills_slow_command() {
        let cmd = if cfg!(windows) {
            let mut c = Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        let t = Instant::now();
        assert_eq!(run_with_timeout(cmd, Duration::from_millis(300)), None);
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn missing_command_is_none() {
        let cmd = Command::new("gloorpflorp-definitely-not-a-program");
        assert_eq!(run_with_timeout(cmd, Duration::from_secs(1)), None);
    }
}
