//! Operating-system specific pieces, kept in one place so everything else is
//! portable and testable on Linux. On non-Windows systems these functions
//! return empty lists / errors / fakes, as documented on each item.

pub mod keys;

use std::net::Ipv4Addr;

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

/// Monitors attached to the primary GPU. Non-Windows: one fake 1920×1080 monitor.
pub fn list_monitors() -> Vec<Monitor> {
    todo!()
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
    todo!()
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

/// Captures game audio with WASAPI process loopback (Windows 10 2004+).
/// Delivers interleaved stereo f32 samples at 48 kHz, in whatever chunk sizes
/// Windows provides, on a capture thread. Stops when dropped.
///
/// Non-Windows: if the env var `GLOORP_FAKE_AUDIO=1` is set, delivers a quiet
/// 440 Hz test tone at real-time pace (for tests); otherwise returns an error.
pub struct LoopbackCapture {}

impl LoopbackCapture {
    pub fn start(
        source: GameAudioSource,
        on_samples: Box<dyn FnMut(&[f32]) + Send>,
    ) -> anyhow::Result<LoopbackCapture> {
        todo!()
    }

    /// Set if the capture thread died (e.g. the chosen program exited).
    pub fn error(&self) -> Option<String> {
        todo!()
    }
}

/// This machine's Tailscale IPv4 address (100.64.0.0/10), found by scanning
/// network interfaces, falling back to running `tailscale ip -4`.
pub fn tailscale_ipv4() -> Option<Ipv4Addr> {
    todo!()
}

/// Computer name, used as the default display name.
pub fn computer_name() -> String {
    todo!()
}

/// Ensures child processes (ffmpeg, mpv) die when this app exits or crashes.
/// Windows: assigns the child to a job object with KILL_ON_JOB_CLOSE
/// (created once, held for the life of the process). Elsewhere: no-op.
pub fn bind_child_to_app_lifetime(child: &std::process::Child) {
    todo!()
}

/// Windows: sets CREATE_NO_WINDOW so console programs (ffmpeg) don't flash a
/// black window. Elsewhere: no-op.
pub fn hide_console_window(cmd: &mut std::process::Command) {
    todo!()
}
