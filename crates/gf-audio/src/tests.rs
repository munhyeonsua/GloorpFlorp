//! Whole-engine tests. They use the fake devices (no sound hardware needed),
//! except `starts_without_devices`, which opens whatever the machine has.

use super::*;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver};

struct Packet {
    stream: AudioStream,
    seq: u16,
    ts: u32,
    data: Vec<u8>,
}

fn channel_sink() -> (PacketSink, Receiver<Packet>) {
    let (tx, rx) = unbounded();
    let sink: PacketSink = Arc::new(move |stream, seq, ts, data: &[u8]| {
        let _ = tx.send(Packet {
            stream,
            seq,
            ts,
            data: data.to_vec(),
        });
    });
    (sink, rx)
}

fn fake_engine(settings: AudioSettings, key_down: KeyFn) -> (AudioEngine, Receiver<Packet>) {
    let (sink, rx) = channel_sink();
    let engine = AudioEngine::start_with(settings, sink, true, key_down).unwrap();
    (engine, rx)
}

fn collect(rx: &Receiver<Packet>, dur: Duration) -> Vec<Packet> {
    let end = Instant::now() + dur;
    let mut out = Vec::new();
    while let Ok(p) = rx.recv_deadline(end) {
        out.push(p);
    }
    out
}

fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn engine_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<AudioEngine>();
}

/// The fake sources are paced by the clock, so at most one packet per 10 ms
/// since `t0` can exist; a slow machine may deliver fewer.
fn assert_real_time_pace(count: usize, t0: Instant) {
    let elapsed = t0.elapsed();
    let max = elapsed.as_millis() as usize / 10 + 2;
    assert!(
        count <= max && count >= max / 2,
        "{count} packets in {elapsed:?}"
    );
}

#[test]
fn fake_mic_sends_voice_at_100_per_second() {
    let t0 = Instant::now();
    let (engine, rx) = fake_engine(AudioSettings::default(), Box::new(|_| false));
    let packets = collect(&rx, Duration::from_millis(1500));
    assert_real_time_pace(packets.len(), t0);
    for w in packets.windows(2) {
        assert_eq!(w[0].stream, AudioStream::Voice);
        assert_eq!(w[1].seq, w[0].seq.wrapping_add(1));
        assert_eq!(w[1].ts, w[0].ts.wrapping_add(480));
    }
    let st = engine.status();
    assert!(st.transmitting, "{st:?}");
    assert!(st.mic_level > 0.5, "{st:?}");
    assert_eq!(st.error, None);
    assert!(!st.game_capturing);
}

#[test]
fn own_voice_looped_back_is_heard() {
    let (engine, rx) = fake_engine(AudioSettings::default(), Box::new(|_| false));
    let end = Instant::now() + Duration::from_millis(1200);
    while let Ok(p) = rx.recv_deadline(end) {
        engine.receive(p.stream, p.seq, p.ts, p.data);
    }
    let st = engine.status();
    assert!(st.peer_talking, "{st:?}");
    assert!(st.peer_voice_level > 0.3, "{st:?}");
    assert!(st.voice_jitter.received > 50, "{st:?}");
    assert_eq!(st.game_jitter.received, 0);

    engine.reset_peer();
    let st = engine.status();
    assert_eq!(st.voice_jitter.received, 0);
    assert!(!st.peer_talking);
    // Garbage from the network is harmless.
    for seq in 0..50 {
        engine.receive(AudioStream::Game, seq, 0, vec![0xFF; seq as usize]);
        engine.receive(AudioStream::Voice, seq, 0, vec![]);
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(engine.status().error, None);
}

#[test]
fn push_to_talk_mute_and_live_settings() {
    let held = Arc::new(AtomicBool::new(false));
    let h = held.clone();
    let settings = AudioSettings {
        voice_mode: VoiceMode::PushToTalk,
        ptt_key: 0x56,
        ..Default::default()
    };
    let (engine, rx) = fake_engine(
        settings.clone(),
        Box::new(move |vk| vk == 0x56 && h.load(Relaxed)),
    );
    assert!(
        collect(&rx, Duration::from_millis(300)).is_empty(),
        "key not held"
    );
    assert!(!engine.status().transmitting);

    held.store(true, Relaxed);
    assert!(collect(&rx, Duration::from_millis(300)).len() > 15);
    assert!(engine.status().transmitting);

    engine.set_settings(AudioSettings {
        mic_muted: true,
        ..settings.clone()
    });
    collect(&rx, Duration::from_millis(100));
    assert!(collect(&rx, Duration::from_millis(300)).is_empty(), "muted");

    engine.set_settings(AudioSettings {
        voice_mode: VoiceMode::AlwaysOn,
        voice_bitrate_kbps: 24,
        ..settings
    });
    held.store(false, Relaxed);
    collect(&rx, Duration::from_millis(100));
    let packets = collect(&rx, Duration::from_millis(300));
    assert!(packets.len() > 15);
    // 24 kbps is about 30 bytes per 10 ms.
    let avg = packets.iter().map(|p| p.data.len()).sum::<usize>() / packets.len();
    assert!(avg < 60, "average packet {avg} bytes");
}

#[test]
fn fake_game_audio_flows_and_restarts() {
    // Only this env var is shared with gf-sys (fake loopback capture), and
    // every test here wants it on.
    std::env::set_var("GLOORP_FAKE_AUDIO", "1");
    let (engine, rx) = fake_engine(
        AudioSettings {
            mic_muted: true,
            ..Default::default()
        },
        Box::new(|_| false),
    );
    let t0 = Instant::now();
    engine
        .start_game_audio(GameAudioSource::AllExceptThisApp)
        .unwrap();
    let packets = collect(&rx, Duration::from_millis(1200));
    assert!(packets.iter().all(|p| p.stream == AudioStream::Game));
    assert_real_time_pace(packets.len(), t0);
    for w in packets.windows(2) {
        assert_eq!(w[1].seq, w[0].seq.wrapping_add(1));
        assert_eq!(w[1].ts, w[0].ts.wrapping_add(480));
    }
    let st = engine.status();
    assert!(st.game_capturing && st.game_level > 0.3, "{st:?}");

    // Decodes as stereo 440 Hz on the other side.
    for p in &packets {
        engine.receive(p.stream, p.seq, p.ts, p.data.clone());
    }
    assert!(engine.status().game_jitter.received >= packets.len() as u64);

    engine.stop_game_audio();
    let st = engine.status();
    assert!(!st.game_capturing);
    let last_seq = collect(&rx, Duration::from_millis(100))
        .last()
        .map_or(packets.last().unwrap().seq, |p| p.seq);
    assert!(collect(&rx, Duration::from_millis(100)).is_empty());

    engine
        .start_game_audio(GameAudioSource::Process { pid: 1 })
        .unwrap();
    let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(first.seq, last_seq.wrapping_add(1), "numbering continues");
    drop(engine);
}

#[test]
fn public_start_honours_fake_env_var() {
    std::env::set_var("GLOORP_FAKE_AUDIO", "1");
    let (sink, rx) = channel_sink();
    let engine = AudioEngine::start(AudioSettings::default(), sink).unwrap();
    assert!(engine.devices.is_none());
    assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
}

#[test]
fn starts_without_devices() {
    // Uses the machine's real devices if there are any (it only ever plays
    // silence); on CI and in containers there are none.
    let (sink, _rx) = channel_sink();
    let t = Instant::now();
    let engine =
        AudioEngine::start_with(AudioSettings::default(), sink, false, Box::new(|_| false))
            .expect("missing devices must not stop the engine");
    assert!(t.elapsed() < Duration::from_secs(10));
    let st = engine.status();
    println!("status without fake devices: {st:?}");
    if let Some(e) = &st.error {
        assert!(!e.contains("cpal") && !e.contains("ALSA"), "{e}");
    }
    if list_input_devices().is_empty() {
        assert!(
            st.error
                .as_deref()
                .is_some_and(|e| e.contains(devices::NO_MIC)),
            "{st:?}"
        );
    }
    if list_output_devices().is_empty() {
        assert!(
            st.error
                .as_deref()
                .is_some_and(|e| e.contains(devices::NO_SPEAKERS)),
            "{st:?}"
        );
    }
    // Incoming audio is still handled (and played to a null sink if needed).
    for seq in 0..20 {
        engine.receive(AudioStream::Voice, seq, 0, vec![0xF8, 0xFF, 0xFE]);
    }
    wait_for("playout to consume", Duration::from_secs(3), || {
        engine.status().voice_jitter.buffered < 20
    });
    // Switching devices doesn't disturb anything.
    engine.set_settings(AudioSettings {
        input_device: Some("A microphone that doesn't exist".into()),
        ..Default::default()
    });
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    drop(engine);
    assert!(t.elapsed() < Duration::from_secs(6));
}
