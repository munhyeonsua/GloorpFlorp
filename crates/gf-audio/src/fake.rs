//! Test mode (`GLOORP_FAKE_AUDIO=1`): a synthetic microphone so voice packets
//! flow on machines without sound devices. The output side needs nothing
//! special: with no output device the playout thread consumes on its own
//! real-time clock (a null sink).

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use gf_proto::{AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE};

use crate::mic::InputFeed;
use crate::shared::Shared;

pub(crate) const FAKE_MIC_HZ: f64 = 300.0;
const FAKE_MIC_AMPLITUDE: f64 = 0.2;

/// Starts a 300 Hz tone "microphone" that delivers 10 ms at a time at
/// real-time pace, as a 48 kHz device would.
pub(crate) fn start_mic(
    shared: Arc<Shared>,
    feeds: &Sender<Option<InputFeed>>,
) -> std::io::Result<JoinHandle<()>> {
    let (mut producer, consumer) = rtrb::RingBuffer::new(AUDIO_SAMPLE_RATE as usize / 2);
    let _ = feeds.send(Some(InputFeed {
        consumer,
        rate: AUDIO_SAMPLE_RATE,
    }));
    std::thread::Builder::new()
        .name("gf-audio-fake-mic".into())
        .spawn(move || {
            log::info!("microphone: fake {FAKE_MIC_HZ} Hz test tone");
            let step = FAKE_MIC_HZ * std::f64::consts::TAU / f64::from(AUDIO_SAMPLE_RATE);
            let mut phase = 0f64;
            let mut buf = vec![0f32; AUDIO_FRAME_SAMPLES];
            let tick = Duration::from_millis(10);
            let start = Instant::now();
            let mut sent: u32 = 0;
            while !shared.stopping() {
                for s in buf.iter_mut() {
                    *s = (phase.sin() * FAKE_MIC_AMPLITUDE) as f32;
                    phase = (phase + step) % std::f64::consts::TAU;
                }
                let _ = producer.push_partial_slice(&buf);
                sent += 1;
                // Absolute deadlines so the pace doesn't drift.
                let next = start + tick * sent;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else if now - next > Duration::from_millis(200) {
                    // Far behind (suspended?): skip ahead instead of bursting.
                    sent = (now.duration_since(start).as_millis() / 10) as u32;
                }
            }
        })
}
