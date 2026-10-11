//! Sound devices (cpal): listing, opening, and the device thread that owns
//! the streams and reopens them when they fail or the user picks another.
//!
//! The stream callbacks only move samples between the device and an `rtrb`
//! ring: the input callback converts to mono f32 and pushes, the output
//! callback pops (silence on underrun). Everything else runs elsewhere.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample, StreamConfig};
use crossbeam_channel::{RecvTimeoutError, Sender};

use crate::mic::InputFeed;
use crate::playout::OutputFeed;
use crate::shared::{lock, Shared};

pub(crate) const NO_MIC: &str =
    "No microphone found — your friend can't hear you, but you can still hear them.";
pub(crate) const NO_SPEAKERS: &str = "No speakers or headphones found.";

/// How often a missing or failed device is tried again.
const RETRY_EVERY: Duration = Duration::from_secs(3);
const BUILD_TIMEOUT: Duration = Duration::from_secs(5);
/// Ring buffer length between a device callback and its thread.
const RING_MS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    Input,
    Output,
}

/// Names of the devices for `dir`, as used in `AudioSettings`.
pub(crate) fn list(dir: Dir) -> Vec<String> {
    let names = catch_unwind(|| {
        let host = cpal::default_host();
        devices(&host, dir)
            .iter()
            .filter_map(device_name)
            .collect::<Vec<_>>()
    })
    .unwrap_or_else(|_| {
        log::warn!("listing sound devices failed");
        Vec::new()
    });
    let mut unique: Vec<String> = Vec::with_capacity(names.len());
    for n in names {
        if !unique.contains(&n) {
            unique.push(n);
        }
    }
    unique
}

fn devices(host: &cpal::Host, dir: Dir) -> Vec<cpal::Device> {
    let found = match dir {
        Dir::Input => host.input_devices().map(|d| d.collect()),
        Dir::Output => host.output_devices().map(|d| d.collect()),
    };
    found.unwrap_or_else(|e| {
        log::debug!("couldn't list {dir:?} devices: {e}");
        Vec::new()
    })
}

fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().to_string())
}

fn find_device(host: &cpal::Host, dir: Dir, name: &str) -> Option<cpal::Device> {
    devices(host, dir)
        .into_iter()
        .find(|d| device_name(d).as_deref() == Some(name))
}

fn default_device(host: &cpal::Host, dir: Dir) -> Option<cpal::Device> {
    match dir {
        Dir::Input => host.default_input_device(),
        Dir::Output => host.default_output_device(),
    }
}

/// What went wrong opening a device, in terms the user can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Problem {
    Missing,
    Busy,
    Blocked,
    Failed,
    Stopped,
}

impl Problem {
    fn from_error(e: &cpal::Error) -> Problem {
        match e.kind() {
            ErrorKind::DeviceNotAvailable | ErrorKind::HostUnavailable => Problem::Missing,
            ErrorKind::DeviceBusy => Problem::Busy,
            ErrorKind::PermissionDenied => Problem::Blocked,
            _ => Problem::Failed,
        }
    }

    fn message(self, dir: Dir) -> String {
        let text = match (dir, self) {
            (Dir::Input, Problem::Missing) => NO_MIC,
            (Dir::Output, Problem::Missing) => NO_SPEAKERS,
            (Dir::Input, Problem::Busy) => "Another program is using the microphone exclusively. Close that program, or pick another microphone in Settings.",
            (Dir::Output, Problem::Busy) => "Another program is using the speakers or headphones exclusively. Close that program, or pick another output in Settings.",
            (Dir::Input, Problem::Blocked) => "Windows is blocking the microphone. Open Windows Settings → Privacy & security → Microphone and allow desktop apps to use it.",
            (Dir::Output, Problem::Blocked) => "Windows is blocking access to the speakers or headphones. Try picking another output in Settings.",
            (Dir::Input, Problem::Failed) => "The microphone couldn't be started. Pick another one in Settings, or unplug it and plug it back in.",
            (Dir::Output, Problem::Failed) => "The speakers or headphones couldn't be started. Pick another output in Settings, or unplug them and plug them back in.",
            (Dir::Input, Problem::Stopped) => "The microphone stopped working (was it unplugged?). GloorpFlorp keeps trying to reconnect it.",
            (Dir::Output, Problem::Stopped) => "The speakers or headphones stopped working (were they unplugged?). GloorpFlorp keeps trying to reconnect them.",
        };
        text.to_string()
    }
}

fn fallback_message(dir: Dir, name: &str) -> String {
    match dir {
        Dir::Input => format!(
            "The microphone “{name}” isn't connected, so the default microphone is used instead."
        ),
        Dir::Output => format!(
            "“{name}” isn't connected, so the default speakers or headphones are used instead."
        ),
    }
}

/// Device errors that end the stream (anything else, like an xrun or the
/// default device being swapped under us, cpal handles itself).
fn is_fatal(kind: ErrorKind) -> bool {
    !matches!(
        kind,
        ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
    )
}

fn error_callback(failed: Arc<AtomicBool>) -> impl FnMut(cpal::Error) + Send + 'static {
    // Called on cpal's stream thread: just raise the flag for the device thread.
    move |e| {
        if is_fatal(e.kind()) {
            failed.store(true, Relaxed);
        }
    }
}

/// The device's default rate and channel count, preferring f32 samples.
fn pick_config(
    device: &cpal::Device,
    dir: Dir,
) -> Result<(StreamConfig, SampleFormat), cpal::Error> {
    let default = match dir {
        Dir::Input => device.default_input_config()?,
        Dir::Output => device.default_output_config()?,
    };
    let config = default.config();
    if config.channels == 0 || !(8_000..=384_000).contains(&config.sample_rate) {
        return Err(cpal::Error::with_message(
            ErrorKind::UnsupportedConfig,
            format!("unusable default format {config:?}"),
        ));
    }
    let mut format = default.sample_format();
    if format != SampleFormat::F32 {
        let ranges: Vec<_> = match dir {
            Dir::Input => device.supported_input_configs().map(|r| r.collect()),
            Dir::Output => device.supported_output_configs().map(|r| r.collect()),
        }
        .unwrap_or_default();
        let f32_ok = ranges.iter().any(|r| {
            r.sample_format() == SampleFormat::F32
                && r.channels() == config.channels
                && (r.min_sample_rate()..=r.max_sample_rate()).contains(&config.sample_rate)
        });
        if f32_ok {
            format = SampleFormat::F32;
        }
    }
    Ok((config, format))
}

/// Calls `$f::<T>($args)` for the Rust type matching a cpal sample format.
macro_rules! with_sample_type {
    ($format:expr, $f:ident($($arg:expr),*)) => {
        match $format {
            SampleFormat::F32 => $f::<f32>($($arg),*),
            SampleFormat::F64 => $f::<f64>($($arg),*),
            SampleFormat::I16 => $f::<i16>($($arg),*),
            SampleFormat::I24 => $f::<cpal::I24>($($arg),*),
            SampleFormat::I32 => $f::<i32>($($arg),*),
            SampleFormat::I8 => $f::<i8>($($arg),*),
            SampleFormat::U8 => $f::<u8>($($arg),*),
            SampleFormat::U16 => $f::<u16>($($arg),*),
            SampleFormat::U32 => $f::<u32>($($arg),*),
            other => Err(cpal::Error::with_message(
                ErrorKind::UnsupportedConfig,
                format!("sample format {other:?} isn't supported"),
            )),
        }
    };
}

fn input_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut producer: rtrb::Producer<f32>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels).max(1);
    let scale = 1.0 / channels as f32;
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            // Real-time: downmix to mono into the ring; no locks, no allocation.
            let frames = (data.len() / channels).min(producer.slots());
            if let Ok(chunk) = producer.write_chunk_uninit(frames) {
                chunk.fill_from_iter(
                    data.chunks_exact(channels)
                        .map(|f| f.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() * scale),
                );
            }
        },
        error_callback(failed),
        Some(BUILD_TIMEOUT),
    )
}

struct OutputShared {
    channels: usize,
    max_request: Arc<AtomicUsize>,
    underruns: Arc<AtomicU64>,
}

fn output_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut consumer: rtrb::Consumer<f32>,
    failed: Arc<AtomicBool>,
    out: OutputShared,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            // Real-time: copy out of the ring, silence for whatever is missing.
            out.max_request.fetch_max(data.len(), Relaxed);
            let n = data.len().min(consumer.slots()) / out.channels * out.channels;
            if let Ok(chunk) = consumer.read_chunk(n) {
                let (a, b) = chunk.as_slices();
                for (d, &s) in data.iter_mut().zip(a.iter().chain(b)) {
                    *d = T::from_sample(s);
                }
                chunk.commit_all();
            }
            if n < data.len() {
                data[n..].fill(T::EQUILIBRIUM);
                out.underruns.fetch_add(1, Relaxed);
            }
        },
        error_callback(failed),
        Some(BUILD_TIMEOUT),
    )
}

enum FeedTx {
    Input(Sender<Option<InputFeed>>),
    Output(Sender<Option<OutputFeed>>),
}

enum Feed {
    Input(InputFeed),
    Output(OutputFeed),
}

/// One direction (microphone or speakers): what the user wants, the open
/// stream, and its health.
struct Slot {
    dir: Dir,
    wanted: Option<String>,
    tx: FeedTx,
    shared: Arc<Shared>,
    stream: Option<cpal::Stream>,
    failed: Arc<AtomicBool>,
    /// The chosen device is missing and the default is used meanwhile.
    on_fallback: bool,
    last_attempt: Instant,
}

impl Slot {
    fn new(dir: Dir, wanted: Option<String>, tx: FeedTx, shared: Arc<Shared>) -> Slot {
        Slot {
            dir,
            wanted,
            tx,
            shared,
            stream: None,
            failed: Arc::new(AtomicBool::new(false)),
            on_fallback: false,
            last_attempt: Instant::now(),
        }
    }

    fn what(&self) -> &'static str {
        match self.dir {
            Dir::Input => "microphone",
            Dir::Output => "speakers",
        }
    }

    fn send(&self, feed: Option<Feed>) {
        // The receiving thread only goes away when the engine stops.
        match (&self.tx, feed) {
            (FeedTx::Input(tx), Some(Feed::Input(f))) => drop(tx.send(Some(f))),
            (FeedTx::Output(tx), Some(Feed::Output(f))) => drop(tx.send(Some(f))),
            (FeedTx::Input(tx), _) => drop(tx.send(None)),
            (FeedTx::Output(tx), _) => drop(tx.send(None)),
        }
    }

    fn set_error(&self, msg: Option<String>) {
        let slot = match self.dir {
            Dir::Input => &self.shared.input_error,
            Dir::Output => &self.shared.output_error,
        };
        let mut current = lock(slot);
        if *current != msg {
            if let Some(m) = &msg {
                log::warn!("{m}");
            }
            *current = msg;
        }
    }

    fn close(&mut self) {
        if let Some(stream) = self.stream.take() {
            self.send(None);
            drop(stream);
        }
    }

    fn open(&mut self) {
        self.close();
        self.last_attempt = Instant::now();
        self.failed = Arc::new(AtomicBool::new(false));
        match catch_unwind(AssertUnwindSafe(|| self.try_open())) {
            Ok(Ok((stream, feed, label))) => {
                log::info!("{}: {label}", self.what());
                self.stream = Some(stream);
                self.send(Some(feed));
                let note = match (&self.wanted, self.on_fallback) {
                    (Some(name), true) => Some(fallback_message(self.dir, name)),
                    _ => None,
                };
                self.set_error(note);
            }
            Ok(Err(problem)) => {
                self.send(None);
                self.set_error(Some(problem.message(self.dir)));
            }
            Err(_) => {
                log::warn!("opening the {} panicked", self.what());
                self.send(None);
                self.set_error(Some(Problem::Failed.message(self.dir)));
            }
        }
    }

    fn try_open(&mut self) -> Result<(cpal::Stream, Feed, String), Problem> {
        let (dir, what) = (self.dir, self.what());
        let host = cpal::default_host();
        let mut fallback = false;
        let device = match &self.wanted {
            Some(name) => find_device(&host, dir, name).or_else(|| {
                log::debug!("{what} {name:?} not found; using the default");
                fallback = true;
                default_device(&host, dir)
            }),
            None => default_device(&host, dir),
        };
        self.on_fallback = fallback;
        let device = device.ok_or(Problem::Missing)?;
        let name = device_name(&device).unwrap_or_else(|| "unnamed device".to_string());
        let fail = |e: cpal::Error| {
            log::warn!("{what} {name:?}: {e}");
            Problem::from_error(&e)
        };
        let (config, format) = pick_config(&device, dir).map_err(fail)?;
        let rate = config.sample_rate;
        let channels = usize::from(config.channels);
        let samples = rate as usize * RING_MS / 1000;
        let failed = self.failed.clone();
        let (stream, feed) = match dir {
            Dir::Input => {
                let (producer, consumer) = rtrb::RingBuffer::new(samples);
                let stream =
                    with_sample_type!(format, input_stream(&device, config, producer, failed))
                        .map_err(fail)?;
                (stream, Feed::Input(InputFeed { consumer, rate }))
            }
            Dir::Output => {
                let (producer, consumer) = rtrb::RingBuffer::new(samples * channels);
                let max_request = Arc::new(AtomicUsize::new(0));
                let underruns = Arc::new(AtomicU64::new(0));
                let shared = OutputShared {
                    channels,
                    max_request: max_request.clone(),
                    underruns: underruns.clone(),
                };
                let stream = with_sample_type!(
                    format,
                    output_stream(&device, config, consumer, failed, shared)
                )
                .map_err(fail)?;
                let feed = OutputFeed {
                    producer,
                    rate,
                    channels,
                    max_request,
                    underruns,
                };
                (stream, Feed::Output(feed))
            }
        };
        stream.play().map_err(fail)?;
        Ok((
            stream,
            feed,
            format!("{name} ({rate} Hz, {channels} ch, {format:?})"),
        ))
    }

    /// Periodic health check: reopens failed streams, retries missing
    /// devices, and switches back once a chosen device is plugged in again.
    fn check(&mut self) {
        let due = self.last_attempt.elapsed() >= RETRY_EVERY;
        if self.stream.is_some() && self.failed.load(Relaxed) {
            log::warn!("the {} stopped working", self.what());
            self.close();
            self.set_error(Some(Problem::Stopped.message(self.dir)));
            if due {
                self.open();
            }
            return;
        }
        if !due {
            return;
        }
        let back = self.on_fallback
            && self.wanted.as_deref().is_some_and(|name| {
                catch_unwind(|| find_device(&cpal::default_host(), self.dir, name).is_some())
                    .unwrap_or(false)
            });
        if self.stream.is_none() || back {
            self.open();
        } else {
            self.last_attempt = Instant::now();
        }
    }
}

enum Cmd {
    Input(Option<String>),
    Output(Option<String>),
}

/// The thread that owns the cpal streams.
pub(crate) struct Devices {
    tx: Option<Sender<Cmd>>,
    thread: Option<JoinHandle<()>>,
}

impl Devices {
    /// Opens both devices (waiting up to a few seconds so the status is
    /// accurate right away) and keeps them healthy from then on.
    pub fn start(
        shared: Arc<Shared>,
        input: Option<String>,
        output: Option<String>,
        input_tx: Sender<Option<InputFeed>>,
        output_tx: Sender<Option<OutputFeed>>,
    ) -> anyhow::Result<Devices> {
        let (tx, rx) = crossbeam_channel::unbounded::<Cmd>();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<()>(1);
        let thread = std::thread::Builder::new()
            .name("gf-audio-devices".into())
            .spawn(move || {
                let mut output = Slot::new(
                    Dir::Output,
                    output,
                    FeedTx::Output(output_tx),
                    shared.clone(),
                );
                let mut input =
                    Slot::new(Dir::Input, input, FeedTx::Input(input_tx), shared.clone());
                output.open();
                input.open();
                let _ = ready_tx.send(());
                loop {
                    match rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(Cmd::Input(name)) => {
                            input.wanted = name;
                            input.open();
                        }
                        Ok(Cmd::Output(name)) => {
                            output.wanted = name;
                            output.open();
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    if shared.stopping() {
                        break;
                    }
                    input.check();
                    output.check();
                }
                input.close();
                output.close();
            })?;
        if ready_rx.recv_timeout(Duration::from_secs(8)).is_err() {
            log::warn!("opening the sound devices is taking unusually long");
        }
        Ok(Devices {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    pub fn set_input(&self, name: Option<String>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Input(name));
        }
    }

    pub fn set_output(&self, name: Option<String>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Output(name));
        }
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        // Closing the channel ends the thread's loop.
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            crate::join_with_timeout(t, "sound-device", Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_never_panics() {
        // Real devices may or may not exist here; names must be usable.
        for dir in [Dir::Input, Dir::Output] {
            let names = list(dir);
            assert!(names.iter().all(|n| !n.is_empty()));
            let mut sorted = names.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), names.len(), "no duplicates");
        }
    }

    #[test]
    fn messages_are_plain() {
        for dir in [Dir::Input, Dir::Output] {
            for p in [
                Problem::Missing,
                Problem::Busy,
                Problem::Blocked,
                Problem::Failed,
                Problem::Stopped,
            ] {
                let m = p.message(dir);
                assert!(m.ends_with('.'), "{m}");
                assert!(!m.contains("cpal") && !m.contains("WASAPI"), "{m}");
            }
            assert!(fallback_message(dir, "USB Headset").contains("“USB Headset”"));
        }
        assert_eq!(Problem::Missing.message(Dir::Input), NO_MIC);
        assert_eq!(Problem::Missing.message(Dir::Output), NO_SPEAKERS);
        let e = cpal::Error::new(ErrorKind::PermissionDenied);
        assert_eq!(Problem::from_error(&e), Problem::Blocked);
        assert!(!is_fatal(ErrorKind::Xrun));
        assert!(is_fatal(ErrorKind::DeviceNotAvailable));
    }
}
