//! Sound devices (cpal): listing, opening, and the device thread that owns
//! the streams and reopens them when they fail or the user picks another.
//!
//! The stream callbacks only move samples between the device and an `rtrb`
//! ring: the input callback converts to mono f32 and pushes, the output
//! callback pops (silence on underrun). Everything else runs elsewhere.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
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

/// How often a missing or failed device is tried again. A stream that dies
/// after running at least this long is replaced at once.
const RETRY_EVERY: Duration = Duration::from_secs(3);
/// How often the device thread looks at the streams' health.
const CHECK_EVERY: Duration = Duration::from_millis(100);
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
            (Dir::Input, Problem::Busy) => "Another program is using the microphone exclusively. Close that program, or pick another microphone under “Voice”.",
            (Dir::Output, Problem::Busy) => "Another program is using the speakers or headphones exclusively. Close that program, or pick other speakers under “Voice”.",
            (Dir::Input, Problem::Blocked) => "Windows is blocking the microphone. Open Windows Settings, then Privacy & security, then Microphone, and turn on “Let desktop apps access your microphone”.",
            (Dir::Output, Problem::Blocked) => "Windows is blocking access to the speakers or headphones. Try picking other speakers under “Voice”.",
            (Dir::Input, Problem::Failed) => "The microphone couldn't be started. Pick another one under “Voice”, or unplug it and plug it back in.",
            (Dir::Output, Problem::Failed) => "The speakers or headphones couldn't be started. Pick other speakers under “Voice”, or unplug them and plug them back in.",
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

/// What a running stream's error callback reported (the worst so far).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    /// Fine, or a glitch cpal recovers from itself (an xrun, rerouting).
    Ok = 0,
    /// Needs rebuilding but may still play: on Windows, the default device
    /// changed under a default-device stream. Switched over quietly.
    Moved = 1,
    /// The stream is dead.
    Failed = 2,
}

impl Health {
    fn after(kind: ErrorKind) -> Health {
        match kind {
            ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied => Health::Ok,
            ErrorKind::StreamInvalidated => Health::Moved,
            _ => Health::Failed,
        }
    }

    fn load(cell: &AtomicU8) -> Health {
        match cell.load(Relaxed) {
            0 => Health::Ok,
            1 => Health::Moved,
            _ => Health::Failed,
        }
    }
}

fn error_callback(health: Arc<AtomicU8>) -> impl FnMut(cpal::Error) + Send + 'static {
    // Called on cpal's stream thread: just note it for the device thread.
    move |e| {
        health.fetch_max(Health::after(e.kind()) as u8, Relaxed);
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

/// cpal (without its "realtime" feature) runs the stream threads at normal
/// priority, so a game using every core starves them: crackles and lost
/// microphone samples. Raise the thread on the first callback, once (a single
/// system call; the priority stays until cpal's thread ends).
fn raise_priority_once(raised: &mut bool) {
    if !*raised {
        *raised = true;
        gf_sys::keep_audio_thread_priority();
    }
}

fn input_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut producer: rtrb::Producer<f32>,
    health: Arc<AtomicU8>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels).max(1);
    let scale = 1.0 / channels as f32;
    let mut raised = false;
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            raise_priority_once(&mut raised);
            // Real-time: downmix to mono into the ring; no locks, no allocation.
            let frames = (data.len() / channels).min(producer.slots());
            if let Ok(chunk) = producer.write_chunk_uninit(frames) {
                chunk.fill_from_iter(
                    data.chunks_exact(channels)
                        .map(|f| f.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() * scale),
                );
            }
        },
        error_callback(health),
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
    health: Arc<AtomicU8>,
    out: OutputShared,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let mut raised = false;
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            raise_priority_once(&mut raised);
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
        error_callback(health),
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

/// An open stream and how it is doing.
struct Running {
    /// Plays until dropped.
    _stream: cpal::Stream,
    health: Arc<AtomicU8>,
    /// Not on the device we want (it is missing or couldn't be opened);
    /// `check` keeps trying to switch.
    fallback: bool,
    since: Instant,
}

/// One direction (microphone or speakers): what the user wants, the open
/// stream, and its health.
struct Slot {
    dir: Dir,
    wanted: Option<String>,
    tx: FeedTx,
    shared: Arc<Shared>,
    running: Option<Running>,
    last_attempt: Instant,
}

impl Slot {
    fn new(dir: Dir, wanted: Option<String>, tx: FeedTx, shared: Arc<Shared>) -> Slot {
        Slot {
            dir,
            wanted,
            tx,
            shared,
            running: None,
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
        if let Some(running) = self.running.take() {
            self.send(None);
            drop(running);
        }
    }

    /// Opens the device we want (or the default instead, if that's allowed
    /// and it's missing) and swaps it in. A stream that still works keeps
    /// playing until the new one runs, and is kept if the new one can't open.
    fn open(&mut self, allow_default: bool) {
        if self
            .running
            .as_ref()
            .is_some_and(|r| Health::load(&r.health) == Health::Failed)
        {
            self.close();
        }
        self.last_attempt = Instant::now();
        let opened = catch_unwind(AssertUnwindSafe(|| self.try_open(allow_default)))
            .unwrap_or_else(|_| {
                log::warn!("opening the {} panicked", self.what());
                Err(Problem::Failed)
            });
        match opened {
            Ok((running, feed, label)) => {
                log::info!("{}: {label}", self.what());
                let note = match (&self.wanted, running.fallback) {
                    (Some(name), true) => Some(fallback_message(self.dir, name)),
                    _ => None,
                };
                let old = self.running.replace(running);
                self.send(Some(feed));
                drop(old);
                self.set_error(note);
            }
            Err(problem) => {
                let note = match self.running.as_mut() {
                    Some(running) => {
                        // Keep what works; `check` tries again later.
                        running.fallback = true;
                        match (&self.wanted, problem) {
                            (Some(name), Problem::Missing) => {
                                Some(fallback_message(self.dir, name))
                            }
                            (None, Problem::Missing) => None,
                            _ => Some(problem.message(self.dir)),
                        }
                    }
                    None => {
                        self.send(None);
                        Some(problem.message(self.dir))
                    }
                };
                self.set_error(note);
            }
        }
    }

    fn try_open(&self, allow_default: bool) -> Result<(Running, Feed, String), Problem> {
        let (dir, what) = (self.dir, self.what());
        let host = cpal::default_host();
        let mut fallback = false;
        let device = match &self.wanted {
            Some(name) => find_device(&host, dir, name).or_else(|| {
                if !allow_default {
                    return None;
                }
                log::debug!("{what} {name:?} not found; using the default");
                fallback = true;
                default_device(&host, dir)
            }),
            None => default_device(&host, dir),
        };
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
        let health = Arc::new(AtomicU8::new(Health::Ok as u8));
        let h = health.clone();
        let (stream, feed) = match dir {
            Dir::Input => {
                let (producer, consumer) = rtrb::RingBuffer::new(samples);
                let stream = with_sample_type!(format, input_stream(&device, config, producer, h))
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
                let stream =
                    with_sample_type!(format, output_stream(&device, config, consumer, h, shared))
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
        let running = Running {
            _stream: stream,
            health,
            fallback,
            since: Instant::now(),
        };
        let label = format!("{name} ({rate} Hz, {channels} ch, {format:?})");
        Ok((running, feed, label))
    }

    /// The device we want is plugged in (the default always counts).
    fn wanted_present(&self) -> bool {
        let dir = self.dir;
        self.wanted.as_deref().is_none_or(|name| {
            catch_unwind(|| find_device(&cpal::default_host(), dir, name).is_some())
                .unwrap_or(false)
        })
    }

    /// Periodic health check: replaces dead streams, follows Windows to a
    /// new default device, retries missing devices, and switches to the
    /// chosen device once it can.
    fn check(&mut self) {
        let due = self.last_attempt.elapsed() >= RETRY_EVERY;
        let Some(running) = &self.running else {
            if due {
                self.open(true);
            }
            return;
        };
        match Health::load(&running.health) {
            Health::Failed => {
                log::warn!("the {} stopped working", self.what());
                let worked_a_while = running.since.elapsed() >= RETRY_EVERY;
                self.close();
                if worked_a_while {
                    // Usually a device that was just unplugged: the default
                    // (or the same device again) takes over right away.
                    self.open(true);
                } else {
                    // Failing again and again: wait a little between tries.
                    self.set_error(Some(Problem::Stopped.message(self.dir)));
                }
            }
            Health::Moved => {
                log::info!("Windows changed the default {}; switching", self.what());
                // (A failure reported meanwhile is kept.)
                let _ = running.health.compare_exchange(
                    Health::Moved as u8,
                    Health::Ok as u8,
                    Relaxed,
                    Relaxed,
                );
                self.open(true);
            }
            Health::Ok if due && running.fallback => {
                self.last_attempt = Instant::now();
                if self.wanted_present() {
                    self.open(false);
                }
            }
            Health::Ok => {}
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
                output.open(true);
                input.open(true);
                let _ = ready_tx.send(());
                loop {
                    match rx.recv_timeout(CHECK_EVERY) {
                        Ok(Cmd::Input(name)) => {
                            input.wanted = name;
                            input.open(true);
                        }
                        Ok(Cmd::Output(name)) => {
                            output.wanted = name;
                            output.open(true);
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
                // No made-up "Settings" section, and only characters the
                // UI's font has ("→" draws as a box).
                assert!(!m.contains(" in Settings"), "{m}");
                assert!(!m.contains('→'), "{m}");
            }
            assert!(fallback_message(dir, "USB Headset").contains("“USB Headset”"));
        }
        assert_eq!(Problem::Missing.message(Dir::Input), NO_MIC);
        assert_eq!(Problem::Missing.message(Dir::Output), NO_SPEAKERS);
        let e = cpal::Error::new(ErrorKind::PermissionDenied);
        assert_eq!(Problem::from_error(&e), Problem::Blocked);
    }

    #[test]
    fn stream_errors() {
        assert_eq!(Health::after(ErrorKind::Xrun), Health::Ok);
        assert_eq!(Health::after(ErrorKind::RealtimeDenied), Health::Ok);
        assert_eq!(Health::after(ErrorKind::DeviceNotAvailable), Health::Failed);
        assert_eq!(Health::after(ErrorKind::BackendError), Health::Failed);
        // Windows' default device changed: switch quietly.
        assert_eq!(Health::after(ErrorKind::StreamInvalidated), Health::Moved);
        // The worst report wins, whatever the order.
        let health = Arc::new(AtomicU8::new(Health::Ok as u8));
        let mut report = error_callback(health.clone());
        report(ErrorKind::Xrun.into());
        assert_eq!(Health::load(&health), Health::Ok);
        report(ErrorKind::StreamInvalidated.into());
        assert_eq!(Health::load(&health), Health::Moved);
        report(ErrorKind::DeviceNotAvailable.into());
        report(ErrorKind::StreamInvalidated.into());
        report(ErrorKind::Xrun.into());
        assert_eq!(Health::load(&health), Health::Failed);
    }
}
