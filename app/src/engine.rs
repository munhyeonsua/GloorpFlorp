//! The engine: owns the network session, encoder, viewer and audio, and
//! reacts to their events. Runs on its own thread; the UI talks to it only
//! through `Command`s and reads a `Snapshot`. Never blocks the UI.
//!
//! Slow work (finding the Tailscale address, probing encoders, starting
//! FFmpeg and mpv, listing devices, stopping child processes) runs on
//! short-lived helper threads that report back to the engine thread.
//! Starting audio and starting/stopping game audio run in order on one
//! audio thread.

mod inner;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use gf_audio::AudioStatus;
use gf_net::NetStats;
use gf_proto::VideoCodec;
use gf_sys::{AudioApp, Monitor};

use crate::settings::Settings;

/// What the UI asks the engine to do.
#[derive(Clone, Debug)]
pub enum Command {
    /// Open a room on this PC's Tailscale address. Clicking it again while
    /// waiting for the friend opens a new room with a new code.
    Host,
    /// Join the room with this code (as typed or pasted).
    Join(String),
    /// Join a host directly, without a room code (tests).
    JoinAddr {
        addr: SocketAddr,
        secret: u32,
    },
    /// Leave the room (or stop hosting / stop trying to join).
    Leave,
    StartSharing,
    StopSharing,
    /// Reopen the viewer window for the friend's screen.
    OpenViewer,
    /// Start sending game audio from the source in the settings.
    StartGameAudio,
    StopGameAudio,
    /// The settings changed (the engine saves them a moment later). Mute
    /// and deafen in them are ignored: those change only with `SetMuted`,
    /// `SetDeafened` and the shortcut keys, so an older copy can't undo a
    /// shortcut.
    ApplySettings(Box<Settings>),
    SetMuted(bool),
    SetDeafened(bool),
    /// List monitors, audio devices and running programs again.
    RefreshLists,
    DismissError,
    /// Leave, stop everything, save the settings and end the engine thread.
    Shutdown,
}

/// Where we are with the friend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    /// Hosting a room. `waiting` becomes true once the room is open (before
    /// that, while the Tailscale address is looked up, `code` is empty).
    Hosting {
        code: String,
        waiting: bool,
    },
    /// Connecting to the friend's room.
    Joining,
    Connected {
        peer: String,
    },
    /// The connection dropped; trying to get it back (joiner only).
    Reconnecting,
}

/// A screen being shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareInfo {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// My own screen sharing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShareState {
    #[default]
    Off,
    /// Waiting for the video encoder check to finish, then starts.
    Waiting,
    /// FFmpeg is starting.
    Starting,
    Live(ShareInfo),
}

/// Everything the UI shows, refreshed by the engine (about 10 times a
/// second, and right after anything important happens).
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub phase: Phase,
    /// The room code while hosting.
    pub room_code: Option<String>,
    /// Where the host listens (real port, also when the port was 0).
    pub listen_addr: Option<SocketAddr>,
    pub tailscale_ip: Option<Ipv4Addr>,
    pub peer_name: Option<String>,
    /// Codecs this PC can share with, best first (empty until probed).
    pub my_encoders: Vec<VideoCodec>,
    pub probing_encoders: bool,
    /// False if FFmpeg wasn't found (no sharing or viewing then).
    pub video_available: bool,
    pub peer_encoders: Vec<VideoCodec>,
    pub my_share: ShareState,
    pub frames_sent: u64,
    pub send_fps: f32,
    /// The friend has the viewer window open for my screen.
    pub peer_watching: bool,
    pub peer_share: Option<ShareInfo>,
    pub frames_received: u64,
    pub recv_fps: f32,
    pub viewer_open: bool,
    /// Game audio is on (or starting; see `game_audio_starting`).
    pub my_game_audio: bool,
    /// Windows is still starting the game-audio capture; the friend is told
    /// once it runs.
    pub game_audio_starting: bool,
    pub peer_game_audio: bool,
    pub net: NetStats,
    /// False while audio is starting (a few seconds after launch), and if it
    /// couldn't start at all (see `error`).
    pub audio_running: bool,
    pub audio: AudioStatus,
    /// Mute and deafen as the engine has them. The checkboxes show these:
    /// the shortcut keys change them without the window.
    pub mic_muted: bool,
    pub deafened: bool,
    /// The game-audio capture's problem, while game audio is still on.
    pub game_audio_note: Option<String>,
    /// Push-to-talk is on and a program running as administrator has (or
    /// had, in the last few seconds) focus: Windows hides the key from us.
    pub ptt_blocked: bool,
    pub monitors: Vec<Monitor>,
    pub input_devices: Vec<String>,
    pub output_devices: Vec<String>,
    pub audio_apps: Vec<AudioApp>,
    /// Plain-language problem for the red banner, until dismissed.
    pub error: Option<String>,
    /// Plain-language note (e.g. "Your friend left the room.").
    pub info: Option<String>,
}

/// Shown when no video encoder works on this PC.
pub const NO_ENCODER: &str = "Your graphics card's video encoder didn't work, so screen sharing can't start. Update your NVIDIA driver, restart the PC, then start GloorpFlorp again.";

/// Called from the engine thread whenever the snapshot changed.
pub type ChangeCallback = Arc<dyn Fn() + Send + Sync>;

/// Overrides, mostly for tests.
#[derive(Clone, Default)]
pub struct EngineOptions {
    /// Host on this address instead of the Tailscale address (tests use
    /// 127.0.0.1). The room code is then only good for its secret.
    pub bind_ip: Option<Ipv4Addr>,
    /// `Join` connects here instead of the address in the room code (two
    /// copies of the app on one PC without Tailscale, for development).
    pub join_ip: Option<Ipv4Addr>,
    /// Use this port instead of `Settings::port` when hosting and joining
    /// with a code (0 lets the OS pick one when hosting; see
    /// `Snapshot::listen_addr`).
    pub port: Option<u16>,
    /// Where to save the settings; `None` never saves.
    pub settings_path: Option<PathBuf>,
    /// Lets the UI repaint when something changed.
    pub on_change: Option<ChangeCallback>,
}

/// Handle to the engine thread. Dropping it shuts the engine down (waiting
/// up to 3 s).
pub struct Engine {
    tx: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    /// Disconnects when the engine thread ends (normally or by panic).
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Engine {
    /// Starts the engine thread. It finds FFmpeg, checks the video
    /// encoders, lists devices and starts audio in the background.
    pub fn start(settings: Settings, opts: EngineOptions) -> anyhow::Result<Engine> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (done_tx, done) = crossbeam_channel::bounded::<()>(0);
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let shared = snapshot.clone();
        let thread = std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || {
                let _done = done_tx;
                inner::run(settings, opts, rx, shared);
            })
            .map_err(|e| {
                anyhow::anyhow!("Couldn't start GloorpFlorp's engine ({e}). Restart GloorpFlorp.")
            })?;
        Ok(Engine {
            tx,
            snapshot,
            done,
            thread: Some(thread),
        })
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    /// A copy of the current state.
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.snapshot).clone()
    }

    /// False once the engine thread has ended (after `Shutdown`, or if it crashed).
    pub fn is_running(&self) -> bool {
        matches!(self.done.try_recv(), Err(TryRecvError::Empty))
    }

    /// Asks the engine to stop and waits up to `timeout`. Returns false if
    /// it didn't finish in time (it then finishes on its own, or with the
    /// process).
    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let _ = self.tx.send(Command::Shutdown);
        match self.done.recv_timeout(timeout) {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                log::warn!("the engine didn't stop within {timeout:?}; not waiting any longer");
                false
            }
            _ => {
                let _ = thread.join();
                true
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown(Duration::from_secs(3));
    }
}

/// The first paragraph of a long error (FFmpeg errors carry details after a
/// blank line, meant for the log).
pub fn first_paragraph(text: &str) -> String {
    let text = text.trim();
    let end = text
        .find("\n\n")
        .or_else(|| text.find("\r\n\r\n"))
        .unwrap_or(text.len());
    text[..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_paragraph_only() {
        assert_eq!(
            first_paragraph("Update your driver.\n\nDetails from FFmpeg:\nfoo"),
            "Update your driver."
        );
        assert_eq!(first_paragraph("One line"), "One line");
        assert_eq!(first_paragraph("A\r\n\r\nB"), "A");
        assert_eq!(first_paragraph("  \n\n"), "");
    }
}
