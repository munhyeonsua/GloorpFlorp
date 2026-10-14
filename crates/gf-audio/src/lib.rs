//! All audio. Everything internal runs at 48 kHz in 10 ms frames
//! ([`gf_proto::AUDIO_FRAME_SAMPLES`] = 480 samples per channel).
//!
//! ```text
//! OUTGOING
//!  mic (cpal, any format) -> mono 48k -> sonora (echo cancel, noise
//!    suppression, auto gain, high-pass) -> gate (push-to-talk / voice
//!    activity / always on, mute) -> Opus mono VOIP, in-band FEC -> sink(Voice)
//!  game (gf_sys::LoopbackCapture, stereo 48k) -> Opus stereo AUDIO,
//!    in-band FEC -> sink(Game)
//!
//! INCOMING (per stream: Voice, Game)
//!  receive() -> JitterBuffer -> playout thread: Opus decode (FEC / PLC on
//!    loss), mix Voice (mono to both ears) + Game (stereo) with their
//!    volumes, resample to the device rate -> ring buffer -> output
//!    callback -> speakers (cpal)
//!  The mixed output is also fed to the echo canceller as its reference.
//! ```
//!
//! Real-time rules: the cpal callbacks never lock, allocate or block (apart
//! from raising their thread's priority once, on the first call). They only
//! move samples through `rtrb` ring buffers; Opus, sonora and resampling run
//! on ordinary threads.
//!
//! Threads: a playout thread pulls one frame per stream from the jitter
//! buffers whenever the output ring holds less than ~30 ms, decodes, mixes,
//! resamples and pushes (so the sound card's clock sets the pace and the
//! jitter buffer absorbs clock drift); a mic thread runs sonora, the gate and
//! the encoder; a device thread owns the cpal streams; game audio gets an
//! encoder thread while it is on.
//!
//! Devices: `None` means the system default. If the chosen device disappears
//! or fails, report it in [`AudioStatus::error`] and keep the rest working
//! (e.g. no microphone must not stop you hearing your friend). A stream that
//! stops working is reopened at once (and when Windows switches its default
//! device, the default streams follow quietly); missing devices are retried
//! every few seconds. While a chosen device is missing or can't be opened,
//! the default (or the device already playing) is used until it can be.
//! The cpal stream threads raise their own priority (MMCSS) on their first
//! callback, like the engine's other audio threads.
//!
//! Test mode: with the environment variable `GLOORP_FAKE_AUDIO=1`,
//! [`AudioEngine::start`] opens no sound devices. The microphone becomes a
//! 300 Hz tone delivered at real-time pace (sent without echo/noise
//! processing, so with the default voice activation it produces ~100 voice
//! packets per second), and playback goes to a null sink that consumes at
//! real-time pace (levels and jitter statistics keep updating).
//! `gf_sys::LoopbackCapture` honours the same variable, so game audio becomes
//! a 440 Hz tone.

mod codec;
mod devices;
mod fake;
mod game;
mod gate;
mod level;
mod mic;
mod mixer;
mod playout;
mod resample;
mod shared;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gf_proto::jitter::JitterStats;
use gf_proto::{AudioStream, AUDIO_FRAME_SAMPLES};
use gf_sys::GameAudioSource;

use crate::devices::{Devices, Dir};
use crate::game::GameAudio;
use crate::gate::KeyFn;
use crate::mic::MicPipeline;
use crate::playout::PlayoutCore;
use crate::shared::{lock, Shared};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceMode {
    /// Send only while the push-to-talk key is held (works in-game).
    PushToTalk,
    /// Send when speech is detected.
    VoiceActivity,
    AlwaysOn,
}

/// How hard noise suppression works (WebRTC's levels): stronger removes
/// more background noise but can make a voice sound a little muffled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NoiseLevel {
    /// ~6 dB less noise.
    Low,
    /// ~12 dB.
    Moderate,
    /// ~18 dB.
    #[default]
    High,
    /// ~21 dB.
    VeryHigh,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSettings {
    /// Device names from [`list_input_devices`]; `None` = system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub voice_mode: VoiceMode,
    /// Windows virtual-key code (see `gf_sys::keys`).
    pub ptt_key: u32,
    /// Voice activity sensitivity, 0.0 (needs loud speech) to 1.0 (very sensitive).
    pub vad_sensitivity: f32,
    pub mic_muted: bool,
    /// Mute everything we play (friend's voice and game audio).
    pub deafened: bool,
    pub echo_cancel: bool,
    pub noise_suppress: bool,
    pub noise_level: NoiseLevel,
    pub auto_gain: bool,
    /// 0.0..=2.0, 1.0 = unchanged.
    pub voice_volume: f32,
    pub game_volume: f32,
    pub voice_bitrate_kbps: u32,
    pub game_bitrate_kbps: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        AudioSettings {
            input_device: None,
            output_device: None,
            voice_mode: VoiceMode::VoiceActivity,
            ptt_key: 0x06, // Mouse 5
            vad_sensitivity: 0.5,
            mic_muted: false,
            deafened: false,
            echo_cancel: true,
            noise_suppress: true,
            noise_level: NoiseLevel::High,
            auto_gain: true,
            voice_volume: 1.0,
            game_volume: 1.0,
            voice_bitrate_kbps: 48,
            game_bitrate_kbps: 160,
        }
    }
}

/// Where encoded packets go: `(stream, seq, sample_timestamp, opus_packet)`.
///
/// Called on the mic thread (voice) and the game encoder thread (game), never
/// from a device callback. `seq` counts packets sent on that stream; the
/// voice timestamp advances 480 per 10 ms frame also while not sending.
pub type PacketSink = Arc<dyn Fn(AudioStream, u16, u32, &[u8]) + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioStatus {
    /// Mic level after processing, 0..1 (for a meter).
    pub mic_level: f32,
    /// True while voice is actually being sent.
    pub transmitting: bool,
    /// Friend's voice level, 0..1.
    pub peer_voice_level: f32,
    pub peer_talking: bool,
    /// Our outgoing game audio level, 0..1.
    pub game_level: f32,
    /// Game-audio capture is running and hasn't reported a problem. Becomes
    /// false when it stops or fails (e.g. the chosen program exited); the
    /// reason is then in `game_error`.
    pub game_capturing: bool,
    pub voice_jitter: JitterStats,
    pub game_jitter: JitterStats,
    /// Plain-language problem with voice chat to show the user (missing
    /// device, etc.). Game-audio problems are in `game_error` instead.
    pub error: Option<String>,
    /// Plain-language reason the game-audio capture stopped (e.g. the chosen
    /// program closed), while it is still switched on.
    pub game_error: Option<String>,
}

/// Microphone names usable as [`AudioSettings::input_device`]. The system
/// default isn't listed separately (that is `None`). Can take a moment.
pub fn list_input_devices() -> Vec<String> {
    devices::list(Dir::Input)
}

/// Speaker / headphone names usable as [`AudioSettings::output_device`].
pub fn list_output_devices() -> Vec<String> {
    devices::list(Dir::Output)
}

/// True if `GLOORP_FAKE_AUDIO=1` (see the crate docs).
fn fake_audio_requested() -> bool {
    std::env::var("GLOORP_FAKE_AUDIO").is_ok_and(|v| v == "1")
}

/// Waits up to `timeout` for a thread to finish; leaves it behind (logged)
/// rather than hanging the caller.
pub(crate) fn join_with_timeout(thread: JoinHandle<()>, what: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !thread.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if thread.is_finished() {
        let _ = thread.join();
    } else {
        log::warn!("{what} thread didn't stop within {timeout:?}; leaving it behind");
    }
}

/// Spawns one of the engine's threads. If it ever panics, the user gets a
/// message instead of silently losing audio.
fn spawn_worker(
    name: &str,
    shared: &Arc<Shared>,
    body: impl FnOnce() + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    let shared = shared.clone();
    let thread_name = name.to_string();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            if catch_unwind(AssertUnwindSafe(body)).is_err() {
                log::error!("audio thread {thread_name} crashed");
                *lock(&shared.internal_error) = Some(
                    "Voice chat stopped working because of an internal error. Restart GloorpFlorp."
                        .to_string(),
                );
            }
        })
}

/// Running audio. Stops everything when dropped.
pub struct AudioEngine {
    shared: Arc<Shared>,
    sink: PacketSink,
    settings: Mutex<AudioSettings>,
    /// `None` in fake mode.
    devices: Option<Devices>,
    game: Mutex<Option<GameAudio>>,
    threads: Vec<JoinHandle<()>>,
}

impl AudioEngine {
    /// Starts playback and the microphone. Missing devices are not an error
    /// (see [`AudioStatus::error`]). Waits up to a few seconds while the
    /// devices open.
    pub fn start(settings: AudioSettings, sink: PacketSink) -> anyhow::Result<AudioEngine> {
        AudioEngine::start_with(
            settings,
            sink,
            fake_audio_requested(),
            Box::new(gf_sys::keys::is_down),
        )
    }

    fn start_with(
        settings: AudioSettings,
        sink: PacketSink,
        fake: bool,
        key_down: KeyFn,
    ) -> anyhow::Result<AudioEngine> {
        log::info!(
            "audio: starting{}",
            if fake {
                " with fake devices (GLOORP_FAKE_AUDIO=1)"
            } else {
                ""
            }
        );
        let shared = Arc::new(Shared::new(&settings));
        // Echo-canceller reference: the playout thread's mix, mono 48 kHz.
        let (reference_tx, reference_rx) = rtrb::RingBuffer::new(AUDIO_FRAME_SAMPLES * 50);
        let core = PlayoutCore::new(shared.clone())?;
        let mic = MicPipeline::new(shared.clone(), sink.clone(), reference_rx, key_down, fake)?;
        let (input_tx, input_rx) = crossbeam_channel::unbounded();
        let (output_tx, output_rx) = crossbeam_channel::unbounded();

        // Built step by step so that an error part-way stops what's running.
        let mut engine = AudioEngine {
            shared: shared.clone(),
            sink,
            settings: Mutex::new(settings.clone()),
            devices: None,
            game: Mutex::new(None),
            threads: Vec::new(),
        };
        let s = shared.clone();
        engine
            .threads
            .push(spawn_worker("gf-audio-playout", &shared, move || {
                playout::run(s, core, reference_tx, output_rx)
            })?);
        engine
            .threads
            .push(spawn_worker("gf-audio-mic", &shared, move || {
                mic::run(mic, input_rx)
            })?);
        if fake {
            engine
                .threads
                .push(fake::start_mic(shared.clone(), &input_tx)?);
        } else {
            engine.devices = Some(Devices::start(
                shared,
                settings.input_device,
                settings.output_device,
                input_tx,
                output_tx,
            )?);
        }
        Ok(engine)
    }

    /// Apply new settings live. Device changes restart only the affected stream.
    pub fn set_settings(&self, settings: AudioSettings) {
        self.shared.live.apply(&settings);
        let mut current = lock(&self.settings);
        if let Some(devices) = &self.devices {
            if current.input_device != settings.input_device {
                devices.set_input(settings.input_device.clone());
            }
            if current.output_device != settings.output_device {
                devices.set_output(settings.output_device.clone());
            }
        }
        *current = settings;
    }

    /// Packet from the network, arriving now.
    pub fn receive(&self, stream: AudioStream, seq: u16, ts: u32, payload: Vec<u8>) {
        self.receive_at(stream, seq, ts, payload, Instant::now());
    }

    /// Packet from the network that arrived at `arrived`. The jitter buffer
    /// sizes itself from arrival times, so pass the time the network thread
    /// received it if that is known (it may be handed over a bit later).
    pub fn receive_at(
        &self,
        stream: AudioStream,
        seq: u16,
        ts: u32,
        payload: Vec<u8>,
        arrived: Instant,
    ) {
        // Ordering and loss come from `seq`; the timestamp isn't needed.
        let _ = ts;
        self.shared.incoming.push(stream, seq, payload, arrived);
    }

    /// Clear all incoming state (call when the friend disconnects).
    pub fn reset_peer(&self) {
        self.shared.incoming.reset();
        let m = &self.shared.meters;
        m.peer_voice_level.store(0.0);
        m.peer_talking.store(false, Relaxed);
    }

    /// Starts (or restarts with a new source) sending game audio. Can block
    /// for several seconds while Windows starts the capture, so call it off
    /// the UI thread.
    pub fn start_game_audio(&self, source: GameAudioSource) -> anyhow::Result<()> {
        self.stop_game_audio();
        let game = GameAudio::start(source, self.shared.clone(), self.sink.clone())?;
        let replaced = lock(&self.game).replace(game);
        drop(replaced);
        Ok(())
    }

    /// Stops sending game audio. Can block for up to ~2 s.
    pub fn stop_game_audio(&self) {
        let game = lock(&self.game).take();
        drop(game);
        self.shared.meters.game_level.store(0.0);
    }

    pub fn status(&self) -> AudioStatus {
        let shared = &self.shared;
        let m = &shared.meters;
        let (voice_jitter, game_jitter) = shared.incoming.stats();
        let game_error = lock(&self.game).as_ref().map(|g| g.error());
        let errors: Vec<String> = [
            lock(&shared.internal_error).clone(),
            lock(&shared.input_error).clone(),
            lock(&shared.output_error).clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        AudioStatus {
            mic_level: m.mic_level.load(),
            transmitting: m.transmitting.load(Relaxed),
            peer_voice_level: m.peer_voice_level.load(),
            peer_talking: m.peer_talking.load(Relaxed),
            game_level: m.game_level.load(),
            game_capturing: matches!(game_error, Some(None)),
            voice_jitter,
            game_jitter,
            error: (!errors.is_empty()).then(|| errors.join(" ")),
            game_error: game_error.flatten(),
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        let game = lock(&self.game).take();
        drop(game);
        drop(self.devices.take());
        for t in self.threads.drain(..) {
            join_with_timeout(t, "audio", Duration::from_secs(3));
        }
        log::info!("audio: stopped");
    }
}

#[cfg(test)]
mod tests;
