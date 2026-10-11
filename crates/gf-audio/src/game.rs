//! Outgoing game audio: process-loopback capture → 10 ms stereo frames → Opus.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gf_proto::{AudioStream, AUDIO_FRAME_SAMPLES};
use gf_sys::{GameAudioSource, LoopbackCapture};

use crate::codec::{OpusEncoder, MAX_PACKET};
use crate::level::{rms_dbfs, LevelMeter};
use crate::shared::Shared;
use crate::PacketSink;

const FRAME_LEN: usize = AUDIO_FRAME_SAMPLES * 2;
/// Half a second of stereo between the capture callback and the encoder.
const RING_SAMPLES: usize = 48_000;
/// The capture sends nothing while the program is silent; a part-filled
/// frame left waiting longer than this is padded with silence and sent.
const FLUSH_AFTER: Duration = Duration::from_millis(30);
/// After this long without samples the level meter drops to zero.
const IDLE_AFTER: Duration = Duration::from_millis(150);

/// A running game-audio capture and its encoder thread. Stops when dropped
/// (which can take up to ~2 s while Windows releases the capture).
pub(crate) struct GameAudio {
    capture: Option<LoopbackCapture>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl GameAudio {
    /// Can block for up to ~10 s while Windows starts the capture.
    pub fn start(
        source: GameAudioSource,
        shared: Arc<Shared>,
        sink: PacketSink,
    ) -> anyhow::Result<GameAudio> {
        let encoder = GameEncoder::new(shared.clone(), sink)?;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(RING_SAMPLES);
        // Runs on the capture thread: hand the samples over, nothing else.
        let on_samples = Box::new(move |samples: &[f32]| {
            let n = samples.len().min(producer.slots()) & !1;
            let _ = producer.push_entire_slice(&samples[..n]);
        });
        let capture = LoopbackCapture::start(source, on_samples)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            crate::spawn_worker("gf-game-encoder", &shared, move || {
                encoder.run(consumer, &stop)
            })?
        };
        log::info!("game audio: capturing {source:?}");
        Ok(GameAudio {
            capture: Some(capture),
            stop,
            thread: Some(thread),
        })
    }

    /// Set when the capture died (e.g. the chosen program closed).
    pub fn error(&self) -> Option<String> {
        self.capture.as_ref().and_then(|c| c.error())
    }
}

impl Drop for GameAudio {
    fn drop(&mut self) {
        drop(self.capture.take());
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        log::info!("game audio: stopped");
    }
}

/// Cuts captured audio into 10 ms frames and sends them.
pub(crate) struct GameEncoder {
    shared: Arc<Shared>,
    sink: PacketSink,
    enc: OpusEncoder,
    seq: u16,
    ts: u32,
    packet: Vec<u8>,
    meter: LevelMeter,
}

impl GameEncoder {
    pub fn new(shared: Arc<Shared>, sink: PacketSink) -> anyhow::Result<GameEncoder> {
        Ok(GameEncoder {
            enc: OpusEncoder::game(shared.live.game_bitrate_kbps())?,
            // Carry on from the previous capture's numbering.
            seq: shared.game_seq.load(Relaxed) as u16,
            ts: shared.game_ts.load(Relaxed),
            shared,
            sink,
            packet: vec![0; MAX_PACKET],
            meter: LevelMeter::default(),
        })
    }

    /// Encodes and sends one interleaved stereo frame (960 samples).
    pub fn send(&mut self, frame: &[f32]) {
        let level = rms_dbfs(frame);
        self.shared
            .meters
            .game_level
            .store(self.meter.update(level));
        self.enc.set_bitrate(self.shared.live.game_bitrate_kbps());
        match self.enc.encode(frame, &mut self.packet) {
            Ok(n) => {
                (self.sink)(AudioStream::Game, self.seq, self.ts, &self.packet[..n]);
                self.seq = self.seq.wrapping_add(1);
                self.ts = self.ts.wrapping_add(AUDIO_FRAME_SAMPLES as u32);
                self.shared.game_seq.store(u32::from(self.seq), Relaxed);
                self.shared.game_ts.store(self.ts, Relaxed);
            }
            Err(e) => log::debug!("game-audio frame not encoded: {e}"),
        }
    }

    fn run(mut self, mut consumer: rtrb::Consumer<f32>, stop: &AtomicBool) {
        let _priority = gf_sys::raise_audio_thread_priority();
        let mut buf = vec![0f32; 4096];
        let mut pending: Vec<f32> = Vec::with_capacity(RING_SAMPLES);
        let mut last_data = Instant::now();
        while !stop.load(Relaxed) && !self.shared.stopping() {
            let n = consumer.pop_partial_slice(&mut buf).0.len();
            if n > 0 {
                last_data = Instant::now();
                for s in &mut buf[..n] {
                    if !s.is_finite() {
                        *s = 0.0;
                    }
                }
                pending.extend_from_slice(&buf[..n]);
                let mut start = 0;
                while pending.len() - start >= FRAME_LEN {
                    self.send(&pending[start..start + FRAME_LEN]);
                    start += FRAME_LEN;
                }
                pending.drain(..start);
                continue;
            }
            let quiet = last_data.elapsed();
            if quiet >= FLUSH_AFTER && !pending.is_empty() {
                pending.resize(FRAME_LEN, 0.0);
                self.send(&pending);
                pending.clear();
            }
            if quiet >= IDLE_AFTER {
                self.meter.reset();
                self.shared.meters.game_level.store(0.0);
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        self.shared.meters.game_level.store(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::tests::sine;
    use crate::AudioSettings;
    use std::sync::Mutex;

    #[test]
    fn gaps_are_flushed_and_numbering_continues() {
        let shared = Arc::new(Shared::new(&AudioSettings::default()));
        let sent: Arc<Mutex<Vec<(u16, u32)>>> = Arc::default();
        let s = sent.clone();
        let sink: PacketSink = Arc::new(move |stream, seq, ts, p: &[u8]| {
            assert_eq!(stream, AudioStream::Game);
            assert!(!p.is_empty());
            s.lock().unwrap().push((seq, ts));
        });
        let enc = GameEncoder::new(shared.clone(), sink.clone()).unwrap();
        let (mut producer, consumer) = rtrb::RingBuffer::new(RING_SAMPLES);
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let t = std::thread::spawn(move || enc.run(consumer, &st));
        // 2.5 frames, then silence (the program went quiet).
        let tone = sine(440.0, 0.3, 0, AUDIO_FRAME_SAMPLES * 5 / 2, 2);
        producer.push_entire_slice(&tone).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(sent.lock().unwrap().len(), 3, "partial frame flushed");
        assert_eq!(shared.meters.game_level.load(), 0.0, "meter idles");
        stop.store(true, Relaxed);
        t.join().unwrap();

        // A new capture continues the numbering.
        let mut enc = GameEncoder::new(shared.clone(), sink).unwrap();
        enc.send(&tone[..FRAME_LEN]);
        assert!(shared.meters.game_level.load() > 0.5);
        let got = sent.lock().unwrap().clone();
        assert_eq!(got, vec![(0, 0), (1, 480), (2, 960), (3, 1440)]);
    }
}
