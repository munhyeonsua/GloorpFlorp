//! Capture-thread plumbing shared by the real (Windows) and fake game-audio
//! sources.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{LoopbackCapture, SampleCallback};

/// How long `start` waits for the capture thread to report that it's running.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `body` on a new thread. `body` calls `ready()` once capture is
/// running, then loops until `stop` is set. An error before `ready()` is
/// returned from here; an error after it is stored for `LoopbackCapture::error`.
pub(crate) fn launch<F>(name: &str, body: F) -> anyhow::Result<LoopbackCapture>
where
    F: FnOnce(&AtomicBool, &dyn Fn()) -> Result<(), String> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None::<String>));
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let thread = {
        let stop = stop.clone();
        let error = error.clone();
        std::thread::Builder::new().name(name.to_string()).spawn(move || {
            let ready_tx = tx.clone();
            let ready = move || {
                let _ = ready_tx.send(Ok(()));
            };
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| body(&stop, &ready)))
                .unwrap_or_else(|_| {
                    Err("Game-audio capture stopped unexpectedly. Click Start sending game audio to try again, or restart GloorpFlorp.".to_string())
                });
            match result {
                Ok(()) => log::info!("game-audio capture stopped"),
                Err(e) => {
                    log::warn!("game-audio capture failed: {e}");
                    if let Ok(mut slot) = error.lock() {
                        *slot = Some(e.clone());
                    }
                    let _ = tx.send(Err(e));
                }
            }
        })?
    };
    let mut capture = LoopbackCapture {
        stop,
        error,
        thread: Some(thread),
    };
    match rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(capture),
        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Dropping `capture` sets `stop` and waits briefly for the thread.
            capture.stop.store(true, Ordering::SeqCst);
            Err(anyhow::anyhow!(
                "Windows didn't start game-audio capture in time. Try again, or restart the PC if it keeps happening."
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let e = capture
                .error()
                .unwrap_or_else(|| "Game-audio capture couldn't start. Try again.".to_string());
            capture.thread.take().map(|t| t.join());
            Err(anyhow::anyhow!(e))
        }
    }
}

pub(crate) const FAKE_RATE: u32 = 48_000;
const FAKE_CHUNK_FRAMES: usize = 480;

/// 440 Hz sine, amplitude 0.1, stereo 48 kHz, 10 ms chunks at real-time pace.
pub(crate) fn fake_tone(
    mut on_samples: SampleCallback,
    stop: &AtomicBool,
    ready: &dyn Fn(),
) -> Result<(), String> {
    let mut buf = vec![0f32; FAKE_CHUNK_FRAMES * 2];
    let step = 440.0 * std::f64::consts::TAU / f64::from(FAKE_RATE);
    let mut phase = 0f64;
    let chunk = Duration::from_millis(10);
    let start = Instant::now();
    let mut sent: u32 = 0;
    ready();
    while !stop.load(Ordering::Relaxed) {
        for frame in buf.as_chunks_mut::<2>().0 {
            let s = (phase.sin() * 0.1) as f32;
            frame[0] = s;
            frame[1] = s;
            phase = (phase + step) % std::f64::consts::TAU;
        }
        on_samples(&buf);
        sent += 1;
        // Absolute deadlines so the pace doesn't drift.
        let next = start + chunk * sent;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > Duration::from_millis(200) {
            // Badly behind (e.g. suspended): don't burst to catch up.
            sent = (now.duration_since(start).as_millis() / 10) as u32;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect() -> (Arc<Mutex<Vec<f32>>>, SampleCallback) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let cb = Box::new(move |s: &[f32]| sink.lock().unwrap().extend_from_slice(s));
        (got, cb)
    }

    #[test]
    fn fake_tone_is_real_time_and_stops_on_drop() {
        let (got, cb) = collect();
        let t = Instant::now();
        let cap = launch("test-fake", move |stop, ready| fake_tone(cb, stop, ready)).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        drop(cap);
        let elapsed = t.elapsed().as_secs_f64();
        let samples = got.lock().unwrap().clone();
        let frames = samples.len() as f64 / 2.0;
        let expected = elapsed * f64::from(FAKE_RATE);
        assert_eq!(samples.len() % (FAKE_CHUNK_FRAMES * 2), 0);
        assert!(
            frames > expected * 0.7 && frames < expected * 1.2,
            "{frames} frames in {elapsed:.3} s"
        );
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.09 && peak <= 0.1001, "peak {peak}");
        assert!(samples.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]));
        // 440 Hz: about 2 * 440 sign changes per second on one channel.
        let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
        let crossings = left
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count() as f64;
        let hz = crossings / 2.0 / (left.len() as f64 / f64::from(FAKE_RATE));
        assert!((hz - 440.0).abs() < 5.0, "{hz} Hz");
        // Nothing more arrives after drop.
        let len = got.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(got.lock().unwrap().len(), len);
    }

    #[test]
    fn start_error_is_returned() {
        let err = launch("test-fail", |_, _| Err("nope".to_string()))
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "nope");
    }

    #[test]
    fn later_error_is_recorded() {
        let cap = launch("test-late", |_, ready| {
            ready();
            Err("program closed".to_string())
        })
        .unwrap();
        let t = Instant::now();
        while cap.error().is_none() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(cap.error().as_deref(), Some("program closed"));
    }

    #[test]
    fn panic_becomes_error() {
        let err = launch("test-panic", |_, _| panic!("boom")).err().unwrap();
        assert!(err.to_string().contains("stopped unexpectedly"));
    }

    #[test]
    fn env_var_selects_fake_capture() {
        // Safe in edition 2021; only this test touches the variable.
        std::env::set_var("GLOORP_FAKE_AUDIO", "1");
        let (got, cb) = collect();
        let cap = LoopbackCapture::start(crate::GameAudioSource::AllExceptThisApp, cb).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(cap.error().is_none());
        drop(cap);
        assert!(!got.lock().unwrap().is_empty());
    }
}
