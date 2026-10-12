//! The FFmpeg capture+encode process: start-up ladder, stdout demuxing and
//! turning failures into plain-language events.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use gf_proto::bitstream::{DemuxedFrame, FlvDemuxer};
use gf_proto::{VideoCodec, VideoFrame};
use gf_sys::WindowState;

use crate::args::{self, Screen, Step};
use crate::proc::{self, StderrTail};
use crate::{tools, EncoderEvent, EncoderSettings, Tools};

/// How long a hardware ladder step may take to produce its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// The test pattern has no fallback, so give slow machines (CI) more time.
const TEST_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
/// Events buffered for the app. If it stops reading, frames are dropped
/// (then skipped until the next keyframe) rather than piling up.
const EVENT_QUEUE: usize = 120;
/// Bytes kept from the start of the stream to read the FLV metadata.
const META_BYTES: usize = 4096;

/// Set once scaling/turning on the GPU has failed where the CPU way then
/// worked: later shares in this run of the app go straight to the CPU.
static GPU_TRANSFORM_FAILED: AtomicBool = AtomicBool::new(false);

/// The monitor `index`, or an upright 1920×1080 if it can't be found.
pub(crate) fn screen(index: u32) -> Screen {
    match gf_sys::list_monitors()
        .into_iter()
        .find(|m| m.index == index)
    {
        Some(m) if m.width > 0 && m.height > 0 => Screen {
            width: m.width,
            height: m.height,
            rotation: m.rotation,
            window: None,
        },
        _ => {
            log::warn!("monitor {index} not found; assuming 1920x1080");
            Screen {
                width: 1920,
                height: 1080,
                rotation: 0,
                window: None,
            }
        }
    }
}

/// The front-most window of the program `exe` (as the "Only one program"
/// list finds it), at the fixed size it is shared at.
fn window_screen(exe: &str, max_height: Option<u32>) -> anyhow::Result<Screen> {
    let Some(app) = gf_sys::list_audio_apps()
        .into_iter()
        .find(|a| a.exe.eq_ignore_ascii_case(exe))
    else {
        anyhow::bail!(
            "{exe} isn't running (or has no open window). Start it, then click Start sharing."
        );
    };
    match gf_sys::window_state(app.hwnd) {
        WindowState::Open { width, height } => {
            let (width, height) = args::window_output_size(width, height, max_height);
            log::info!("sharing {exe}'s window {:#x} \"{}\"", app.hwnd, app.title);
            Ok(Screen {
                width,
                height,
                rotation: 0,
                window: Some(app.hwnd),
            })
        }
        state => anyhow::bail!("{}", window_advice(exe, state, false).unwrap_or_default()),
    }
}

/// What the shared window's state explains, in plain words: closed,
/// minimized (gfxcapture sends nothing then; at start it also can't tell
/// the size), or open but no picture came (seen as likely for a game in
/// exclusive fullscreen; not tested with one).
fn window_advice(exe: &str, state: WindowState, no_picture: bool) -> Option<String> {
    match state {
        WindowState::Gone => Some(format!(
            "{exe}'s window was closed, so sharing stopped. Start {exe} again, then click \
             Start sharing."
        )),
        WindowState::Minimized => Some(format!(
            "{exe}'s window is minimized. Restore it, then click Start sharing."
        )),
        WindowState::Open { .. } if no_picture => Some(format!(
            "No picture came from {exe}'s window. If it's a game in exclusive fullscreen, \
             switch it to borderless windowed mode, or share the whole monitor instead."
        )),
        WindowState::Open { .. } => None,
    }
}

/// Size of the monitor `index` as shown on the desktop (upright).
pub(crate) fn monitor_size(index: u32) -> (u32, u32) {
    screen(index).size()
}

/// Messages from a reader thread to the supervisor.
enum Internal {
    /// Stream size from the FLV metadata, sent before the first frame if found.
    Size(u32, u32),
    Frame(DemuxedFrame, u64),
    /// stdout closed (`None`) or the FLV couldn't be parsed.
    Ended(Option<String>),
    Stop,
}

struct Attempt {
    child: Child,
    stderr: StderrTail,
    reader: Option<JoinHandle<()>>,
}

type SharedChild = Arc<Mutex<Option<Attempt>>>;

pub(crate) struct EncoderInner {
    current: SharedChild,
    internal: Sender<Internal>,
    supervisor: Option<JoinHandle<()>>,
    pub(crate) window: Option<u64>,
}

fn spawn_attempt(
    tools: &Tools,
    step: Step,
    settings: &EncoderSettings,
    screen: Screen,
    internal: &Sender<Internal>,
) -> anyhow::Result<Attempt> {
    let a = args::encoder_args(step, settings, screen);
    log::info!("starting FFmpeg ({}): {}", step.label(), a.join(" "));
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.args(&a)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    gf_sys::hide_console_window(&mut cmd);
    let mut child = gf_sys::spawn_bound(&mut cmd).map_err(|e| {
        anyhow::anyhow!(
            "Couldn't start FFmpeg ({e}). Extract the whole GloorpFlorp zip again, \
             then restart GloorpFlorp."
        )
    })?;
    let stderr = StderrTail::spawn("ffmpeg", child.stderr.take());
    let stdout = child.stdout.take();
    let tx = internal.clone();
    let reader = std::thread::Builder::new()
        .name("ffmpeg-stdout".into())
        .spawn(move || match stdout {
            Some(out) => read_flv(out, &tx),
            None => {
                let _ = tx.send(Internal::Ended(None));
            }
        });
    let reader = match reader {
        Ok(r) => Some(r),
        Err(e) => {
            proc::kill(&mut child);
            anyhow::bail!("Couldn't start screen sharing ({e}). Restart GloorpFlorp.");
        }
    };
    Ok(Attempt {
        child,
        stderr,
        reader,
    })
}

fn read_flv(mut out: impl Read, tx: &Sender<Internal>) {
    let mut demuxer = FlvDemuxer::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut head = Vec::new();
    let mut want_meta = true;
    let end = loop {
        let n = match out.read(&mut buf) {
            Ok(0) => break None,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::debug!("ffmpeg stdout read error: {e}");
                break None;
            }
        };
        if want_meta {
            head.extend_from_slice(&buf[..n.min(META_BYTES - head.len())]);
            if let Some((w, h)) = args::flv_metadata_size(&head) {
                want_meta = false;
                let _ = tx.send(Internal::Size(w, h));
            } else if head.len() >= META_BYTES {
                want_meta = false;
            }
        }
        match demuxer.push(&buf[..n]) {
            Ok(frames) => {
                let now = crate::clock_us();
                for f in frames {
                    want_meta = false;
                    if tx.send(Internal::Frame(f, now)).is_err() {
                        return;
                    }
                }
            }
            Err(e) => break Some(e.to_string()),
        }
    };
    let _ = tx.send(Internal::Ended(end));
}

/// Stops the attempt's process and collects its last stderr lines.
fn finish_attempt(
    current: &SharedChild,
) -> Option<(Option<std::process::ExitStatus>, Vec<String>)> {
    let mut attempt = current.lock().ok()?.take()?;
    let status = proc::kill(&mut attempt.child);
    if let Some(r) = attempt.reader.take() {
        let _ = r.join();
    }
    let lines = attempt.stderr.finish(Duration::from_secs(1));
    Some((status, lines))
}

/// Forwards events to the app without ever blocking on it.
struct Out {
    tx: Sender<EncoderEvent>,
    skipping: bool,
}

impl Out {
    fn frame(&mut self, frame: VideoFrame) {
        if self.skipping && !frame.keyframe {
            return;
        }
        match self.tx.try_send(EncoderEvent::Frame(frame)) {
            Ok(()) => self.skipping = false,
            Err(TrySendError::Full(_)) => {
                if !self.skipping {
                    log::warn!(
                        "video frames aren't being picked up; skipping to the next keyframe"
                    );
                }
                self.skipping = true;
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn control(&self, ev: EncoderEvent) {
        let _ = self.tx.send_timeout(ev, Duration::from_secs(2));
    }
}

impl EncoderInner {
    pub(crate) fn start(
        tools: &Tools,
        settings: &EncoderSettings,
    ) -> anyhow::Result<(EncoderInner, Receiver<EncoderEvent>)> {
        let screen = match &settings.window_exe {
            Some(exe) => window_screen(exe, settings.max_height)?,
            None => screen(settings.monitor),
        };
        let size = args::output_size(screen.width, screen.height, settings.max_height);
        let test_source = tools::use_test_source();
        let transform = screen.needs_transform(settings.max_height);
        let gpu_transform = if GPU_TRANSFORM_FAILED.load(Ordering::Relaxed) {
            if transform && !test_source {
                log::info!("scaling on the CPU (the GPU way failed earlier in this session)");
            }
            None
        } else {
            args::gpu_transform_step(screen.turned(), args::CUDA_TRANSFORM)
        };
        let steps = args::ladder(test_source, transform, gpu_transform);
        let (internal_tx, internal_rx) = crossbeam_channel::unbounded();
        let (tx, rx) = crossbeam_channel::bounded(EVENT_QUEUE);

        // Spawn the first step here so a missing/broken FFmpeg is reported at once.
        let first = spawn_attempt(tools, steps[0], settings, screen, &internal_tx)?;
        let current: SharedChild = Arc::new(Mutex::new(Some(first)));

        let sup = Supervisor {
            tools: tools.clone(),
            settings: settings.clone(),
            screen,
            size,
            steps,
            timeout: if test_source {
                TEST_FIRST_FRAME_TIMEOUT
            } else {
                FIRST_FRAME_TIMEOUT
            },
            current: current.clone(),
            internal_tx: internal_tx.clone(),
            internal_rx,
            out: Out {
                tx,
                skipping: false,
            },
            transform,
            gpu_transform_failed: false,
        };
        let supervisor = std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || sup.run());
        let supervisor = match supervisor {
            Ok(s) => Some(s),
            Err(e) => {
                finish_attempt(&current);
                anyhow::bail!("Couldn't start screen sharing ({e}). Restart GloorpFlorp.");
            }
        };
        Ok((
            EncoderInner {
                current,
                internal: internal_tx,
                supervisor,
                window: screen.window,
            },
            rx,
        ))
    }

    pub(crate) fn stop(&mut self) {
        let Some(sup) = self.supervisor.take() else {
            return;
        };
        let _ = self.internal.send(Internal::Stop);
        // Kill right away so a reader blocked on stdout wakes up.
        if let Ok(mut cur) = self.current.lock() {
            if let Some(a) = cur.as_mut() {
                let _ = a.child.kill();
            }
        }
        let _ = sup.join();
        finish_attempt(&self.current);
        log::info!("screen sharing stopped");
    }
}

impl Drop for EncoderInner {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Supervisor {
    tools: Tools,
    settings: EncoderSettings,
    screen: Screen,
    size: (u32, u32),
    steps: Vec<Step>,
    timeout: Duration,
    current: SharedChild,
    internal_tx: Sender<Internal>,
    internal_rx: Receiver<Internal>,
    out: Out,
    /// The picture is scaled or turned (the first step does it on the GPU).
    transform: bool,
    /// The GPU scaling/turning step failed to start in this ladder.
    gpu_transform_failed: bool,
}

enum StepResult {
    Stopped,
    /// Failed before the first frame; try the next step.
    NoStart(Vec<String>),
    /// Failed after frames were flowing.
    Died(Vec<String>),
}

impl Supervisor {
    fn run(mut self) {
        let codec = self.settings.codec;
        let mut failures: Vec<(Step, Vec<String>)> = Vec::new();
        for i in 0..self.steps.len() {
            let step = self.steps[i];
            if i > 0 {
                match spawn_attempt(
                    &self.tools,
                    step,
                    &self.settings,
                    self.screen,
                    &self.internal_tx,
                ) {
                    Ok(a) => {
                        if let Ok(mut cur) = self.current.lock() {
                            *cur = Some(a);
                        }
                    }
                    Err(e) => {
                        self.out.control(EncoderEvent::Stopped {
                            error: Some(e.to_string()),
                        });
                        return;
                    }
                }
            }
            match self.run_step(step) {
                StepResult::Stopped => {
                    self.out.control(EncoderEvent::Stopped { error: None });
                    return;
                }
                StepResult::NoStart(lines) => {
                    log::warn!(
                        "FFmpeg step '{}' didn't start: {}",
                        step.label(),
                        lines.last().map(String::as_str).unwrap_or("(no output)")
                    );
                    self.gpu_transform_failed |= self.transform && step != Step::Cpu;
                    failures.push((step, lines));
                }
                StepResult::Died(lines) => {
                    // A closed window ends gfxcapture silently (exit code 0).
                    let msg = self.with_window_advice(args::error_message(&lines, codec), false);
                    log::warn!("screen sharing stopped: {msg}");
                    self.out.control(EncoderEvent::Stopped { error: Some(msg) });
                    return;
                }
            }
        }
        let no_picture = failures
            .iter()
            .any(|(_, l)| l.iter().any(|x| x.starts_with("(no picture within")));
        let msg = self.with_window_advice(ladder_failure_message(&failures, codec), no_picture);
        log::warn!("screen sharing couldn't start: {msg}");
        self.out.control(EncoderEvent::Stopped { error: Some(msg) });
    }

    /// For a window share, puts what the window's state explains first.
    fn with_window_advice(&self, msg: String, no_picture: bool) -> String {
        let (Some(exe), Some(hwnd)) = (&self.settings.window_exe, self.screen.window) else {
            return msg;
        };
        match window_advice(exe, gf_sys::window_state(hwnd), no_picture) {
            Some(advice) => format!("{advice}\n\n{msg}"),
            None => msg,
        }
    }

    fn run_step(&mut self, step: Step) -> StepResult {
        let deadline = Instant::now() + self.timeout;
        let mut size = self.size;
        let mut started = false;
        loop {
            let msg = if started {
                self.internal_rx.recv().ok()
            } else {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.internal_rx.recv_timeout(left) {
                    Ok(m) => Some(m),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        let mut lines = finish_attempt(&self.current)
                            .map(|f| f.1)
                            .unwrap_or_default();
                        // Drop what the killed attempt's reader sent last, keeping a stop request.
                        while let Ok(m) = self.internal_rx.try_recv() {
                            if matches!(m, Internal::Stop) {
                                return StepResult::Stopped;
                            }
                        }
                        lines.push(format!("(no picture within {} s)", self.timeout.as_secs()));
                        return StepResult::NoStart(lines);
                    }
                    Err(_) => None,
                }
            };
            match msg {
                Some(Internal::Stop) | None => return StepResult::Stopped,
                Some(Internal::Size(w, h)) => {
                    if (w, h) != size {
                        log::info!("FFmpeg reports {w}x{h} (expected {}x{})", size.0, size.1);
                    }
                    size = (w, h);
                }
                Some(Internal::Frame(f, sent_us)) => {
                    if !started {
                        started = true;
                        if self.gpu_transform_failed && step == Step::Cpu {
                            GPU_TRANSFORM_FAILED.store(true, Ordering::Relaxed);
                        }
                        log::info!(
                            "screen sharing started: {} {}x{} @ {} fps via {}",
                            f.codec.label(),
                            size.0,
                            size.1,
                            self.settings.fps,
                            step.label()
                        );
                        self.out.control(EncoderEvent::Started {
                            codec: f.codec,
                            width: size.0,
                            height: size.1,
                            fps: self.settings.fps,
                        });
                    }
                    self.out.frame(VideoFrame {
                        codec: f.codec,
                        keyframe: f.keyframe,
                        sent_us,
                        data: f.data,
                    });
                }
                Some(Internal::Ended(demux_err)) => {
                    // A Stop may already be queued behind the end of stdout.
                    if matches!(self.internal_rx.try_recv(), Ok(Internal::Stop)) {
                        return StepResult::Stopped;
                    }
                    let (status, mut lines) =
                        finish_attempt(&self.current).unwrap_or((None, Vec::new()));
                    if let Some(e) = demux_err {
                        lines.push(format!("(couldn't read FFmpeg's output: {e})"));
                    }
                    if let Some(s) = status {
                        log::debug!("FFmpeg exited: {s}");
                    }
                    return if started {
                        StepResult::Died(lines)
                    } else {
                        StepResult::NoStart(lines)
                    };
                }
            }
        }
    }
}

/// Advice based on everything every step printed, then the details of each.
fn ladder_failure_message(failures: &[(Step, Vec<String>)], codec: VideoCodec) -> String {
    let all: Vec<String> = failures
        .iter()
        .flat_map(|(_, l)| l.iter().cloned())
        .collect();
    let advice = args::advice(&all.join("\n"), codec);
    let mut details = String::new();
    for (step, lines) in failures {
        details.push_str(&format!("\n[{}]\n", step.label()));
        if lines.is_empty() {
            details.push_str("(no output)\n");
        }
        for l in lines.iter().rev().take(12).rev() {
            details.push_str(l);
            details.push('\n');
        }
    }
    format!("{advice}\n\nDetails from FFmpeg:{}", details.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_message_combines_steps() {
        let failures = vec![
            (
                Step::GpuDirect,
                vec!["No such filter: 'scale_d3d11'".to_string()],
            ),
            (
                Step::Cpu,
                vec!["OpenEncodeSessionEx failed: out of memory (10)".to_string()],
            ),
        ];
        let m = ladder_failure_message(&failures, VideoCodec::Hevc);
        assert!(
            m.starts_with("The NVIDIA video encoder couldn't start for HEVC"),
            "{m}"
        );
        assert!(m.contains("[GPU (D3D11 -> NVENC)]\nNo such filter"));
        assert!(m.ends_with("OpenEncodeSessionEx failed: out of memory (10)"));
        let m = ladder_failure_message(&[(Step::TestSource, vec![])], VideoCodec::H264);
        assert!(m.contains("(no output)"));
    }
}
