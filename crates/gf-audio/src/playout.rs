//! Incoming audio: jitter buffers, decoding, mixing, and the playout thread
//! that keeps the output device's ring buffer topped up.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use gf_proto::jitter::{JitterBuffer, JitterConfig, JitterStats, Pop};
use gf_proto::{AudioStream, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE};

use crate::codec::OpusDecoder;
use crate::level::{rms_dbfs, LevelMeter};
use crate::mixer::{self, MixParams};
use crate::resample::Resampler;
use crate::shared::{lock, Shared};

/// How much audio the playout thread keeps queued for the sound card.
const TARGET_FILL_MS: usize = 30;
/// Decoded friend's voice above this level counts as talking...
const TALKING_DBFS: f32 = -50.0;
/// ...and keeps counting for this many frames afterwards.
const TALKING_HOLD_FRAMES: u32 = 30;
const FRAME: Duration = Duration::from_millis(10);

/// Packets from the network, waiting to be played. Shared between
/// [`crate::AudioEngine::receive`] (network thread) and the playout thread;
/// each side holds a lock only for one push or pop.
pub(crate) struct Incoming {
    voice: Mutex<JitterBuffer>,
    game: Mutex<JitterBuffer>,
    /// Bumped by `reset` so the playout thread also resets its decoders.
    generation: AtomicU64,
}

impl Incoming {
    pub fn new() -> Incoming {
        Incoming {
            voice: Mutex::new(JitterBuffer::new(JitterConfig::default())),
            game: Mutex::new(JitterBuffer::new(JitterConfig::default())),
            generation: AtomicU64::new(0),
        }
    }

    fn buffer(&self, stream: AudioStream) -> &Mutex<JitterBuffer> {
        match stream {
            AudioStream::Voice => &self.voice,
            AudioStream::Game => &self.game,
        }
    }

    pub fn push(&self, stream: AudioStream, seq: u16, payload: Vec<u8>, now: Instant) {
        lock(self.buffer(stream)).push(seq, payload, now);
    }

    pub fn pop(&self, stream: AudioStream) -> Pop {
        lock(self.buffer(stream)).pop()
    }

    pub fn reset(&self) {
        lock(&self.voice).reset();
        lock(&self.game).reset();
        self.generation.fetch_add(1, Relaxed);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Relaxed)
    }

    /// (voice, game)
    pub fn stats(&self) -> (JitterStats, JitterStats) {
        (lock(&self.voice).stats(), lock(&self.game).stats())
    }
}

/// Pulls one frame per stream from the jitter buffers, decodes and mixes.
/// Everything except the output device, so it can be tested directly.
pub(crate) struct PlayoutCore {
    shared: Arc<Shared>,
    generation: u64,
    voice_dec: OpusDecoder,
    game_dec: OpusDecoder,
    voice: Vec<f32>,
    game: Vec<f32>,
    meter: LevelMeter,
    talking_hold: u32,
}

impl PlayoutCore {
    pub fn new(shared: Arc<Shared>) -> anyhow::Result<PlayoutCore> {
        Ok(PlayoutCore {
            generation: shared.incoming.generation(),
            shared,
            voice_dec: OpusDecoder::new(1)?,
            game_dec: OpusDecoder::new(2)?,
            voice: vec![0.0; AUDIO_FRAME_SAMPLES],
            game: vec![0.0; AUDIO_FRAME_SAMPLES * 2],
            meter: LevelMeter::default(),
            talking_hold: 0,
        })
    }

    /// The next 10 ms of the mix into `out` (interleaved stereo, 48 kHz,
    /// 960 samples). Also updates the friend's voice meter.
    pub fn render(&mut self, mix: MixParams, out: &mut [f32]) {
        let generation = self.shared.incoming.generation();
        if generation != self.generation {
            self.generation = generation;
            self.voice_dec.reset();
            self.game_dec.reset();
            self.meter.reset();
            self.talking_hold = 0;
        }
        let incoming = &self.shared.incoming;
        pull(
            incoming,
            AudioStream::Voice,
            &mut self.voice_dec,
            &mut self.voice,
        );
        pull(
            incoming,
            AudioStream::Game,
            &mut self.game_dec,
            &mut self.game,
        );

        let level = rms_dbfs(&self.voice);
        if level > TALKING_DBFS {
            self.talking_hold = TALKING_HOLD_FRAMES;
        } else {
            self.talking_hold = self.talking_hold.saturating_sub(1);
        }
        let meters = &self.shared.meters;
        meters.peer_voice_level.store(self.meter.update(level));
        meters.peer_talking.store(self.talking_hold > 0, Relaxed);

        mixer::mix(&self.voice, &self.game, mix, out);
    }
}

fn pull(incoming: &Incoming, stream: AudioStream, dec: &mut OpusDecoder, out: &mut [f32]) {
    match incoming.pop(stream) {
        Pop::Frame(packet) => {
            dec.decode(&packet, out);
        }
        Pop::Lost { next: Some(next) } => dec.decode_fec(&next, out),
        Pop::Lost { next: None } => dec.conceal(out),
        Pop::Empty => out.fill(0.0),
    }
}

/// An opened output device, handed from the device thread to the playout thread.
pub(crate) struct OutputFeed {
    /// Interleaved samples at the device's rate and channel count; the
    /// device callback pops them.
    pub producer: rtrb::Producer<f32>,
    pub rate: u32,
    pub channels: usize,
    /// Largest number of samples the device asked for in one callback.
    pub max_request: Arc<AtomicUsize>,
    /// Callbacks that found the ring short.
    pub underruns: Arc<AtomicU64>,
}

struct DeviceOut {
    feed: OutputFeed,
    resampler: Resampler,
    spread: Vec<f32>,
    reported_underruns: u64,
    last_report: Instant,
}

impl DeviceOut {
    fn new(feed: OutputFeed) -> Option<DeviceOut> {
        match Resampler::new(AUDIO_SAMPLE_RATE, feed.rate, 2) {
            Ok(resampler) => Some(DeviceOut {
                spread: Vec::with_capacity(feed.rate as usize / 50 * feed.channels),
                feed,
                resampler,
                reported_underruns: 0,
                last_report: Instant::now(),
            }),
            Err(e) => {
                log::warn!("can't play to this output device: {e}");
                None
            }
        }
    }

    fn fill(&self) -> usize {
        let p = &self.feed.producer;
        p.buffer().capacity() - p.slots()
    }

    /// 30 ms, or more if the device takes bigger bites.
    fn target(&self) -> usize {
        let base = self.feed.rate as usize * TARGET_FILL_MS / 1000 * self.feed.channels;
        let request = self.feed.max_request.load(Relaxed);
        base.max(request + base / 3)
            .min(self.feed.producer.buffer().capacity() / 2)
    }

    fn push(&mut self, frame: &[f32]) {
        let DeviceOut {
            feed,
            resampler,
            spread,
            ..
        } = self;
        resampler.process(frame, |chunk| {
            mixer::spread(chunk, feed.channels, spread);
            // Whole frames only, so channels never get out of step.
            let room = feed.producer.slots() / feed.channels * feed.channels;
            let n = spread.len().min(room);
            let _ = feed.producer.push_entire_slice(&spread[..n]);
        });
    }

    fn report(&mut self) {
        if self.last_report.elapsed() < Duration::from_secs(10) {
            return;
        }
        self.last_report = Instant::now();
        let total = self.feed.underruns.load(Relaxed);
        if total > self.reported_underruns {
            log::debug!(
                "playback: {} underruns in the last 10 s",
                total - self.reported_underruns
            );
            self.reported_underruns = total;
        }
    }
}

/// The playout thread. With an output device it renders whenever the
/// device's ring drops below the target, so the device clock sets the pace.
/// Without one (no speakers, or fake mode) it renders on its own 10 ms clock
/// and throws the audio away, so meters and jitter statistics keep working.
pub(crate) fn run(
    shared: Arc<Shared>,
    mut core: PlayoutCore,
    mut reference: rtrb::Producer<f32>,
    feeds: Receiver<Option<OutputFeed>>,
) {
    let _priority = gf_sys::raise_audio_thread_priority();
    let mut device: Option<DeviceOut> = None;
    let mut frame = vec![0f32; AUDIO_FRAME_SAMPLES * 2];
    let mut mono = vec![0f32; AUDIO_FRAME_SAMPLES];
    let mut next_tick = Instant::now();
    while !shared.stopping() {
        for feed in feeds.try_iter() {
            device = feed.and_then(DeviceOut::new);
            next_tick = Instant::now();
        }
        if device
            .as_ref()
            .is_some_and(|d| d.feed.producer.is_abandoned())
        {
            // The device thread dropped the stream; a new feed will follow.
            device = None;
            next_tick = Instant::now();
        }
        match device.as_mut() {
            Some(dev) => {
                let mut rendered = 0;
                while dev.fill() < dev.target() && rendered < 10 {
                    core.render(shared.live.mix(), &mut frame);
                    mixer::downmix(&frame, &mut mono);
                    if reference.slots() >= mono.len() {
                        let _ = reference.push_entire_slice(&mono);
                    }
                    dev.push(&frame);
                    rendered += 1;
                }
                dev.report();
                std::thread::sleep(Duration::from_millis(2));
            }
            None => {
                let now = Instant::now();
                if now.saturating_duration_since(next_tick) > Duration::from_millis(200) {
                    // Way behind (suspended?): don't burst to catch up.
                    next_tick = now;
                }
                while next_tick <= now {
                    core.render(shared.live.mix(), &mut frame);
                    next_tick += FRAME;
                }
                std::thread::sleep(
                    next_tick
                        .saturating_duration_since(Instant::now())
                        .min(FRAME),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::tests::{rms, sine, zero_crossing_hz};
    use crate::codec::{OpusEncoder, MAX_PACKET};
    use crate::AudioSettings;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn setup() -> (Arc<Shared>, PlayoutCore) {
        let shared = Arc::new(Shared::new(&AudioSettings::default()));
        let core = PlayoutCore::new(shared.clone()).unwrap();
        (shared, core)
    }

    fn voice_packets(hz: f64, frames: usize) -> Vec<Vec<u8>> {
        let mut enc = OpusEncoder::voice(48).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        (0..frames)
            .map(|f| {
                let pcm = sine(hz, 0.3, f * AUDIO_FRAME_SAMPLES, AUDIO_FRAME_SAMPLES, 1);
                let n = enc.encode(&pcm, &mut buf).unwrap();
                buf[..n].to_vec()
            })
            .collect()
    }

    #[test]
    fn tone_survives_reordering_and_loss() {
        let (shared, mut core) = setup();
        let packets = voice_packets(440.0, 300);
        let mut rng = StdRng::seed_from_u64(7);
        // Network: 5% loss, and neighbours swap places now and then.
        let mut order: Vec<usize> = (0..packets.len())
            .filter(|_| !rng.random_bool(0.05))
            .collect();
        for i in 1..order.len() {
            if rng.random_bool(0.2) {
                order.swap(i - 1, i);
            }
        }
        let t0 = Instant::now();
        let mut out = vec![0f32; AUDIO_FRAME_SAMPLES * 2];
        let mut played = Vec::new();
        let mut next = 0;
        let mut talking = 0;
        for tick in 0..340u64 {
            // Packets arrive on time, ~1 per 10 ms, with the shuffled order.
            while next < order.len() && next as u64 <= tick {
                let seq = order[next];
                let now = t0 + Duration::from_millis(tick * 10);
                shared
                    .incoming
                    .push(AudioStream::Voice, seq as u16, packets[seq].clone(), now);
                next += 1;
            }
            core.render(MixParams::default(), &mut out);
            played.extend_from_slice(&out);
            if shared.meters.peer_talking.load(Relaxed) {
                talking += 1;
            }
        }
        let stats = shared.incoming.stats().0;
        assert!(stats.lost > 5, "{stats:?}");
        // Mono voice is on both channels.
        assert!(played.chunks(2).all(|f| f[0] == f[1]));
        let start = played.iter().position(|s| s.abs() > 0.01).unwrap();
        let tail =
            &played[start + 20 * AUDIO_FRAME_SAMPLES * 2..start + 260 * AUDIO_FRAME_SAMPLES * 2];
        let hz = zero_crossing_hz(tail, 2, AUDIO_SAMPLE_RATE);
        assert!((hz - 440.0).abs() < 10.0, "{hz} Hz");
        assert!(rms(tail) > 0.1, "rms {}", rms(tail));
        // Talking from shortly after the start until shortly after the end.
        assert!(
            (290..=335).contains(&talking),
            "talking for {talking} frames"
        );
        assert!(!shared.meters.peer_talking.load(Relaxed));
    }

    #[test]
    fn empty_buffers_play_silence() {
        let (shared, mut core) = setup();
        let mut out = vec![1f32; AUDIO_FRAME_SAMPLES * 2];
        for _ in 0..5 {
            core.render(MixParams::default(), &mut out);
            assert!(out.iter().all(|&s| s == 0.0));
        }
        assert!(!shared.meters.peer_talking.load(Relaxed));
        assert_eq!(shared.meters.peer_voice_level.load(), 0.0);
    }

    #[test]
    fn game_and_voice_mix_and_reset() {
        let (shared, mut core) = setup();
        let mut enc = OpusEncoder::game(160).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let voice = voice_packets(300.0, 150);
        let t0 = Instant::now();
        let mut out = vec![0f32; AUDIO_FRAME_SAMPLES * 2];
        let (mut game_only, mut voice_only) = (Vec::new(), Vec::new());
        for (f, voice_packet) in voice.into_iter().enumerate() {
            let pcm = sine(1000.0, 0.2, f * AUDIO_FRAME_SAMPLES, AUDIO_FRAME_SAMPLES, 2);
            let n = enc.encode(&pcm, &mut buf).unwrap();
            let now = t0 + Duration::from_millis(10 * f as u64);
            shared
                .incoming
                .push(AudioStream::Game, f as u16, buf[..n].to_vec(), now);
            shared
                .incoming
                .push(AudioStream::Voice, f as u16, voice_packet, now);
            // First half: friend's voice muted by volume; second half: game.
            let mix = if f < 75 {
                MixParams {
                    voice_volume: 0.0,
                    ..Default::default()
                }
            } else {
                MixParams {
                    game_volume: 0.0,
                    ..Default::default()
                }
            };
            core.render(mix, &mut out);
            if (20..75).contains(&f) {
                game_only.extend_from_slice(&out);
            } else if f >= 95 {
                voice_only.extend_from_slice(&out);
            }
        }
        let hz = zero_crossing_hz(&game_only, 2, AUDIO_SAMPLE_RATE);
        assert!((hz - 1000.0).abs() < 10.0, "game {hz} Hz");
        let hz = zero_crossing_hz(&voice_only, 2, AUDIO_SAMPLE_RATE);
        assert!((hz - 300.0).abs() < 10.0, "voice {hz} Hz");
        assert!(rms(&game_only) > 0.1 && rms(&voice_only) > 0.1);

        let deaf = MixParams {
            deafened: true,
            ..Default::default()
        };
        core.render(deaf, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert!(
            shared.meters.peer_talking.load(Relaxed),
            "meters keep working when deafened"
        );

        shared.incoming.reset();
        let (v, g) = shared.incoming.stats();
        assert_eq!((v.received, g.received, v.buffered), (0, 0, 0));
        core.render(MixParams::default(), &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn garbage_packets_are_concealed() {
        let (shared, mut core) = setup();
        let now = Instant::now();
        for seq in 0..20u16 {
            shared.incoming.push(
                AudioStream::Voice,
                seq,
                vec![0xFF; (seq as usize * 37) % 300],
                now,
            );
            shared
                .incoming
                .push(AudioStream::Game, seq, vec![seq as u8; 3], now);
        }
        let mut out = vec![0f32; AUDIO_FRAME_SAMPLES * 2];
        for _ in 0..30 {
            core.render(MixParams::default(), &mut out);
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
    }

    #[test]
    fn device_out_keeps_whole_frames() {
        let (producer, mut consumer) = rtrb::RingBuffer::new(44_100 * 6 / 2);
        let feed = OutputFeed {
            producer,
            rate: 44_100,
            channels: 6,
            max_request: Arc::new(AtomicUsize::new(0)),
            underruns: Arc::new(AtomicU64::new(0)),
        };
        let mut dev = DeviceOut::new(feed).unwrap();
        assert_eq!(dev.target(), 1323 * 6);
        let frame = sine(440.0, 0.5, 0, AUDIO_FRAME_SAMPLES, 2);
        while dev.fill() < dev.target() {
            dev.push(&frame);
        }
        assert_eq!(dev.fill() % 6, 0);
        let mut got = vec![0f32; dev.fill()];
        consumer.pop_entire_slice(&mut got).unwrap();
        assert!(got.chunks(6).all(|f| f[2..].iter().all(|&s| s == 0.0)));
        // A device with big callbacks gets a deeper queue.
        dev.feed.max_request.store(4410 * 6, Relaxed);
        assert_eq!(dev.target(), (4410 + 441) * 6);
    }
}
