//! Two engines (host + joiner) talking over 127.0.0.1 with fake audio
//! (`GLOORP_FAKE_AUDIO=1`) and FFmpeg's test pattern instead of the screen
//! (`GLOORP_FAKE_VIDEO=1`, always the case off Windows). The video part is
//! skipped when FFmpeg isn't installed.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Once;
use std::time::{Duration, Instant};

use gf_proto::room::RoomCode;
use gloorpflorp::engine::{Command, Engine, EngineOptions, Phase, ShareState, Snapshot};
use gloorpflorp::settings::{Settings, VoiceModeChoice};

fn init() {
    static ONCE: Once = Once::new();
    // Before any engine thread exists (setting variables isn't thread-safe).
    ONCE.call_once(|| {
        std::env::set_var("GLOORP_FAKE_AUDIO", "1");
        std::env::set_var("GLOORP_FAKE_VIDEO", "1");
        // Shown with the test output when a test fails.
        if log::set_logger(&PrintLogger).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    });
}

/// `println!` is captured per test, unlike writing to stdout directly.
struct PrintLogger;

impl log::Log for PrintLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        let thread = std::thread::current();
        println!(
            "{:5} [{}] {}",
            r.level(),
            thread.name().unwrap_or("?"),
            r.args()
        );
    }
    fn flush(&self) {}
}

fn wait_for(
    engine: &Engine,
    what: &str,
    timeout: Duration,
    ok: impl Fn(&Snapshot) -> bool,
) -> Snapshot {
    let deadline = Instant::now() + timeout;
    loop {
        let s = engine.snapshot();
        if ok(&s) {
            return s;
        }
        assert!(
            engine.is_running(),
            "the engine stopped while waiting for {what}"
        );
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}; last state:\n{s:#?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn settings(name: &str) -> Settings {
    let mut s = Settings {
        name: name.into(),
        fps: 30,
        // Small picture: software encoders on a busy CI machine.
        max_height: Some(360),
        ..Settings::default()
    };
    s.audio.voice_mode = VoiceModeChoice::AlwaysOn;
    s
}

fn host_settings() -> Settings {
    settings("Host PC")
}

fn engine(name: &str) -> Engine {
    Engine::start(
        settings(name),
        EngineOptions {
            bind_ip: Some(Ipv4Addr::LOCALHOST),
            port: Some(0),
            ..EngineOptions::default()
        },
    )
    .expect("engine starts")
}

const T: Duration = Duration::from_secs(20);

fn connected_to(name: &'static str) -> impl Fn(&Snapshot) -> bool {
    move |s| s.phase == Phase::Connected { peer: name.into() }
}

/// Joins `host`'s room by address (the room code only carries the secret here).
fn join(host: &Engine, joiner: &Engine) {
    let s = wait_for(host, "the room to open", T, |s| {
        matches!(s.phase, Phase::Hosting { waiting: true, .. }) && s.listen_addr.is_some()
    });
    let port = s.listen_addr.unwrap().port();
    let secret = RoomCode::parse(s.room_code.as_deref().expect("room code"))
        .expect("valid room code")
        .secret;
    joiner.send(Command::JoinAddr {
        addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        secret,
    });
    wait_for(joiner, "the joiner to connect", T, connected_to("Host PC"));
    wait_for(
        host,
        "the host to see the joiner",
        T,
        connected_to("Joiner PC"),
    );
}

#[test]
fn two_engines_end_to_end() {
    init();
    let video = match gf_video::Tools::locate() {
        Ok(_) => true,
        Err(_) => {
            println!("note: FFmpeg isn't available, so the video part of this test is skipped");
            false
        }
    };
    let mut host = engine("Host PC");
    let mut joiner = engine("Joiner PC");
    host.send(Command::Host);
    join(&host, &joiner);

    // Voice flows both ways (always-on mode with the fake microphone) and
    // is decoded on the other side.
    let voice = |s: &Snapshot| s.net.audio_recv_kbps > 0.0 && s.audio.peer_voice_level > 0.0;
    wait_for(&host, "voice from the joiner", T, voice);
    wait_for(&joiner, "voice from the host", T, voice);

    // Game audio: the friend's UI shows it.
    host.send(Command::StartGameAudio);
    wait_for(&host, "game audio to start", T, |s| s.my_game_audio);
    wait_for(&joiner, "the host's game audio", T, |s| {
        s.peer_game_audio && s.audio.game_jitter.received > 0
    });

    if video {
        host.send(Command::StartSharing);
        let share = wait_for(&joiner, "the host's share", Duration::from_secs(60), |s| {
            s.peer_share.is_some()
        })
        .peer_share
        .unwrap();
        assert_eq!((share.width, share.height, share.fps), (640, 360, 30));
        // The viewer opens by itself (FFmpeg null sink here) and asks for frames.
        let s = wait_for(&joiner, "video frames", Duration::from_secs(60), |s| {
            s.frames_received >= 15
        });
        assert!(s.viewer_open, "viewer should be open: {s:#?}");
        let s = wait_for(&host, "the host to send frames", T, |s| {
            s.peer_watching && s.frames_sent > 0
        });
        assert!(
            matches!(s.my_share, ShareState::Live(i) if i == share),
            "{s:#?}"
        );
        // A new frame rate while sharing restarts the encoder; the friend's
        // viewer stays open (same codec and size) and frames keep coming.
        let mut changed = host_settings();
        changed.fps = 60;
        host.send(Command::ApplySettings(Box::new(changed)));
        let s = wait_for(
            &joiner,
            "the new frame rate",
            Duration::from_secs(60),
            |s| s.peer_share.is_some_and(|p| p.fps == 60),
        );
        let before = s.frames_received;
        let s = wait_for(
            &joiner,
            "frames after the restart",
            Duration::from_secs(60),
            |s| s.frames_received >= before + 15,
        );
        assert!(s.viewer_open, "viewer should still be open: {s:#?}");

        host.send(Command::StopSharing);
        wait_for(&joiner, "the share to end", T, |s| {
            s.peer_share.is_none() && !s.viewer_open
        });
        wait_for(&host, "sharing to stop", T, |s| {
            s.my_share == ShareState::Off
        });
    }

    host.send(Command::StopGameAudio);
    wait_for(&joiner, "the host's game audio to stop", T, |s| {
        !s.peer_game_audio
    });

    // The joiner leaves: the host sees it and keeps the room open.
    joiner.send(Command::Leave);
    wait_for(&joiner, "the joiner to be idle", T, |s| {
        s.phase == Phase::Idle
    });
    let s = wait_for(&host, "the host to see the friend leave", T, |s| {
        matches!(s.phase, Phase::Hosting { waiting: true, .. }) && s.peer_name.is_none()
    });
    assert!(
        s.info.as_deref().is_some_and(|i| i.contains("left")),
        "{s:#?}"
    );

    // Rejoin, then the host leaves: the joiner sees the room close.
    join(&host, &joiner);
    host.send(Command::Leave);
    let s = wait_for(&joiner, "the joiner to see the host leave", T, |s| {
        s.phase == Phase::Idle
    });
    assert!(
        s.info
            .as_deref()
            .is_some_and(|i| i.contains("closed the room")),
        "{s:#?}"
    );
    assert_eq!(s.peer_name, None);

    let t = Instant::now();
    assert!(host.shutdown(Duration::from_secs(5)));
    assert!(joiner.shutdown(Duration::from_secs(5)));
    assert!(
        t.elapsed() < Duration::from_secs(6),
        "shutdown took {:?}",
        t.elapsed()
    );
}

#[test]
fn join_problems_are_shown_in_plain_language() {
    init();
    let mut e = engine("Solo PC");
    e.send(Command::Join("K7QM-2XPA".into()));
    // (Without FFmpeg there is already an error about that.)
    let has_error = |s: &Snapshot, text: &str| s.error.as_deref().is_some_and(|e| e.contains(text));
    let s = wait_for(&e, "a room code error", T, |s| has_error(s, "room code"));
    assert_eq!(s.phase, Phase::Idle);

    e.send(Command::DismissError);
    wait_for(&e, "the error to go away", T, |s| s.error.is_none());

    // Nobody listens on this port: the join fails with an explanation.
    let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap();
    e.send(Command::JoinAddr {
        addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        secret: 1,
    });
    wait_for(&e, "a connection error", T, |s| {
        has_error(s, "friend") && s.phase == Phase::Idle
    });
    assert!(e.shutdown(Duration::from_secs(5)));
    assert!(!e.is_running());
}

#[test]
fn settings_are_saved() {
    init();
    let dir = std::env::temp_dir().join(format!("gloorpflorp-e2e-{}", std::process::id()));
    let path = dir.join("settings.json");
    let _ = std::fs::remove_dir_all(&dir);
    let mut e = Engine::start(
        settings("Saver PC"),
        EngineOptions {
            settings_path: Some(path.clone()),
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let mut s = settings("Saver PC");
    s.fps = 144;
    s.audio.voice_volume = 1.5;
    e.send(Command::ApplySettings(Box::new(s.clone())));
    // Saved a moment after the change...
    let deadline = Instant::now() + T;
    while Settings::load(&path).0 != s {
        assert!(Instant::now() < deadline, "settings weren't saved");
        std::thread::sleep(Duration::from_millis(100));
    }
    // ...and on shutdown.
    s.fps = 120;
    e.send(Command::ApplySettings(Box::new(s.clone())));
    assert!(e.shutdown(Duration::from_secs(5)));
    assert_eq!(Settings::load(&path), (s, None));
    let _ = std::fs::remove_dir_all(&dir);
}
