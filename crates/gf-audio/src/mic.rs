//! Outgoing voice: microphone samples → 48 kHz frames → sonora (echo
//! cancellation, noise suppression, auto gain, high-pass) → gate → Opus.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use gf_proto::{AudioStream, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE};
use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, MaxProcessingRate,
    NoiseSuppression, NoiseSuppressionLevel, Pipeline,
};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::codec::{OpusEncoder, MAX_PACKET};
use crate::gate::{KeyFn, VoiceGate};
use crate::level::{rms_dbfs, LevelMeter};
use crate::resample::Resampler;
use crate::shared::{Processing, Shared};
use crate::{NoiseLevel, PacketSink};

/// Rough playback-to-capture delay: the output queue (~30 ms) plus typical
/// device buffers. Only a starting hint; AEC3 measures the real delay itself.
const ECHO_DELAY_HINT_MS: i32 = 50;
/// More echo reference than this waiting means the mic thread stalled; older
/// frames are useless by now and are dropped.
const MAX_REFERENCE_BACKLOG: usize = 25;

/// An opened microphone, handed from the device thread to the mic thread.
pub(crate) struct InputFeed {
    /// Mono samples at the device's rate, pushed by the device callback.
    pub consumer: rtrb::Consumer<f32>,
    pub rate: u32,
}

/// The sonora stage. Rebuilt whenever the processing toggles change.
pub(crate) struct MicProcessor {
    apm: Option<AudioProcessing>,
    current: Option<Processing>,
    out: Vec<f32>,
    scratch: Vec<f32>,
}

impl MicProcessor {
    pub fn new() -> MicProcessor {
        MicProcessor {
            apm: None,
            current: None,
            out: vec![0.0; AUDIO_FRAME_SAMPLES],
            scratch: vec![0.0; AUDIO_FRAME_SAMPLES],
        }
    }

    pub fn configure(&mut self, p: Processing) {
        if self.current == Some(p) {
            return;
        }
        log::info!(
            "voice processing: echo cancellation {}, noise suppression {}, auto gain {}",
            on_off(p.echo_cancel),
            p.noise_suppress
                .map_or("off".to_string(), |l| format!("{l:?}")),
            on_off(p.auto_gain)
        );
        self.current = Some(p);
        self.apm = Some(build_apm(p));
    }

    /// One 10 ms frame of what the speakers are about to play.
    pub fn render(&mut self, reference: &[f32]) {
        if !self.current.is_some_and(|p| p.echo_cancel) {
            return;
        }
        if let Some(apm) = self.apm.as_mut() {
            if let Err(e) = apm.process_render_f32(&[reference], &mut [&mut self.scratch[..]]) {
                log::trace!("echo reference rejected: {e}");
            }
        }
    }

    /// Processes one 10 ms mono frame in place.
    pub fn process(&mut self, frame: &mut [f32]) {
        let Some(apm) = self.apm.as_mut() else {
            return;
        };
        let _ = apm.set_stream_delay_ms(ECHO_DELAY_HINT_MS);
        match apm.process_capture_f32(&[&frame[..]], &mut [&mut self.out[..]]) {
            Ok(()) => frame.copy_from_slice(&self.out),
            Err(e) => log::trace!("voice processing skipped a frame: {e}"),
        }
    }
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

fn build_apm(p: Processing) -> AudioProcessing {
    let config = Config {
        pipeline: Pipeline {
            maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
            ..Default::default()
        },
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: p.echo_cancel.then(EchoCanceller::default),
        noise_suppression: p.noise_suppress.map(|level| NoiseSuppression {
            level: match level {
                NoiseLevel::Low => NoiseSuppressionLevel::Low,
                NoiseLevel::Moderate => NoiseSuppressionLevel::Moderate,
                NoiseLevel::High => NoiseSuppressionLevel::High,
                NoiseLevel::VeryHigh => NoiseSuppressionLevel::VeryHigh,
            },
            ..Default::default()
        }),
        gain_controller2: p.auto_gain.then(|| GainController2 {
            adaptive_digital: Some(AdaptiveDigital::default()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let stream = StreamConfig::new(AUDIO_SAMPLE_RATE, 1);
    AudioProcessing::builder()
        .config(config)
        .capture_config(stream)
        .render_config(stream)
        .build()
}

/// Encodes and sends voice frames, numbering them.
pub(crate) struct VoiceSender {
    enc: OpusEncoder,
    seq: u16,
    ts: u32,
    buf: Vec<u8>,
}

impl VoiceSender {
    pub fn new(bitrate_kbps: u32) -> anyhow::Result<VoiceSender> {
        Ok(VoiceSender {
            enc: OpusEncoder::voice(bitrate_kbps)?,
            seq: 0,
            ts: 0,
            buf: vec![0; MAX_PACKET],
        })
    }

    /// One 10 ms frame went by. `ts` counts every frame (the sample clock,
    /// also during silence); `seq` counts only packets actually sent.
    pub fn frame(&mut self, pcm: &[f32], send: bool, bitrate_kbps: u32, sink: &PacketSink) {
        if send {
            self.enc.set_bitrate(bitrate_kbps);
            match self.enc.encode(pcm, &mut self.buf) {
                Ok(n) => {
                    sink(AudioStream::Voice, self.seq, self.ts, &self.buf[..n]);
                    self.seq = self.seq.wrapping_add(1);
                }
                Err(e) => log::debug!("voice frame not encoded: {e}"),
            }
        }
        self.ts = self.ts.wrapping_add(AUDIO_FRAME_SAMPLES as u32);
    }
}

/// Everything that happens to one 48 kHz frame of microphone audio.
pub(crate) struct MicPipeline {
    shared: Arc<Shared>,
    sink: PacketSink,
    reference: rtrb::Consumer<f32>,
    reference_frame: Vec<f32>,
    processor: MicProcessor,
    /// Fake mode: the test tone goes out unprocessed.
    bypass: bool,
    gate: VoiceGate,
    meter: LevelMeter,
    sender: VoiceSender,
}

impl MicPipeline {
    pub fn new(
        shared: Arc<Shared>,
        sink: PacketSink,
        reference: rtrb::Consumer<f32>,
        key_down: KeyFn,
        bypass: bool,
    ) -> anyhow::Result<MicPipeline> {
        let sender = VoiceSender::new(shared.live.voice_bitrate_kbps())?;
        Ok(MicPipeline {
            shared,
            sink,
            reference,
            reference_frame: vec![0.0; AUDIO_FRAME_SAMPLES],
            processor: MicProcessor::new(),
            bypass,
            gate: VoiceGate::new(key_down),
            meter: LevelMeter::default(),
            sender,
        })
    }

    /// Hands every complete echo-reference frame to the echo canceller.
    fn feed_reference(&mut self) {
        let mut pending = self.reference.slots() / AUDIO_FRAME_SAMPLES;
        while pending > 0 {
            if self
                .reference
                .pop_entire_slice(&mut self.reference_frame)
                .is_err()
            {
                break;
            }
            if pending <= MAX_REFERENCE_BACKLOG {
                self.processor.render(&self.reference_frame);
            }
            pending -= 1;
        }
    }

    /// Drops all queued echo reference (no microphone, or it just changed).
    fn skip_reference(&mut self) {
        let n = self.reference.slots();
        if let Ok(chunk) = self.reference.read_chunk(n) {
            chunk.commit_all();
        }
    }

    pub fn process_frame(&mut self, frame: &mut [f32]) {
        // A misbehaving driver must not poison the processing state.
        for s in frame.iter_mut() {
            *s = if s.is_finite() {
                s.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        if self.bypass {
            self.skip_reference();
        } else {
            self.processor.configure(self.shared.live.processing());
            self.feed_reference();
            self.processor.process(frame);
        }
        let live = &self.shared.live;
        let level = rms_dbfs(frame);
        // (Never set in bypass mode: nothing was processed.)
        let auto_gain = self.processor.current.is_some_and(|p| p.auto_gain);
        let send = self.gate.update(
            live.voice_mode(),
            live.ptt_key(),
            live.vad_sensitivity(),
            auto_gain,
            live.mic_muted(),
            level,
        );
        let meters = &self.shared.meters;
        meters.mic_level.store(self.meter.update(level));
        meters.transmitting.store(send, Relaxed);
        self.sender
            .frame(frame, send, live.voice_bitrate_kbps(), &self.sink);
    }

    /// No microphone yet, or a new one just opened.
    fn idle(&mut self) {
        self.skip_reference();
        // (The fake test tone has no start-up noise to wait out.)
        self.gate.restart(!self.bypass);
        self.meter.reset();
        self.shared.meters.mic_level.store(0.0);
        self.shared.meters.transmitting.store(false, Relaxed);
    }
}

/// The mic thread: waits for microphone samples and pushes them through
/// the pipeline 10 ms at a time.
pub(crate) fn run(mut pipeline: MicPipeline, feeds: Receiver<Option<InputFeed>>) {
    let _priority = gf_sys::raise_audio_thread_priority();
    let shared = pipeline.shared.clone();
    let mut input: Option<(rtrb::Consumer<f32>, Resampler)> = None;
    let mut raw = vec![0f32; 4096];
    let mut pending: Vec<f32> = Vec::with_capacity(16 * AUDIO_FRAME_SAMPLES);
    let mut frame = vec![0f32; AUDIO_FRAME_SAMPLES];
    while !shared.stopping() {
        for feed in feeds.try_iter() {
            input = feed.and_then(|f| match Resampler::new(f.rate, AUDIO_SAMPLE_RATE, 1) {
                Ok(rs) => Some((f.consumer, rs)),
                Err(e) => {
                    log::warn!("can't use this microphone: {e}");
                    None
                }
            });
            pending.clear();
            pipeline.idle();
        }
        let Some((consumer, resampler)) = input.as_mut() else {
            pipeline.idle();
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let n = consumer.pop_partial_slice(&mut raw).0.len();
        if n == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        resampler.process(&raw[..n], |chunk| pending.extend_from_slice(chunk));
        let mut start = 0;
        while pending.len() - start >= AUDIO_FRAME_SAMPLES {
            frame.copy_from_slice(&pending[start..start + AUDIO_FRAME_SAMPLES]);
            start += AUDIO_FRAME_SAMPLES;
            pipeline.process_frame(&mut frame);
        }
        pending.drain(..start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::tests::{rms, sine};
    use crate::gate::MIC_SETTLE_FRAMES;
    use crate::{AudioSettings, VoiceMode};
    use std::sync::Mutex;

    type Sent = Arc<Mutex<Vec<(AudioStream, u16, u32, usize)>>>;

    fn pipeline(settings: AudioSettings, bypass: bool) -> (MicPipeline, rtrb::Producer<f32>, Sent) {
        let shared = Arc::new(Shared::new(&settings));
        let sent: Sent = Arc::default();
        let s = sent.clone();
        let sink: PacketSink = Arc::new(move |stream, seq, ts, p: &[u8]| {
            s.lock().unwrap().push((stream, seq, ts, p.len()))
        });
        let (producer, consumer) = rtrb::RingBuffer::new(48_000);
        let p = MicPipeline::new(shared, sink, consumer, Box::new(|_| false), bypass).unwrap();
        (p, producer, sent)
    }

    /// Voiced, syllable-like test signal (a pure tone would be treated as
    /// noise and removed by the noise suppressor).
    fn speechish(i: usize, f0: f64) -> f32 {
        let t = i as f64 / f64::from(AUDIO_SAMPLE_RATE);
        let f = f0 + 30.0 * (t * 1.3).sin();
        let voiced: f64 = (1..8)
            .map(|h| (t * f * h as f64 * std::f64::consts::TAU).sin() / h as f64)
            .sum();
        let syllables = (t * 3.0 * std::f64::consts::TAU).sin().max(0.0);
        (0.15 * voiced * syllables) as f32
    }

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }

    fn mean(v: &[f32]) -> f32 {
        v.iter().sum::<f32>() / v.len() as f32
    }

    #[test]
    fn echo_is_cancelled() {
        let n = AUDIO_FRAME_SAMPLES;
        let far: Vec<f32> = (0..600 * n).map(|i| speechish(i, 220.0)).collect();
        let delay = 30 * 48;
        for echo_cancel in [false, true] {
            let mut p = MicProcessor::new();
            p.configure(Processing {
                echo_cancel,
                noise_suppress: None,
                auto_gain: false,
            });
            let (mut echo_in, mut echo_out) = (Vec::new(), Vec::new());
            for f in 0..600 {
                p.render(&far[f * n..(f + 1) * n]);
                // The speakers' sound reaches the mic 30 ms later, quieter.
                let mut frame: Vec<f32> = (f * n..(f + 1) * n)
                    .map(|i| {
                        if i >= delay {
                            0.3 * far[i - delay]
                        } else {
                            0.0
                        }
                    })
                    .collect();
                echo_in.push(rms(&frame));
                p.process(&mut frame);
                echo_out.push(rms(&frame));
            }
            let (input, output) = (mean(&echo_in[500..]), mean(&echo_out[500..]));
            if echo_cancel {
                assert!(output < 0.2 * input, "echo {input} -> {output}");
            } else {
                assert!(
                    output > 0.7 * input,
                    "no cancelling when off: {input} -> {output}"
                );
            }
        }
    }

    #[test]
    fn stronger_noise_levels_remove_more_noise() {
        let kept: Vec<f32> = [
            NoiseLevel::Low,
            NoiseLevel::Moderate,
            NoiseLevel::High,
            NoiseLevel::VeryHigh,
        ]
        .into_iter()
        .map(|level| {
            let mut p = MicProcessor::new();
            p.configure(Processing {
                echo_cancel: false,
                noise_suppress: Some(level),
                auto_gain: false,
            });
            let mut seed = 1;
            let (mut input, mut output) = (Vec::new(), Vec::new());
            for _ in 0..200 {
                let mut frame: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
                    .map(|_| 0.01 * noise(&mut seed))
                    .collect();
                input.push(rms(&frame));
                p.process(&mut frame);
                output.push(rms(&frame));
            }
            mean(&output[100..]) / mean(&input[100..])
        })
        .collect();
        assert!(
            kept.windows(2).all(|w| w[0] > w[1]),
            "noise kept per level: {kept:?}"
        );
    }

    #[test]
    fn noise_suppressed_speech_kept_and_rebuilt_live() {
        let settings = AudioSettings {
            voice_mode: VoiceMode::AlwaysOn,
            echo_cancel: false,
            auto_gain: false,
            ..Default::default()
        };
        let (mut p, mut reference, sent) = pipeline(settings.clone(), false);
        let mut seed = 1;
        let (mut input, mut output) = (Vec::new(), Vec::new());
        for f in 0..400 {
            // 2 s of fan noise, then speech over it.
            let _ = reference.push_entire_slice(&[0.0; AUDIO_FRAME_SAMPLES]);
            let mut frame: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
                .map(|k| {
                    let speech = if f >= 200 {
                        speechish(f * AUDIO_FRAME_SAMPLES + k, 140.0)
                    } else {
                        0.0
                    };
                    speech + 0.01 * noise(&mut seed)
                })
                .collect();
            input.push(rms(&frame));
            p.process_frame(&mut frame);
            assert!(frame.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
            output.push(rms(&frame));
        }
        let noise_ratio = mean(&output[100..200]) / mean(&input[100..200]);
        let speech_ratio = mean(&output[300..]) / mean(&input[300..]);
        assert!(noise_ratio < 0.3, "noise kept {noise_ratio}");
        assert!(speech_ratio > 0.4, "speech kept {speech_ratio}");

        // Turning everything on rebuilds the processor; it keeps working.
        p.shared.live.apply(&AudioSettings {
            voice_mode: VoiceMode::AlwaysOn,
            ..Default::default()
        });
        for f in 400..500 {
            let _ = reference.push_entire_slice(&[0.0; AUDIO_FRAME_SAMPLES]);
            let mut frame: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
                .map(|k| speechish(f * AUDIO_FRAME_SAMPLES + k, 140.0))
                .collect();
            p.process_frame(&mut frame);
            assert!(frame.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
        assert_eq!(
            p.processor.current,
            Some(Processing {
                echo_cancel: true,
                noise_suppress: Some(NoiseLevel::High),
                auto_gain: true
            })
        );
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 500);
        for (i, &(stream, seq, ts, len)) in sent.iter().enumerate() {
            assert_eq!(stream, AudioStream::Voice);
            assert_eq!(seq, i as u16);
            assert_eq!(ts, (i * AUDIO_FRAME_SAMPLES) as u32);
            assert!(len > 0);
        }
        assert!(p.shared.meters.transmitting.load(Relaxed));
        assert!(p.shared.meters.mic_level.load() > 0.3);
    }

    #[test]
    fn gate_controls_sending_and_seq_counts_packets() {
        let settings = AudioSettings {
            voice_mode: VoiceMode::VoiceActivity,
            ..Default::default()
        };
        let (mut p, _reference, sent) = pipeline(settings.clone(), true);
        for f in 0..100 {
            let amp = if f < 50 { 0.3 } else { 0.0 };
            let mut frame = sine(300.0, amp, f * AUDIO_FRAME_SAMPLES, AUDIO_FRAME_SAMPLES, 1);
            p.process_frame(&mut frame);
        }
        // 50 loud frames plus the 40-frame hangover.
        let got = sent.lock().unwrap().clone();
        assert_eq!(got.len(), 90);
        assert_eq!(got.last().unwrap().1, 89);
        assert!(!p.shared.meters.transmitting.load(Relaxed));

        p.shared.live.apply(&AudioSettings {
            mic_muted: true,
            ..settings
        });
        for f in 0..20 {
            let mut frame = sine(300.0, 0.3, f * AUDIO_FRAME_SAMPLES, AUDIO_FRAME_SAMPLES, 1);
            p.process_frame(&mut frame);
        }
        assert_eq!(sent.lock().unwrap().len(), 90, "muted");
        // The sample clock kept running: the next packet's ts reflects the gap.
        p.shared.live.apply(&AudioSettings {
            voice_mode: VoiceMode::AlwaysOn,
            ..Default::default()
        });
        let mut frame = vec![0.0; AUDIO_FRAME_SAMPLES];
        p.process_frame(&mut frame);
        let last = *sent.lock().unwrap().last().unwrap();
        assert_eq!((last.1, last.2), (90, 120 * AUDIO_FRAME_SAMPLES as u32));
    }

    #[test]
    fn new_microphone_settles_before_voice_activation() {
        let settings = AudioSettings {
            voice_mode: VoiceMode::VoiceActivity,
            vad_sensitivity: 1.0,
            echo_cancel: false,
            noise_suppress: false,
            auto_gain: false,
            ..Default::default()
        };
        let (mut p, _reference, sent) = pipeline(settings.clone(), false);
        let loud = |p: &mut MicPipeline, f: usize| {
            let mut frame = sine(300.0, 0.3, f * AUDIO_FRAME_SAMPLES, AUDIO_FRAME_SAMPLES, 1);
            p.process_frame(&mut frame);
        };
        // A microphone opens loud (its start-up burst, or speech already).
        p.idle();
        for f in 0..MIC_SETTLE_FRAMES as usize {
            loud(&mut p, f);
        }
        assert!(sent.lock().unwrap().is_empty());
        assert!(!p.shared.meters.transmitting.load(Relaxed));
        assert!(
            p.shared.meters.mic_level.load() > 0.5,
            "the meter still moves"
        );
        loud(&mut p, MIC_SETTLE_FRAMES as usize);
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert!(p.shared.meters.transmitting.load(Relaxed));

        // Switching microphones mid-sentence: the hangover doesn't carry over.
        p.idle();
        for f in 0..10 {
            let mut frame = vec![0.0; AUDIO_FRAME_SAMPLES];
            p.process_frame(&mut frame);
            assert!(!p.shared.meters.transmitting.load(Relaxed), "frame {f}");
        }
        assert_eq!(sent.lock().unwrap().len(), 1);

        // The fake test tone sends at once.
        let (mut p, _reference, sent) = pipeline(settings, true);
        p.idle();
        loud(&mut p, 0);
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn bad_samples_are_sanitized() {
        let settings = AudioSettings {
            voice_mode: VoiceMode::AlwaysOn,
            ..Default::default()
        };
        let (mut p, _reference, sent) = pipeline(settings, false);
        for f in 0..50 {
            let mut frame = vec![f32::NAN; AUDIO_FRAME_SAMPLES];
            frame[f] = f32::INFINITY;
            frame[f + 1] = 7.0;
            p.process_frame(&mut frame);
            assert!(frame.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
        assert_eq!(sent.lock().unwrap().len(), 50);
    }

    #[test]
    fn stale_reference_is_bounded() {
        let (mut p, mut reference, _) = pipeline(AudioSettings::default(), false);
        let _ = reference.push_entire_slice(&vec![0.1; 100 * AUDIO_FRAME_SAMPLES]);
        let mut frame = vec![0.0; AUDIO_FRAME_SAMPLES];
        p.process_frame(&mut frame);
        assert_eq!(p.reference.slots(), 0);
        let _ = reference.push_entire_slice(&[0.1; 700]);
        p.idle();
        assert_eq!(p.reference.slots(), 0);
    }
}
