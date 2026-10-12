//! The engine thread's state machine (see `docs/ARCHITECTURE.md`, "Session
//! lifecycle").

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{never, select, tick, Receiver, Sender};
use gf_audio::{AudioEngine, AudioSettings, PacketSink};
use gf_net::{Net, NetConfig, NetEvent, NetTx, Role};
use gf_proto::room::{is_tailscale_ipv4, RoomCode};
use gf_proto::wire::TcpMsg;
use gf_proto::{VideoCodec, VideoFrame};
use gf_sys::{AudioApp, GameAudioSource, Monitor, WindowState};
use gf_video::{EncoderEvent, EncoderSettings, Tools, Viewer};

use super::{
    first_paragraph, lock, Command, EngineOptions, Phase, ShareInfo, ShareState, Snapshot,
    NO_ENCODER,
};
use crate::settings::{GameAudioMode, Settings, VoiceModeChoice};

const TICK: Duration = Duration::from_millis(100);
/// Settings are written this long after the last change.
const SAVE_DELAY: Duration = Duration::from_secs(1);
/// Video settings changed while sharing: restart the encoder after this
/// pause (so dragging a slider doesn't restart it over and over).
const RESTART_DELAY: Duration = Duration::from_millis(1500);
/// After closing a session the old port may stay busy for a few seconds.
const REBIND_WINDOW: Duration = Duration::from_secs(5);
/// A viewer that closes this soon after opening probably failed to start.
const VIEWER_EARLY_CLOSE: Duration = Duration::from_secs(2);
/// Game audio is considered dead after the capture reports a problem this long.
const GAME_DEAD_AFTER: Duration = Duration::from_secs(1);
/// Frames kept for a viewer that is still starting (as many as the viewer
/// itself keeps while mpv starts).
const PENDING_FRAMES: usize = 300;
const PENDING_BYTES: usize = 16 << 20;
/// My share sent no frame for this long: the picture is still (a shared
/// window only sends frames when it changes), so a new viewer gets the
/// latest picture instead of waiting for a keyframe that won't come.
const STILL_SHARE: Duration = Duration::from_secs(1);
/// How often to check whether an elevated program has focus (a few cheap
/// system calls; only with push-to-talk).
const ELEVATED_CHECK: Duration = Duration::from_millis(500);
/// How long the "runs as administrator" hint stays after that program
/// lost focus.
const ELEVATED_HINT_HOLD: Duration = Duration::from_secs(10);

const TAILSCALE_MISSING: &str = "Tailscale isn't running or isn't signed in on this PC. Open Tailscale, sign in, then click Host again.";
const NO_AUDIO: &str = "Sound isn't working in GloorpFlorp right now, so game audio can't be sent. Restart GloorpFlorp, then try again.";
const GAME_AUDIO_DIED: &str = "Game audio stopped by itself (the program you picked may have closed). Choose what to send under \"Game audio\", then click Start sending game audio again.";

type EncoderStart = Result<(gf_video::Encoder, Receiver<EncoderEvent>), String>;

struct HostStarted {
    net: Net,
    code: String,
    ip: Ipv4Addr,
}

/// Results from helper threads.
enum Internal {
    Probed(Vec<VideoCodec>),
    Lists {
        monitors: Vec<Monitor>,
        inputs: Vec<String>,
        outputs: Vec<String>,
        apps: Vec<AudioApp>,
    },
    TailscaleIp(Option<Ipv4Addr>),
    HostReady {
        gen: u64,
        result: Result<HostStarted, String>,
    },
    EncoderReady {
        gen: u64,
        result: EncoderStart,
    },
    ViewerReady {
        gen: u64,
        result: Result<Viewer, String>,
    },
    /// The audio engine started (or couldn't).
    AudioReady(Result<Arc<AudioEngine>, String>),
    GameAudioReady {
        gen: u64,
        result: Result<(), String>,
        /// The running programs, when the audio thread looked them up.
        apps: Option<Vec<AudioApp>>,
    },
}

/// What to send as game audio, as chosen in the settings.
#[derive(Debug)]
enum GameSource {
    AllExceptThisApp,
    /// A program by exe name, looked up on the audio thread (listing
    /// windows can wait on unresponsive programs).
    Program(String),
}

/// Work for the audio thread: starting audio and starting or stopping game
/// audio can each block for seconds, so they never run on the engine thread.
/// Jobs run one at a time, in order.
enum AudioJob {
    StartGame { gen: u64, source: GameSource },
    StopGame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GameAudio {
    Off,
    /// Asked the audio thread to start (or switch) the capture.
    Starting,
    On,
}

/// The audio thread: starts the audio engine, then runs game-audio jobs
/// until the engine thread hangs up.
fn audio_thread(
    settings: AudioSettings,
    sink: PacketSink,
    jobs: Receiver<AudioJob>,
    tx: Sender<Internal>,
) {
    let audio = match AudioEngine::start(settings, sink) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            let _ = tx.send(Internal::AudioReady(Err(e.to_string())));
            return;
        }
    };
    if tx.send(Internal::AudioReady(Ok(audio.clone()))).is_err() {
        return;
    }
    while let Ok(mut job) = jobs.recv() {
        // Each job sets the final state by itself, so only the newest counts.
        while let Ok(next) = jobs.try_recv() {
            job = next;
        }
        match job {
            AudioJob::StartGame { gen, source } => {
                let (result, apps) = start_game_audio(&audio, source);
                let _ = tx.send(Internal::GameAudioReady { gen, result, apps });
            }
            AudioJob::StopGame => {
                audio.stop_game_audio();
                log::info!("game audio stopped");
            }
        }
    }
}

/// Finds the chosen program and starts (or switches) the capture. Also
/// returns the fresh program list when it had to look.
fn start_game_audio(
    audio: &AudioEngine,
    source: GameSource,
) -> (Result<(), String>, Option<Vec<AudioApp>>) {
    let (source, apps) = match source {
        GameSource::AllExceptThisApp => (GameAudioSource::AllExceptThisApp, None),
        GameSource::Program(exe) => {
            let apps = gf_sys::list_audio_apps();
            match apps.iter().find(|a| a.exe.eq_ignore_ascii_case(&exe)) {
                Some(a) => (GameAudioSource::Process { pid: a.pid }, Some(apps)),
                None => {
                    // Don't leave a capture of the previous choice running.
                    audio.stop_game_audio();
                    let msg = format!("{exe} isn't running. Start it, click Refresh next to the program list, pick it, then click Start sending game audio again.");
                    return (Err(msg), Some(apps));
                }
            }
        }
    };
    log::info!("starting game audio from {source:?}");
    (
        audio.start_game_audio(source).map_err(|e| e.to_string()),
        apps,
    )
}

/// Drops `value` (stopping a child process, a session, ...) on a helper
/// thread so the engine thread never waits for it.
fn dispose<T: Send + 'static>(what: &str, value: T) {
    let res = std::thread::Builder::new()
        .name(format!("stop-{what}"))
        .spawn(move || drop(value));
    if let Err(e) = res {
        log::warn!("couldn't start a thread to stop the {what} ({e}); stopped it directly");
    }
}

/// Video from the latest keyframe on: what arrives while the viewer starts
/// (the new viewer gets it once it runs, so it can start at the opening
/// keyframe instead of waiting for the next one), and my share's latest
/// picture (see `STILL_SHARE`).
#[derive(Default)]
struct PendingFrames {
    frames: Vec<VideoFrame>,
    bytes: usize,
}

impl PendingFrames {
    fn push(&mut self, codec: VideoCodec, frame: VideoFrame) {
        if frame.codec != codec {
            return;
        }
        if frame.keyframe {
            self.clear();
        } else if self.frames.is_empty() {
            // Nothing to decode it from.
            return;
        }
        if self.frames.len() >= PENDING_FRAMES || self.bytes + frame.data.len() > PENDING_BYTES {
            // Too much: wait for the next keyframe.
            self.clear();
            return;
        }
        self.bytes += frame.data.len();
        self.frames.push(frame);
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }
}

fn spawn_helper(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        log::warn!("couldn't start the {name} thread: {e}");
    }
}

struct Core {
    settings: Settings,
    opts: EngineOptions,
    shared: Arc<Mutex<Snapshot>>,
    snap: Snapshot,
    /// Publish (and wake the UI) after this loop iteration.
    urgent: bool,
    internal_tx: Sender<Internal>,

    tools: Option<Tools>,
    tools_error: Option<String>,

    /// Set once the audio thread has started the audio engine.
    audio: Option<Arc<AudioEngine>>,
    /// Jobs for the audio thread; `None` if audio couldn't start.
    audio_jobs: Option<Sender<AudioJob>>,
    /// Where the audio engine's packets go (the current session, if any).
    audio_net: Arc<Mutex<Option<NetTx>>>,
    game_audio: GameAudio,
    /// Bumped for every game-audio start or stop, so a late result is discarded.
    game_gen: u64,

    net: Option<Net>,
    net_rx: Receiver<NetEvent>,
    /// Bumped for every new session, so a late host start-up is discarded.
    session_gen: u64,
    /// Host start-ups still running (a discarded one may hold the port).
    host_starts: usize,
    hosting: bool,
    last_close: Option<Instant>,

    encoder: Option<gf_video::Encoder>,
    enc_rx: Receiver<EncoderEvent>,
    enc_gen: u64,
    /// The settings the running (or starting) encoder uses.
    enc_settings: Option<EncoderSettings>,
    share_restart_at: Option<Instant>,
    /// Don't send video until a keyframe (the friend's viewer starts there).
    need_keyframe: bool,
    /// The friend was told `ShareStarted` (and not yet `ShareStopped`).
    share_announced: bool,
    /// My share's latest picture, and when the encoder last sent a frame.
    share_latest: PendingFrames,
    last_frame_at: Option<Instant>,

    viewer: Option<Viewer>,
    /// What the open viewer shows, and when it opened.
    viewer_for: Option<(ShareInfo, Instant)>,
    /// A viewer is starting on a helper thread (launching mpv can take
    /// ~0.4 s) for this share; its video waits in `viewer_pending`.
    viewer_starting: Option<ShareInfo>,
    viewer_pending: PendingFrames,
    /// Bumped whenever the viewer closes, so a late start is discarded.
    viewer_gen: u64,
    /// The user closed the viewer for the friend's current share: don't
    /// reopen it by itself until the friend shares again.
    viewer_declined: bool,

    /// Since when the game-audio capture has been reported not running
    /// while it should be.
    game_dead_since: Option<Instant>,
    /// When to next check whether an elevated program has focus.
    elevated_check_at: Instant,
    /// When an elevated program last had focus (with push-to-talk on).
    elevated_seen: Option<Instant>,

    save_at: Option<Instant>,
    fps_at: Instant,
    fps_sent: u64,
    fps_recv: u64,
}

/// The engine thread's body.
pub(super) fn run(
    settings: Settings,
    opts: EngineOptions,
    commands: Receiver<Command>,
    shared: Arc<Mutex<Snapshot>>,
) {
    let (internal_tx, internal_rx) = crossbeam_channel::unbounded();
    let mut core = Core {
        settings,
        opts,
        shared,
        snap: Snapshot::default(),
        urgent: true,
        internal_tx,
        tools: None,
        tools_error: None,
        audio: None,
        audio_jobs: None,
        audio_net: Arc::new(Mutex::new(None)),
        game_audio: GameAudio::Off,
        game_gen: 0,
        net: None,
        net_rx: never(),
        session_gen: 0,
        host_starts: 0,
        hosting: false,
        last_close: None,
        encoder: None,
        enc_rx: never(),
        enc_gen: 0,
        enc_settings: None,
        share_restart_at: None,
        need_keyframe: true,
        share_announced: false,
        share_latest: PendingFrames::default(),
        last_frame_at: None,
        viewer: None,
        viewer_for: None,
        viewer_starting: None,
        viewer_pending: PendingFrames::default(),
        viewer_gen: 0,
        viewer_declined: false,
        game_dead_since: None,
        elevated_check_at: Instant::now(),
        elevated_seen: None,
        save_at: None,
        fps_at: Instant::now(),
        fps_sent: 0,
        fps_recv: 0,
    };
    core.startup();
    let ticker = tick(TICK);
    loop {
        select! {
            recv(commands) -> cmd => match cmd {
                Ok(Command::Shutdown) | Err(_) => break,
                Ok(cmd) => core.command(cmd),
            },
            recv(core.net_rx) -> ev => match ev {
                Ok(ev) => core.net_event(ev),
                Err(_) => core.net_rx = never(),
            },
            recv(core.enc_rx) -> ev => match ev {
                Ok(ev) => core.encoder_event(ev),
                Err(_) => core.enc_rx = never(),
            },
            recv(internal_rx) -> msg => {
                if let Ok(msg) = msg {
                    core.internal(msg);
                }
            },
            recv(ticker) -> _ => core.tick(),
        }
        if core.urgent {
            core.publish(true);
        }
    }
    core.shutdown();
}

impl Core {
    // ------------------------------------------------------------ plumbing

    fn publish(&mut self, notify: bool) {
        *lock(&self.shared) = self.snap.clone();
        self.urgent = false;
        if notify {
            if let Some(cb) = &self.opts.on_change {
                cb();
            }
        }
    }

    fn error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::warn!("shown to the user: {msg}");
        self.snap.error = Some(msg);
        self.urgent = true;
    }

    fn info(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::info!("{msg}");
        self.snap.info = Some(msg);
        self.urgent = true;
    }

    fn connected(&self) -> bool {
        matches!(self.snap.phase, Phase::Connected { .. })
    }

    /// Control message to the friend (dropped by gf-net while not connected).
    fn send(&self, msg: TcpMsg) {
        if let Some(net) = &self.net {
            net.tx.send(msg);
        }
    }

    fn port(&self) -> u16 {
        self.opts.port.unwrap_or(self.settings.port)
    }

    fn peer_label(&self) -> String {
        self.snap
            .peer_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "Your friend".into())
    }

    // ------------------------------------------------------------ start / stop

    fn startup(&mut self) {
        log::info!("engine starting");
        self.publish(false);
        self.refresh_lists();
        {
            let tx = self.internal_tx.clone();
            spawn_helper("tailscale-ip", move || {
                let _ = tx.send(Internal::TailscaleIp(gf_sys::tailscale_ipv4()));
            });
        }
        match Tools::locate() {
            Ok(tools) => {
                self.snap.video_available = true;
                self.snap.probing_encoders = true;
                let tx = self.internal_tx.clone();
                let t = tools.clone();
                spawn_helper("probe-encoders", move || {
                    let _ = tx.send(Internal::Probed(gf_video::probe_encoders(&t)));
                });
                self.tools = Some(tools);
            }
            Err(e) => {
                let msg =
                    format!("{e} Voice chat still works, but screen sharing and watching don't.");
                self.tools_error = Some(e.to_string());
                self.error(msg);
            }
        }
        let slot = self.audio_net.clone();
        let sink: PacketSink = Arc::new(move |stream, seq, ts, data: &[u8]| {
            let tx = lock(&slot).clone();
            if let Some(tx) = tx {
                tx.send_audio(stream, seq, ts, data);
            }
        });
        let (jobs_tx, jobs_rx) = crossbeam_channel::unbounded();
        let settings = AudioSettings::from(&self.settings.audio);
        let tx = self.internal_tx.clone();
        match std::thread::Builder::new()
            .name("audio-control".into())
            .spawn(move || audio_thread(settings, sink, jobs_rx, tx))
        {
            Ok(_) => self.audio_jobs = Some(jobs_tx),
            Err(e) => {
                log::error!("couldn't start the audio thread: {e}");
                self.error(NO_AUDIO);
            }
        }
        self.publish(true);
    }

    fn shutdown(&mut self) {
        log::info!("engine shutting down");
        self.save_now();
        self.close_net();
        // Stopped here, not on helper threads: the process may exit right after.
        self.enc_rx = never();
        self.encoder = None;
        self.viewer = None;
        // The audio thread ends once it has no more jobs; whichever of us
        // drops the audio engine last stops it (game audio included).
        self.audio_jobs = None;
        self.audio = None;
        self.snap = Snapshot::default();
        self.publish(true);
        log::info!("engine stopped");
    }

    fn save_now(&mut self) {
        self.save_at = None;
        if let Some(path) = &self.opts.settings_path {
            match self.settings.save(path) {
                Ok(()) => log::debug!("settings saved"),
                Err(e) => log::warn!("couldn't save settings to {}: {e}", path.display()),
            }
        }
    }

    fn refresh_lists(&self) {
        let tx = self.internal_tx.clone();
        spawn_helper("list-devices", move || {
            let _ = tx.send(Internal::Lists {
                monitors: gf_sys::list_monitors(),
                inputs: gf_audio::list_input_devices(),
                outputs: gf_audio::list_output_devices(),
                apps: gf_sys::list_audio_apps(),
            });
        });
    }

    // ------------------------------------------------------------ commands

    fn command(&mut self, cmd: Command) {
        log::debug!("command: {}", command_name(&cmd));
        self.urgent = true;
        match cmd {
            Command::Host => self.host(),
            Command::Join(code) => self.join_code(&code),
            Command::JoinAddr { addr, secret } => self.join(addr, secret),
            Command::Leave => self.leave(),
            Command::StartSharing => {
                self.snap.info = None;
                self.start_sharing();
            }
            Command::StopSharing => self.stop_sharing(true),
            Command::OpenViewer => self.open_viewer_cmd(),
            Command::StartGameAudio => self.start_game_audio(),
            Command::StopGameAudio => self.stop_game_audio(true),
            Command::ApplySettings(s) => self.apply_settings(*s),
            Command::RefreshLists => self.refresh_lists(),
            Command::DismissError => self.snap.error = None,
            Command::Shutdown => {}
        }
    }

    fn host(&mut self) {
        if matches!(
            self.snap.phase,
            Phase::Connected { .. } | Phase::Reconnecting | Phase::Joining
        ) {
            self.end_session();
        } else {
            // Hosting again only gives a new code; local sharing continues.
            self.close_net();
        }
        self.snap.error = None;
        self.snap.info = None;
        self.session_gen += 1;
        self.hosting = true;
        self.snap.room_code = None;
        self.snap.listen_addr = None;
        self.snap.phase = Phase::Hosting {
            code: String::new(),
            waiting: false,
        };
        let gen = self.session_gen;
        let bind_override = self.opts.bind_ip;
        let cfg = NetConfig {
            role: Role::Host {
                bind_ip: Ipv4Addr::UNSPECIFIED,
            },
            port: self.port(),
            secret: 0,
            name: self.settings.name.clone(),
            encoders: self.snap.my_encoders.clone(),
        };
        let retry_until = self.last_close.map(|t| t + REBIND_WINDOW);
        // An older start-up (now discarded) may be about to take the port.
        let overlap = self.host_starts > 0;
        self.host_starts += 1;
        let tx = self.internal_tx.clone();
        spawn_helper("host-start", move || {
            let result = start_host(cfg, bind_override, retry_until, overlap);
            // If the engine is gone, this drops (closes) the new session.
            let _ = tx.send(Internal::HostReady { gen, result });
        });
    }

    fn join_code(&mut self, text: &str) {
        match RoomCode::parse(text) {
            Ok(code) => {
                let port = match self.port() {
                    0 => self.settings.port,
                    p => p,
                };
                let ip = self.opts.join_ip.unwrap_or(code.ip);
                self.join(SocketAddr::new(IpAddr::V4(ip), port), code.secret);
            }
            Err(e) => self.error(format!(
                "That room code doesn't look right: {e}. Ask your friend to send it again, then paste it exactly."
            )),
        }
    }

    fn join(&mut self, addr: SocketAddr, secret: u32) {
        if self.net.is_some() || self.hosting {
            self.end_session();
        }
        self.snap.error = None;
        self.snap.info = None;
        let IpAddr::V4(host_ip) = addr.ip() else {
            self.error("That address can't be used. Use the room code your friend sent.");
            return;
        };
        self.session_gen += 1;
        let cfg = NetConfig {
            role: Role::Join { host_ip },
            port: addr.port(),
            secret,
            name: self.settings.name.clone(),
            encoders: self.snap.my_encoders.clone(),
        };
        match gf_net::start(cfg) {
            Ok(net) => {
                log::info!("joining the room at {addr}");
                self.install_net(net);
                self.snap.phase = Phase::Joining;
            }
            Err(e) => self.error(e.to_string()),
        }
    }

    fn leave(&mut self) {
        if self.net.is_some() || self.hosting {
            log::info!("leaving the room");
        }
        self.end_session();
        self.snap.info = None;
    }

    /// Ends the session (if any) and everything that belongs to it.
    fn end_session(&mut self) {
        self.session_gen += 1;
        self.close_net();
        self.peer_gone();
        self.stop_local_media();
        self.hosting = false;
        self.snap.phase = Phase::Idle;
        self.snap.room_code = None;
        self.snap.listen_addr = None;
        self.snap.net = Default::default();
    }

    fn install_net(&mut self, net: Net) {
        self.net_rx = net.events.clone();
        *lock(&self.audio_net) = Some(net.tx.clone());
        self.net = Some(net);
    }

    fn close_net(&mut self) {
        self.net_rx = never();
        *lock(&self.audio_net) = None;
        if let Some(net) = self.net.take() {
            // Sends Bye; returns within ~150 ms.
            drop(net);
            self.last_close = Some(Instant::now());
        }
    }

    /// The friend is gone (for now): forget everything about them.
    fn peer_gone(&mut self) {
        self.snap.peer_name = None;
        self.snap.peer_encoders.clear();
        self.snap.peer_share = None;
        self.snap.peer_watching = false;
        self.snap.peer_game_audio = false;
        self.snap.frames_received = 0;
        self.close_viewer();
        self.viewer_declined = false;
        self.need_keyframe = true;
        self.share_announced = false;
        if let Some(a) = &self.audio {
            a.reset_peer();
        }
    }

    fn stop_local_media(&mut self) {
        self.stop_sharing(false);
        self.stop_game_audio(false);
    }

    // ------------------------------------------------------------ sharing

    /// The encoder settings for the current settings, or a plain error.
    fn encoder_settings(&self) -> Result<EncoderSettings, String> {
        let s = &self.settings;
        let mine = &self.snap.my_encoders;
        let codec = match s.codec.codec() {
            None => *mine.first().ok_or(NO_ENCODER)?,
            Some(c) if mine.contains(&c) => c,
            Some(_) if mine.is_empty() => return Err(NO_ENCODER.into()),
            Some(c) => {
                return Err(format!(
                    "Your graphics card can't send {} video. Under \"Your screen\", choose Auto (best available) or another codec.",
                    c.label()
                ))
            }
        };
        let monitors = &self.snap.monitors;
        let monitor = if monitors.is_empty() || monitors.iter().any(|m| m.index == s.monitor) {
            s.monitor
        } else {
            let m = monitors.iter().find(|m| m.primary).unwrap_or(&monitors[0]);
            log::warn!(
                "monitor {} not found; sharing {} instead",
                s.monitor,
                m.name
            );
            m.index
        };
        Ok(EncoderSettings {
            codec,
            monitor,
            fps: s.fps,
            max_height: s.max_height,
            bitrate_kbps: s.bitrate_kbps(codec),
            keyframe_interval: s.keyframe_interval(),
            window_exe: s.share_app.clone(),
        })
    }

    fn start_sharing(&mut self) {
        let Some(tools) = self.tools.clone() else {
            let msg = self
                .tools_error
                .clone()
                .unwrap_or_else(|| "Screen sharing isn't available.".into());
            self.error(msg);
            return;
        };
        if matches!(
            self.snap.my_share,
            ShareState::Starting | ShareState::Live(_)
        ) {
            return;
        }
        if self.snap.probing_encoders {
            self.snap.my_share = ShareState::Waiting;
            self.info("Checking which video formats your graphics card can send. Sharing starts in a moment…");
            return;
        }
        let es = match self.encoder_settings() {
            Ok(es) => es,
            Err(msg) => {
                self.snap.my_share = ShareState::Off;
                self.error(msg);
                return;
            }
        };
        self.enc_gen += 1;
        let gen = self.enc_gen;
        log::info!(
            "starting to share {} ({} {} kbps, {} fps, max height {:?})",
            match &es.window_exe {
                Some(exe) => format!("{exe}'s window"),
                None => format!("monitor {}", es.monitor),
            },
            es.codec.label(),
            es.bitrate_kbps,
            es.fps,
            es.max_height
        );
        self.enc_settings = Some(es.clone());
        self.snap.my_share = ShareState::Starting;
        let tx = self.internal_tx.clone();
        spawn_helper("encoder-start", move || {
            let result = gf_video::Encoder::start(&tools, &es).map_err(|e| e.to_string());
            let _ = tx.send(Internal::EncoderReady { gen, result });
        });
    }

    fn announce_share(&mut self, info: ShareInfo) {
        self.send(TcpMsg::ShareStarted {
            codec: info.codec,
            width: info.width,
            height: info.height,
            fps: info.fps,
        });
        self.share_announced = true;
    }

    fn announce_share_stopped(&mut self) {
        if self.share_announced {
            self.send(TcpMsg::ShareStopped);
            self.share_announced = false;
        }
        // The friend's viewer closes on ShareStopped without saying so.
        self.snap.peer_watching = false;
    }

    /// Stops sharing. `announce`: tell the friend (not when the session is
    /// ending anyway, or when a new encoder follows at once).
    fn stop_sharing(&mut self, announce: bool) {
        self.enc_gen += 1;
        self.enc_rx = never();
        if let Some(enc) = self.encoder.take() {
            dispose("encoder", enc);
        }
        self.enc_settings = None;
        self.share_restart_at = None;
        self.share_latest.clear();
        self.last_frame_at = None;
        self.snap.my_share = ShareState::Off;
        self.snap.frames_sent = 0;
        self.snap.send_fps = 0.0;
        if announce {
            self.announce_share_stopped();
        }
    }

    /// New video settings while sharing: start a new encoder. The friend
    /// gets a new `ShareStarted` (no `ShareStopped`), so their viewer stays
    /// open if the codec and size are unchanged.
    fn restart_sharing(&mut self) {
        self.share_restart_at = None;
        if self.enc_settings.is_none() {
            return;
        }
        let minimized = (self.encoder.as_ref().and_then(|e| e.window()))
            .is_some_and(|w| matches!(gf_sys::window_state(w), WindowState::Minimized));
        let old_exe = self.enc_settings.as_ref().map(|e| &e.window_exe);
        match self.encoder_settings() {
            Ok(es) if Some(&es) == self.enc_settings.as_ref() => return,
            // A minimized window can't start a share, but minimizing only
            // pauses one: keep it going and restart once it's restored.
            Ok(es) if minimized && old_exe == Some(&es.window_exe) => {
                log::debug!("the shared window is minimized; restarting sharing once restored");
                self.share_restart_at = Some(Instant::now() + RESTART_DELAY);
                return;
            }
            Ok(_) => {}
            Err(msg) => {
                self.stop_sharing(true);
                self.error(msg);
                return;
            }
        }
        log::info!("video settings changed; restarting screen sharing");
        self.stop_sharing(false);
        self.start_sharing();
        if self.snap.my_share == ShareState::Off {
            self.announce_share_stopped();
        }
    }

    fn encoder_event(&mut self, ev: EncoderEvent) {
        match ev {
            EncoderEvent::Started {
                codec,
                width,
                height,
                fps,
            } => {
                let info = ShareInfo {
                    codec,
                    width,
                    height,
                    fps,
                };
                self.snap.my_share = ShareState::Live(info);
                self.need_keyframe = true;
                self.urgent = true;
                self.announce_share(info);
            }
            EncoderEvent::Frame(frame) => {
                self.last_frame_at = Some(Instant::now());
                self.share_latest.push(frame.codec, frame.clone());
                self.send_frame(frame);
            }
            EncoderEvent::Stopped { error } => {
                self.stop_sharing(true);
                if let Some(e) = error {
                    self.error(first_paragraph(&e));
                }
            }
        }
    }

    fn send_frame(&mut self, frame: VideoFrame) {
        if !self.snap.peer_watching || !self.connected() {
            return;
        }
        if self.need_keyframe {
            if !frame.keyframe {
                return;
            }
            self.need_keyframe = false;
        }
        self.snap.frames_sent += 1;
        if let Some(net) = &self.net {
            net.tx.send_video(frame);
        }
    }

    // ------------------------------------------------------------ viewer

    /// Starts the viewer on a helper thread: launching mpv can take several
    /// hundred ms, and the engine thread must keep passing on audio.
    fn open_viewer(&mut self, info: ShareInfo) {
        self.close_viewer();
        let Some(tools) = self.tools.clone() else {
            let msg = self
                .tools_error
                .clone()
                .unwrap_or_else(|| "The viewer isn't available.".into());
            self.error(msg);
            return;
        };
        let title = format!("{}'s screen - GloorpFlorp", self.peer_label());
        let gen = self.viewer_gen;
        self.viewer_starting = Some(info);
        self.viewer_declined = false;
        self.snap.viewer_open = true;
        self.urgent = true;
        // Ask for video now; it waits in `viewer_pending` until mpv runs.
        self.send(TcpMsg::Watching { on: true });
        let tx = self.internal_tx.clone();
        spawn_helper("viewer-start", move || {
            let started = Instant::now();
            let result = Viewer::start(&tools, info.codec, &title).map_err(|e| e.to_string());
            log::debug!("viewer start took {} ms", started.elapsed().as_millis());
            // If the engine is gone, this drops (closes) the viewer.
            let _ = tx.send(Internal::ViewerReady { gen, result });
        });
    }

    fn viewer_ready(&mut self, gen: u64, result: Result<Viewer, String>) {
        let info = match self.viewer_starting {
            Some(info) if gen == self.viewer_gen => info,
            _ => {
                if let Ok(v) = result {
                    dispose("viewer", v);
                }
                return;
            }
        };
        self.viewer_starting = None;
        let pending = std::mem::take(&mut self.viewer_pending);
        match result {
            Ok(v) => {
                log::debug!(
                    "viewer started; passing on {} frame(s) that arrived meanwhile",
                    pending.frames.len()
                );
                self.viewer_for = Some((info, Instant::now()));
                let open = pending.frames.iter().all(|f| v.push(f));
                self.viewer = Some(v);
                if !open {
                    self.viewer_closed();
                }
            }
            Err(e) => {
                self.snap.viewer_open = false;
                self.send(TcpMsg::Watching { on: false });
                self.error(e);
            }
        }
    }

    fn close_viewer(&mut self) {
        self.viewer_gen += 1;
        self.viewer_starting = None;
        self.viewer_pending.clear();
        if let Some(v) = self.viewer.take() {
            dispose("viewer", v);
        }
        self.viewer_for = None;
        self.snap.viewer_open = false;
    }

    /// The viewer window was closed (by the user, or mpv exited).
    fn viewer_closed(&mut self) {
        let early = self
            .viewer_for
            .is_some_and(|(_, at)| at.elapsed() < VIEWER_EARLY_CLOSE);
        log::info!("the viewer window was closed");
        self.close_viewer();
        self.viewer_declined = true;
        self.send(TcpMsg::Watching { on: false });
        if early {
            self.error("The viewer window closed right after opening. Click Open viewer to try again. If it keeps happening, extract the whole GloorpFlorp zip again.");
        }
        self.urgent = true;
    }

    fn open_viewer_cmd(&mut self) {
        match self.snap.peer_share {
            Some(info) => {
                if self.viewer_starting.is_some()
                    || self.viewer.as_ref().is_some_and(|v| v.is_open())
                {
                    return;
                }
                self.open_viewer(info);
            }
            None => self.info(format!(
                "{} isn't sharing their screen right now.",
                self.peer_label()
            )),
        }
    }

    fn peer_share_started(&mut self, info: ShareInfo) {
        log::info!(
            "friend is sharing {} {}x{} @ {} fps",
            info.codec.label(),
            info.width,
            info.height,
            info.fps
        );
        if self.viewer.as_ref().is_some_and(|v| !v.is_open()) {
            self.viewer_closed();
        }
        if self.snap.peer_share.is_none() {
            self.viewer_declined = false;
        }
        self.snap.peer_share = Some(info);
        self.snap.frames_received = 0;
        let same_picture =
            |s: ShareInfo| (s.codec, s.width, s.height) == (info.codec, info.width, info.height);
        // What the open (or still starting) viewer shows.
        let shown = self.viewer_for.map(|(s, _)| s).or(self.viewer_starting);
        match shown {
            Some(shown) if same_picture(shown) => {
                // Same picture format (the friend restarted their encoder,
                // e.g. a new frame rate): keep the window.
                self.send(TcpMsg::Watching { on: true });
            }
            Some(_) => self.open_viewer(info),
            None => {
                if self.settings.auto_open_viewer && !self.viewer_declined {
                    self.open_viewer(info);
                }
            }
        }
    }

    // ------------------------------------------------------------ game audio

    /// The capture source for the game-audio setting, or a plain error.
    fn game_audio_source(&self) -> Result<GameSource, String> {
        match &self.settings.game_audio {
            GameAudioMode::AllExceptThisApp => Ok(GameSource::AllExceptThisApp),
            GameAudioMode::App { exe } if exe.trim().is_empty() => {
                Err("Choose the program to send under \"Game audio\" first.".into())
            }
            GameAudioMode::App { exe } => Ok(GameSource::Program(exe.clone())),
        }
    }

    /// Push-to-talk can't see its key while a program running as
    /// administrator has focus. Remembered for a few seconds, so the hint is
    /// still there after switching back to GloorpFlorp's window.
    fn check_ptt_blocked(&mut self, now: Instant) {
        let ptt = self.settings.audio.voice_mode == VoiceModeChoice::PushToTalk;
        if ptt && gf_sys::foreground_is_elevated() {
            self.elevated_seen = Some(now);
        }
        let blocked = ptt
            && self
                .elevated_seen
                .is_some_and(|t| now.duration_since(t) < ELEVATED_HINT_HOLD);
        if blocked != self.snap.ptt_blocked {
            if blocked {
                log::info!("an elevated program has focus: push-to-talk can't see its key");
            }
            self.snap.ptt_blocked = blocked;
            self.urgent = true;
        }
    }

    fn set_game_audio(&mut self, state: GameAudio) {
        self.game_audio = state;
        self.game_dead_since = None;
        self.snap.my_game_audio = state != GameAudio::Off;
        self.snap.game_audio_starting = state == GameAudio::Starting;
        self.urgent = true;
    }

    /// Starts game audio, or switches it to the source now in the settings.
    /// The capture starts on the audio thread (Windows can take a while to
    /// activate it); the friend is told once it runs.
    fn start_game_audio(&mut self) {
        let Some(jobs) = self.audio_jobs.clone() else {
            self.error(NO_AUDIO);
            return;
        };
        let source = match self.game_audio_source() {
            Ok(s) => s,
            Err(msg) => {
                self.stop_game_audio(true);
                self.error(msg);
                return;
            }
        };
        self.game_gen += 1;
        let gen = self.game_gen;
        if jobs.send(AudioJob::StartGame { gen, source }).is_err() {
            self.stop_game_audio(true);
            self.error(NO_AUDIO);
            return;
        }
        self.set_game_audio(GameAudio::Starting);
    }

    fn game_audio_started(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.set_game_audio(GameAudio::On);
                self.send(TcpMsg::GameAudio { active: true });
            }
            Err(msg) => {
                self.set_game_audio(GameAudio::Off);
                self.send(TcpMsg::GameAudio { active: false });
                self.error(msg);
            }
        }
    }

    fn stop_game_audio(&mut self, announce: bool) {
        if self.game_audio == GameAudio::Off {
            return;
        }
        self.game_gen += 1;
        if let Some(jobs) = &self.audio_jobs {
            let _ = jobs.send(AudioJob::StopGame);
        }
        self.set_game_audio(GameAudio::Off);
        if announce {
            self.send(TcpMsg::GameAudio { active: false });
        }
    }

    // ------------------------------------------------------------ settings

    fn apply_settings(&mut self, mut new: Settings) {
        new.sanitize();
        if new == self.settings {
            return;
        }
        let old = std::mem::replace(&mut self.settings, new);
        if old.audio != self.settings.audio {
            if let Some(a) = &self.audio {
                a.set_settings(AudioSettings::from(&self.settings.audio));
            }
        }
        if self.enc_settings.is_some() {
            let changed = (
                old.monitor,
                old.max_height,
                old.fps,
                old.codec,
                old.bitrate_mbps,
            ) != (
                self.settings.monitor,
                self.settings.max_height,
                self.settings.fps,
                self.settings.codec,
                self.settings.bitrate_mbps,
            ) || old.keyframe_secs != self.settings.keyframe_secs
                || old.share_app != self.settings.share_app;
            if changed {
                self.share_restart_at = Some(Instant::now() + RESTART_DELAY);
            }
        }
        // Switch the running capture, except to "Only one program" before a
        // program is picked (the old source keeps going until then).
        let unpicked = matches!(&self.settings.game_audio, GameAudioMode::App { exe } if exe.trim().is_empty());
        if self.snap.my_game_audio && old.game_audio != self.settings.game_audio && !unpicked {
            self.start_game_audio();
        }
        self.save_at = Some(Instant::now() + SAVE_DELAY);
    }

    // ------------------------------------------------------------ events

    fn internal(&mut self, msg: Internal) {
        self.urgent = true;
        match msg {
            Internal::Probed(list) => {
                log::info!("this PC can share with: {list:?}");
                self.snap.my_encoders = list;
                self.snap.probing_encoders = false;
                if self.snap.my_share == ShareState::Waiting {
                    self.snap.my_share = ShareState::Off;
                    self.snap.info = None;
                    self.start_sharing();
                }
            }
            Internal::Lists {
                monitors,
                inputs,
                outputs,
                apps,
            } => {
                log::debug!(
                    "{} monitor(s), {} microphone(s), {} speaker(s), {} program(s)",
                    monitors.len(),
                    inputs.len(),
                    outputs.len(),
                    apps.len()
                );
                self.snap.monitors = monitors;
                self.snap.input_devices = inputs;
                self.snap.output_devices = outputs;
                self.snap.audio_apps = apps;
            }
            Internal::TailscaleIp(ip) => {
                if self.snap.tailscale_ip.is_none() {
                    self.snap.tailscale_ip = ip;
                }
            }
            Internal::HostReady { gen, result } => {
                self.host_starts = self.host_starts.saturating_sub(1);
                if gen != self.session_gen {
                    if let Ok(h) = result {
                        dispose("old room", h.net);
                    }
                    return;
                }
                match result {
                    Ok(h) => {
                        if is_tailscale_ipv4(h.ip) {
                            self.snap.tailscale_ip = Some(h.ip);
                        }
                        self.snap.room_code = Some(h.code.clone());
                        self.snap.phase = Phase::Hosting {
                            code: h.code,
                            waiting: false,
                        };
                        self.install_net(h.net);
                    }
                    Err(msg) => {
                        self.hosting = false;
                        self.snap.phase = Phase::Idle;
                        self.error(msg);
                    }
                }
            }
            Internal::EncoderReady { gen, result } => {
                if gen != self.enc_gen {
                    if let Ok((enc, rx)) = result {
                        drop(rx);
                        dispose("encoder", enc);
                    }
                    return;
                }
                match result {
                    Ok((enc, rx)) => {
                        self.encoder = Some(enc);
                        self.enc_rx = rx;
                    }
                    Err(msg) => {
                        self.enc_settings = None;
                        self.snap.my_share = ShareState::Off;
                        self.announce_share_stopped();
                        self.error(first_paragraph(&msg));
                    }
                }
            }
            Internal::ViewerReady { gen, result } => self.viewer_ready(gen, result),
            Internal::AudioReady(Ok(audio)) => {
                // The settings may have changed while it started.
                audio.set_settings(AudioSettings::from(&self.settings.audio));
                self.snap.audio = audio.status();
                self.snap.audio_running = true;
                self.audio = Some(audio);
            }
            Internal::AudioReady(Err(msg)) => {
                self.audio_jobs = None;
                if self.game_audio != GameAudio::Off {
                    self.set_game_audio(GameAudio::Off);
                }
                self.error(format!(
                    "Voice chat couldn't start ({}). Restart GloorpFlorp. If it keeps happening, send your log file.",
                    msg.trim().trim_end_matches('.')
                ));
            }
            Internal::GameAudioReady { gen, result, apps } => {
                if let Some(apps) = apps {
                    self.snap.audio_apps = apps;
                }
                if gen == self.game_gen {
                    self.game_audio_started(result);
                }
            }
        }
    }

    fn net_event(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::Video(frame) => {
                self.snap.frames_received += 1;
                if let Some(info) = self.viewer_starting {
                    self.viewer_pending.push(info.codec, frame);
                } else if self.viewer.as_ref().is_some_and(|v| !v.push(&frame)) {
                    self.viewer_closed();
                }
            }
            NetEvent::Audio {
                stream,
                seq,
                ts,
                payload,
                arrived,
            } => {
                if let Some(a) = &self.audio {
                    a.receive_at(stream, seq, ts, payload, arrived);
                }
            }
            NetEvent::Stats(stats) => self.snap.net = stats,
            NetEvent::Listening { addr } => {
                self.urgent = true;
                self.snap.listen_addr = Some(addr);
                if self.hosting {
                    let code = self.snap.room_code.clone().unwrap_or_default();
                    self.snap.phase = Phase::Hosting {
                        code,
                        waiting: true,
                    };
                }
            }
            NetEvent::Connected {
                peer_name,
                peer_encoders,
            } => {
                self.urgent = true;
                let name = crate::settings::clean_name(&peer_name);
                let name = if name.is_empty() {
                    "Your friend".to_string()
                } else {
                    name
                };
                log::info!("connected to {name} (can share {peer_encoders:?})");
                let reconnected = self.snap.phase == Phase::Reconnecting;
                self.peer_gone();
                self.snap.peer_name = Some(name.clone());
                self.snap.peer_encoders = peer_encoders;
                self.snap.phase = Phase::Connected { peer: name.clone() };
                self.snap.info = Some(if reconnected {
                    format!("Reconnected to {name}.")
                } else {
                    format!("{name} is here. You can talk now.")
                });
                // Tell the (possibly reconnected) friend what we're doing.
                if let ShareState::Live(i) = self.snap.my_share {
                    self.announce_share(i);
                }
                if self.game_audio == GameAudio::On {
                    self.send(TcpMsg::GameAudio { active: true });
                }
            }
            NetEvent::Msg(msg) => self.peer_msg(msg),
            NetEvent::Disconnected { reason, will_retry } => {
                self.urgent = true;
                log::info!("disconnected (retrying: {will_retry}): {reason}");
                let was_reconnecting = self.snap.phase == Phase::Reconnecting;
                self.peer_gone();
                if will_retry {
                    // Keep sharing and game audio; they're announced again on
                    // reconnect. (The UI says it is reconnecting.)
                    self.snap.phase = Phase::Reconnecting;
                    self.info(reason);
                } else if self.hosting {
                    // The host can't tell a network blip from leaving, so its
                    // sharing and game audio keep running (like before the
                    // friend first joined) and are announced again when the
                    // friend rejoins.
                    let code = self.snap.room_code.clone().unwrap_or_default();
                    self.snap.phase = Phase::Hosting {
                        code,
                        waiting: true,
                    };
                    self.info(format!(
                        "{reason} The room stays open, so they can join again with the same code."
                    ));
                } else {
                    // The joiner's session is over for good.
                    let had_media =
                        self.snap.my_share != ShareState::Off || self.game_audio != GameAudio::Off;
                    self.stop_local_media();
                    self.close_net();
                    self.snap.phase = Phase::Idle;
                    self.snap.net = Default::default();
                    self.snap.info = None;
                    let reason = if had_media {
                        format!("{reason} Your screen sharing and game audio were stopped.")
                    } else {
                        reason
                    };
                    if was_reconnecting {
                        self.error(reason);
                    } else {
                        self.info(reason);
                    }
                }
            }
            NetEvent::Rejected { reason } | NetEvent::Error(reason) => {
                // A joiner's first attempt, or a host whose network went
                // away. Local sharing and game audio continue.
                self.session_gen += 1;
                self.close_net();
                self.peer_gone();
                self.hosting = false;
                self.snap.phase = Phase::Idle;
                self.snap.room_code = None;
                self.snap.listen_addr = None;
                self.snap.net = Default::default();
                self.snap.info = None;
                self.error(reason);
            }
        }
    }

    fn peer_msg(&mut self, msg: TcpMsg) {
        self.urgent = true;
        match msg {
            TcpMsg::ShareStarted {
                codec,
                width,
                height,
                fps,
            } => self.peer_share_started(ShareInfo {
                codec,
                width,
                height,
                fps,
            }),
            TcpMsg::ShareStopped => {
                log::info!("friend stopped sharing");
                self.snap.peer_share = None;
                self.viewer_declined = false;
                self.close_viewer();
            }
            TcpMsg::Watching { on } => {
                // An answer to a share that has stopped since.
                if on && !self.share_announced {
                    log::debug!("ignoring Watching from the friend: not sharing");
                    return;
                }
                log::info!("friend {} watching", if on { "started" } else { "stopped" });
                // Only a newly opened viewer needs a keyframe; the friend
                // also confirms (again) after each encoder restart, which
                // already starts at one.
                if on && !self.snap.peer_watching {
                    self.need_keyframe = true;
                }
                self.snap.peer_watching = on;
            }
            TcpMsg::GameAudio { active } => {
                log::info!("friend's game audio: {}", if active { "on" } else { "off" });
                self.snap.peer_game_audio = active;
            }
            other => log::debug!("ignoring unexpected message from the friend: {other:?}"),
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.viewer.as_ref().is_some_and(|v| !v.is_open()) {
            self.viewer_closed();
        }
        if let Some(a) = &self.audio {
            self.snap.audio = a.status();
        }
        // (The audio engine keeps reporting a failed capture until the audio
        // thread has dropped it.)
        self.snap.game_audio_note = self
            .snap
            .audio
            .game_error
            .clone()
            .filter(|_| self.game_audio == GameAudio::On);
        // A capture that reports a problem (e.g. the chosen program exited)
        // has stopped for good: tell the user and the friend.
        if self.game_audio == GameAudio::On && !self.snap.audio.game_capturing {
            let since = *self.game_dead_since.get_or_insert(now);
            if now.duration_since(since) >= GAME_DEAD_AFTER {
                let msg = self.snap.audio.game_error.clone();
                log::warn!("game audio capture stopped: {msg:?}");
                self.stop_game_audio(true);
                self.snap.game_audio_note = None;
                self.error(msg.unwrap_or_else(|| GAME_AUDIO_DIED.into()));
            }
        } else {
            self.game_dead_since = None;
        }
        if now >= self.elevated_check_at {
            self.elevated_check_at = now + ELEVATED_CHECK;
            self.check_ptt_blocked(now);
        }
        let secs = now.duration_since(self.fps_at).as_secs_f32();
        if secs >= 1.0 {
            let rate = |now: u64, before: u64| now.saturating_sub(before) as f32 / secs;
            self.snap.send_fps = rate(self.snap.frames_sent, self.fps_sent);
            self.snap.recv_fps = rate(self.snap.frames_received, self.fps_recv);
            self.fps_sent = self.snap.frames_sent;
            self.fps_recv = self.snap.frames_received;
            self.fps_at = now;
        }
        // shortcut: a keyframe group past PENDING_FRAMES/PENDING_BYTES (long
        // keyframe interval at a high frame rate or bitrate) isn't kept, so
        // that viewer waits until the window changes; restart the encoder
        // (an open window sends a first picture) if that turns out to matter.
        if self.need_keyframe
            && self.snap.peer_watching
            && self.connected()
            && !self.share_latest.frames.is_empty()
            && self
                .last_frame_at
                .is_some_and(|t| now.duration_since(t) >= STILL_SHARE)
        {
            log::debug!("my share is still; sending the friend's new viewer its latest picture");
            // As one group: frame by frame, gf-net drops a group longer than
            // its queue (MAX_VIDEO_QUEUE), keyframe and all.
            let group = self.share_latest.frames.clone();
            self.snap.frames_sent += group.len() as u64;
            self.need_keyframe = false;
            if let Some(net) = &self.net {
                net.tx.send_video_group(group);
            }
        }
        if self.share_restart_at.is_some_and(|t| now >= t) {
            self.restart_sharing();
        }
        if self.save_at.is_some_and(|t| now >= t) {
            self.save_now();
        }
        let notify = self.urgent;
        self.publish(notify);
    }
}

/// Finds the address, makes the room code and opens the port (on a helper
/// thread: the Tailscale lookup can take seconds).
///
/// `retry_until`: keep trying until then (the previous room's port may still
/// be closing). `overlap`: another start-up was still running, so keep
/// trying for a while after the lookup too (its room is closed as soon as
/// it reports back).
fn start_host(
    mut cfg: NetConfig,
    bind_override: Option<Ipv4Addr>,
    retry_until: Option<Instant>,
    overlap: bool,
) -> Result<HostStarted, String> {
    let ip = match bind_override {
        Some(ip) => ip,
        None => gf_sys::tailscale_ipv4().ok_or_else(|| TAILSCALE_MISSING.to_string())?,
    };
    let code = if is_tailscale_ipv4(ip) {
        RoomCode::new_random(ip).map_err(|e| e.to_string())?
    } else {
        // Test address: the code is only used for its secret.
        RoomCode {
            ip,
            secret: rand::random::<u32>() & ((1 << 30) - 1),
        }
    };
    cfg.role = Role::Host { bind_ip: ip };
    cfg.secret = code.secret;
    let retry_until = retry_until.max(overlap.then(|| Instant::now() + REBIND_WINDOW));
    loop {
        match gf_net::start(cfg.clone()) {
            Ok(net) => {
                return Ok(HostStarted {
                    net,
                    code: code.encode(),
                    ip,
                })
            }
            // The previous room's port may still be closing.
            Err(e) if retry_until.is_some_and(|t| Instant::now() < t) => {
                log::debug!("couldn't open the room yet ({e}); retrying");
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Host => "Host",
        Command::Join(_) => "Join",
        Command::JoinAddr { .. } => "JoinAddr",
        Command::Leave => "Leave",
        Command::StartSharing => "StartSharing",
        Command::StopSharing => "StopSharing",
        Command::OpenViewer => "OpenViewer",
        Command::StartGameAudio => "StartGameAudio",
        Command::StopGameAudio => "StopGameAudio",
        Command::ApplySettings(_) => "ApplySettings",
        Command::RefreshLists => "RefreshLists",
        Command::DismissError => "DismissError",
        Command::Shutdown => "Shutdown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(codec: VideoCodec, keyframe: bool, len: usize) -> VideoFrame {
        VideoFrame {
            codec,
            keyframe,
            sent_us: 0,
            data: vec![0; len],
        }
    }

    /// While the viewer starts, its video is kept from the latest keyframe
    /// on, so the new window can start at the opening keyframe.
    #[test]
    fn video_waits_for_a_starting_viewer() {
        let h264 = VideoCodec::H264;
        let mut p = PendingFrames::default();
        p.push(h264, frame(h264, false, 10));
        assert!(p.frames.is_empty(), "nothing before the first keyframe");
        p.push(h264, frame(h264, true, 100));
        p.push(h264, frame(h264, false, 10));
        p.push(h264, frame(VideoCodec::Av1, true, 10));
        assert_eq!(p.frames.len(), 2, "other codecs are skipped");
        assert!(p.frames[0].keyframe);
        assert_eq!(p.bytes, 110);

        // A newer keyframe replaces what came before it.
        p.push(h264, frame(h264, true, 50));
        assert_eq!((p.frames.len(), p.bytes), (1, 50));

        // Too much (the viewer never started): wait for the next keyframe.
        for _ in 1..PENDING_FRAMES {
            p.push(h264, frame(h264, false, 1));
        }
        assert_eq!(p.frames.len(), PENDING_FRAMES);
        p.push(h264, frame(h264, false, 1));
        assert!(p.frames.is_empty());
        p.push(h264, frame(h264, false, 1));
        assert!(p.frames.is_empty());
        p.push(h264, frame(h264, true, PENDING_BYTES + 1));
        assert!(p.frames.is_empty(), "the byte limit counts too");
        p.push(h264, frame(h264, true, 1));
        assert_eq!((p.frames.len(), p.bytes), (1, 1));
    }
}
