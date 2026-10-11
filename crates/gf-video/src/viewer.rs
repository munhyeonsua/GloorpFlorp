//! The viewer process (mpv, or an FFmpeg null sink in dev/test mode) and
//! the thread that feeds it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use gf_proto::bitstream::{is_decodable_start, viewer_prologue, write_viewer_frame};
use gf_proto::{VideoCodec, VideoFrame};

use crate::args;
use crate::proc::{self, StderrTail};
use crate::{tools, Tools};

/// Frames waiting for the viewer. Small on purpose: more would only add delay.
const QUEUE: usize = 6;
/// Until the viewer reads its input and has caught up, every frame from the
/// latest keyframe on is kept (mpv takes ~0.6 s to start on Windows), up to
/// this many frames or bytes. mpv then plays the backlog at once
/// (`--untimed`).
const STARTUP_QUEUE: usize = 300;
const STARTUP_BYTES: usize = 16 << 20;
/// A pipe takes ~64 KB without anyone reading it, so the viewer only
/// surely reads once more than this went through; or once writes have kept
/// going for `READING_AFTER` (a quiet screen makes tiny frames).
const PIPE_SLACK: u64 = 256 << 10;
const READING_AFTER: Duration = Duration::from_secs(3);

/// Whether the viewer reads its input, judged by the writer.
fn is_reading(bytes_written: u64, since_first_write: Duration) -> bool {
    bytes_written > PIPE_SLACK || since_first_write >= READING_AFTER
}

pub(crate) struct ViewerInner {
    codec: VideoCodec,
    open: Arc<AtomicBool>,
    /// Set by `stop()` so the watcher knows the exit was intended.
    stopping: Arc<AtomicBool>,
    /// Skip frames until the next decodable keyframe.
    need_key: AtomicBool,
    /// Set by the writer once the viewer reads its input (see `is_reading`).
    reading: Arc<AtomicBool>,
    /// The start-up backlog is gone: from now on `QUEUE` waiting frames
    /// mean the viewer is behind.
    caught_up: AtomicBool,
    /// Bytes waiting in the queue.
    queued_bytes: Arc<AtomicUsize>,
    tx: Option<Sender<Vec<u8>>>,
    /// Lets `push` clear the queue when the viewer falls behind.
    rx: Receiver<Vec<u8>>,
    child: Arc<Mutex<Child>>,
    writer: Option<JoinHandle<()>>,
    watcher: Option<JoinHandle<()>>,
}

/// mpv on Windows; elsewhere mpv only when there is a display, else the
/// FFmpeg sink (also used on Windows in test mode without mpv).
fn choose_program(
    tools: &Tools,
    codec: VideoCodec,
    title: &str,
) -> anyhow::Result<(PathBuf, Vec<String>, bool)> {
    let has_display = cfg!(windows)
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some();
    match &tools.mpv {
        Some(mpv) if has_display => Ok((mpv.clone(), args::mpv_args(codec, title), true)),
        _ if !cfg!(windows) || tools::use_test_source() => {
            Ok((tools.ffmpeg.clone(), args::null_sink_args(codec), false))
        }
        _ => anyhow::bail!(
            "Couldn't find mpv next to GloorpFlorp.exe, so your friend's screen can't be shown. \
             Extract the whole GloorpFlorp zip again, then restart GloorpFlorp."
        ),
    }
}

impl ViewerInner {
    pub(crate) fn start(
        tools: &Tools,
        codec: VideoCodec,
        title: &str,
    ) -> anyhow::Result<ViewerInner> {
        let title = args::clean_title(title);
        let (program, a, is_mpv) = choose_program(tools, codec, &title)?;
        let name = if is_mpv { "mpv" } else { "ffmpeg-sink" };
        Self::spawn(&program, &a, codec, name)
    }

    fn spawn(
        program: &Path,
        a: &[String],
        codec: VideoCodec,
        name: &'static str,
    ) -> anyhow::Result<ViewerInner> {
        log::info!(
            "opening viewer ({name}) for {}: {}",
            codec.label(),
            a.join(" ")
        );
        let (mut child, output) = spawn_child(program, a, name)?;
        let stdin = child.stdin.take();
        let child = Arc::new(Mutex::new(child));
        let open = Arc::new(AtomicBool::new(true));
        let stopping = Arc::new(AtomicBool::new(false));
        let reading = Arc::new(AtomicBool::new(false));
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(STARTUP_QUEUE);

        let spawned = (|| -> std::io::Result<(JoinHandle<()>, JoinHandle<()>)> {
            let writer = {
                let feed = Feed {
                    rx: rx.clone(),
                    open: open.clone(),
                    reading: reading.clone(),
                    queued_bytes: queued_bytes.clone(),
                };
                std::thread::Builder::new()
                    .name("viewer-writer".into())
                    .spawn(move || write_frames(stdin, codec, &feed))?
            };
            let watcher = {
                let (child, open, stopping) = (child.clone(), open.clone(), stopping.clone());
                std::thread::Builder::new()
                    .name("viewer-watch".into())
                    .spawn(move || watch(&child, &open, &stopping, output, name))?
            };
            Ok((writer, watcher))
        })();
        let (writer, watcher) = match spawned {
            Ok(t) => t,
            Err(e) => {
                if let Ok(mut c) = child.lock() {
                    proc::kill(&mut c);
                }
                anyhow::bail!("Couldn't open the viewer window ({e}). Restart GloorpFlorp.");
            }
        };
        Ok(ViewerInner {
            codec,
            open,
            stopping,
            need_key: AtomicBool::new(true),
            reading,
            caught_up: AtomicBool::new(false),
            queued_bytes,
            tx: Some(tx),
            rx,
            child,
            writer: Some(writer),
            watcher: Some(watcher),
        })
    }

    pub(crate) fn push(&self, frame: &VideoFrame) -> bool {
        if !self.is_open() {
            return false;
        }
        let Some(tx) = &self.tx else { return false };
        if frame.codec != self.codec {
            log::trace!(
                "viewer skipped a {} frame (expects {})",
                frame.codec.label(),
                self.codec.label()
            );
            return true;
        }
        let startable = frame.keyframe && is_decodable_start(self.codec, &frame.data);
        if self.need_key.load(Ordering::Relaxed) {
            if !startable {
                return true;
            }
            self.need_key.store(false, Ordering::Relaxed);
        }
        if !self.caught_up.load(Ordering::Relaxed)
            && self.reading.load(Ordering::Relaxed)
            && tx.is_empty()
        {
            self.caught_up.store(true, Ordering::Relaxed);
        }
        let starting = !self.caught_up.load(Ordering::Relaxed);
        if starting && startable {
            // Still starting: frames before a newer keyframe aren't needed.
            self.clear();
        }
        let behind = if starting {
            tx.len() >= STARTUP_QUEUE
                || self.queued_bytes.load(Ordering::Relaxed) + frame.data.len() > STARTUP_BYTES
        } else {
            tx.len() >= QUEUE
        };
        if behind {
            self.skip_to_keyframe(startable);
            if !startable {
                return self.is_open();
            }
        }
        let len = frame.data.len();
        self.queued_bytes.fetch_add(len, Ordering::Relaxed);
        if let Err(e) = tx.try_send(frame.data.clone()) {
            self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
            match e {
                // Only if `push` runs on two threads at once.
                TrySendError::Full(_) => self.skip_to_keyframe(false),
                TrySendError::Disconnected(_) => self.open.store(false, Ordering::Relaxed),
            }
        }
        self.is_open()
    }

    /// The viewer is behind: drop what waits, then go on from `frame` if it
    /// is a keyframe it can start from, else from the next one.
    fn skip_to_keyframe(&self, startable: bool) {
        log::debug!("viewer is behind; skipping to the next keyframe");
        self.clear();
        if !startable {
            self.need_key.store(true, Ordering::Relaxed);
        }
    }

    fn clear(&self) {
        while let Ok(f) = self.rx.try_recv() {
            self.queued_bytes.fetch_sub(f.len(), Ordering::Relaxed);
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    pub(crate) fn stop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.open.store(false, Ordering::Relaxed);
        // Closing the queue ends the writer, which closes mpv's stdin.
        self.tx = None;
        if let Ok(mut c) = self.child.lock() {
            proc::kill(&mut c);
        }
        for t in [self.writer.take(), self.watcher.take()]
            .into_iter()
            .flatten()
        {
            let _ = t.join();
        }
    }
}

impl Drop for ViewerInner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the viewer program with all three pipes. mpv prints its messages
/// on stdout (on Windows), FFmpeg on stderr: the last lines of both are kept
/// for the log.
fn spawn_child(
    program: &Path,
    a: &[String],
    name: &str,
) -> anyhow::Result<(Child, [StderrTail; 2])> {
    let mut cmd = Command::new(program);
    cmd.args(a)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Harmless for GUI programs; stops a console window flashing up otherwise.
    gf_sys::hide_console_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "Couldn't open the viewer window ({e}). Extract the whole GloorpFlorp zip \
             again, then restart GloorpFlorp."
        )
    })?;
    gf_sys::bind_child_to_app_lifetime(&child);
    let output = [
        StderrTail::spawn(name, child.stdout.take()),
        StderrTail::spawn(name, child.stderr.take()),
    ];
    Ok((child, output))
}

/// What the writer thread shares with `ViewerInner`.
struct Feed {
    rx: Receiver<Vec<u8>>,
    open: Arc<AtomicBool>,
    reading: Arc<AtomicBool>,
    queued_bytes: Arc<AtomicUsize>,
}

fn write_frames(stdin: Option<ChildStdin>, codec: VideoCodec, feed: &Feed) {
    let Some(mut stdin) = stdin else {
        feed.open.store(false, Ordering::Relaxed);
        return;
    };
    // The prologue goes out together with the first frame.
    let mut buf = viewer_prologue(codec);
    let mut written = 0u64;
    let mut bytes = 0u64;
    let mut first_write = None;
    while let Ok(frame) = feed.rx.recv() {
        feed.queued_bytes.fetch_sub(frame.len(), Ordering::Relaxed);
        write_viewer_frame(codec, &frame, &mut buf);
        if let Err(e) = stdin.write_all(&buf).and_then(|()| stdin.flush()) {
            if feed.open.swap(false, Ordering::Relaxed) {
                log::info!("viewer closed its input ({e})");
            }
            break;
        }
        bytes += buf.len() as u64;
        let since = first_write.get_or_insert_with(Instant::now).elapsed();
        if is_reading(bytes, since) {
            feed.reading.store(true, Ordering::Relaxed);
        }
        buf.clear();
        written += 1;
    }
    log::debug!("viewer writer done after {written} frames");
}

fn watch(
    child: &Mutex<Child>,
    open: &AtomicBool,
    stopping: &AtomicBool,
    output: [StderrTail; 2],
    name: &str,
) {
    let status = loop {
        match child.lock().map(|mut c| c.try_wait()) {
            Ok(Ok(Some(status))) => break Some(status),
            Ok(Ok(None)) => {}
            _ => break None,
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    open.store(false, Ordering::Relaxed);
    let lines: Vec<String> = output
        .into_iter()
        .flat_map(|t| t.finish(Duration::from_secs(1)))
        .collect();
    if stopping.load(Ordering::Relaxed) {
        log::debug!("{name} stopped");
    } else {
        match status {
            Some(s) if s.success() => log::info!("viewer window closed"),
            _ => log::warn!("{name} exited ({status:?}): {}", lines.join(" | ")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(keyframe: bool, data: Vec<u8>) -> VideoFrame {
        VideoFrame {
            codec: VideoCodec::H264,
            keyframe,
            sent_us: 0,
            data,
        }
    }

    /// NAL types are all `is_decodable_start` looks at.
    fn synthetic(keyframe: bool, size: usize) -> VideoFrame {
        let mut d = Vec::with_capacity(size + 16);
        if keyframe {
            d.extend_from_slice(&[
                0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65,
            ]);
        } else {
            d.extend_from_slice(&[0, 0, 0, 1, 0x41]);
        }
        d.resize(size, 0x55);
        frame(keyframe, d)
    }

    impl ViewerInner {
        /// Ends the input like a finished stream and waits for the child.
        fn finish_input(&mut self, timeout: Duration) -> bool {
            self.tx = None;
            if let Some(w) = self.writer.take() {
                let _ = w.join();
            }
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if matches!(self.child.lock().map(|mut c| c.try_wait()), Ok(Ok(Some(_)))) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }
    }

    fn ffmpeg_with(encoder: &str) -> Option<PathBuf> {
        let tools = crate::tools::locate_in(None, std::env::var_os("PATH")).ok()?;
        let (ok, out, _) = crate::tools::run_short(
            &tools.ffmpeg,
            &["-hide_banner".into(), "-encoders".into()],
            Duration::from_secs(10),
        )?;
        (ok && out.contains(encoder)).then_some(tools.ffmpeg)
    }

    #[test]
    fn sink_decodes_frames_from_first_keyframe() {
        let Some(ffmpeg) = ffmpeg_with("libx264") else {
            println!("note: skipping, ffmpeg with libx264 is not available");
            return;
        };
        let dir = std::env::temp_dir().join(format!("gf-video-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let flv = dir.join("in.flv");
        let crc = dir.join("out.crc");
        let a: Vec<String> = [
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30",
            "-frames:v",
            "40",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-bf",
            "0",
            "-g",
            "10",
            "-f",
            "flv",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([flv.to_string_lossy().into_owned()])
        .collect();
        let (ok, _, err) = crate::tools::run_short(&ffmpeg, &a, Duration::from_secs(30)).unwrap();
        assert!(ok, "{err}");
        let frames = gf_proto::bitstream::FlvDemuxer::new()
            .push(&std::fs::read(&flv).unwrap())
            .unwrap();
        assert_eq!(frames.len(), 40);

        let sink: Vec<String> = [
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-f",
            "framecrc",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([crc.to_string_lossy().into_owned()])
        .collect();
        let mut v = ViewerInner::spawn(&ffmpeg, &sink, VideoCodec::H264, "test-sink").unwrap();
        for f in &frames[5..] {
            // Never overflow here (a slow sink start-up would skip frames).
            let deadline = Instant::now() + Duration::from_secs(20);
            while !v.rx.is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(v.push(&frame(f.keyframe, f.data.clone())));
        }
        assert!(v.finish_input(Duration::from_secs(20)));
        v.stop();
        assert!(!v.is_open());
        let out = std::fs::read_to_string(&crc).unwrap();
        let decoded = out.lines().filter(|l| !l.starts_with('#')).count();
        // Frames 5..9 come before a keyframe and are skipped.
        assert_eq!(decoded, 30, "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn encode_flv(
        ffmpeg: &Path,
        encoder: &str,
        out: &Path,
    ) -> Vec<gf_proto::bitstream::DemuxedFrame> {
        let mut a: Vec<String> = [
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30",
            "-frames:v",
            "40",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            encoder,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        a.extend(match encoder {
            "libx265" => vec![
                "-x265-params".to_string(),
                "keyint=10:min-keyint=10:bframes=0:log-level=error".into(),
            ],
            "libsvtav1" => vec![
                "-g".to_string(),
                "10".into(),
                "-svtav1-params".into(),
                "pred-struct=1".into(),
            ],
            _ => vec!["-bf".to_string(), "0".into(), "-g".into(), "10".into()],
        });
        a.extend([
            "-f".to_string(),
            "flv".into(),
            out.to_string_lossy().into_owned(),
        ]);
        let (ok, _, err) = crate::tools::run_short(ffmpeg, &a, Duration::from_secs(60)).unwrap();
        assert!(ok, "{err}");
        gf_proto::bitstream::FlvDemuxer::new()
            .push(&std::fs::read(out).unwrap())
            .unwrap()
    }

    /// The real mpv command line (with `--vo=image` instead of a window):
    /// every frame from the first keyframe is shown, and the first one
    /// without waiting for the next frame. Runs only where mpv is installed.
    #[test]
    fn mpv_shows_each_frame_at_once() {
        let Ok(tools) = crate::tools::locate_in(None, std::env::var_os("PATH")) else {
            println!("note: skipping, ffmpeg is not available");
            return;
        };
        let Some(mpv) = tools.mpv.clone() else {
            println!("note: skipping, mpv is not available");
            return;
        };
        match mpv_version(&mpv) {
            Some(v) if v >= (0, 41) => {}
            v => {
                println!("note: skipping, mpv {v:?} is older than 0.41 (no --hwdec-threads)");
                return;
            }
        }
        let dir = std::env::temp_dir().join(format!("gf-video-mpv-{}", std::process::id()));
        let _cleanup = RemoveOnDrop(dir.clone());
        for codec in VideoCodec::ALL {
            let encoder = args::software_encoder(codec);
            if ffmpeg_with(encoder).is_none() {
                println!("note: skipping {encoder}, not in this ffmpeg");
                continue;
            }
            let _ = std::fs::remove_dir_all(&dir);
            let shots = dir.join("shots");
            std::fs::create_dir_all(&shots).unwrap();
            let frames = encode_flv(&tools.ffmpeg, encoder, &dir.join("in.flv"));
            assert_eq!(frames.len(), 40);
            let mut a = args::mpv_args(codec, "test");
            a.pop();
            a.extend([
                "--vo=image".to_string(),
                // The image output can't take frames straight from a GPU
                // decoder; copying them back holds frames, which the real
                // window (zero-copy) never does. Decode on the CPU here.
                "--hwdec=no".into(),
                "--vo-image-format=png".into(),
                format!("--vo-image-outdir={}", shots.display()),
                "-".into(),
            ]);
            let count = || std::fs::read_dir(&shots).map(|d| d.count()).unwrap_or(0);
            let mut v = ViewerInner::spawn(&mpv, &a, codec, "mpv").unwrap();
            let mut pushed = 0;
            for (i, f) in frames.iter().enumerate().skip(5) {
                let vf = VideoFrame {
                    codec,
                    keyframe: f.keyframe,
                    sent_us: 0,
                    data: f.data.clone(),
                };
                assert!(v.push(&vf), "{codec:?}: mpv closed");
                if i >= 10 {
                    pushed += 1;
                }
                if i == 10 {
                    // Nothing else is written until the first picture is out.
                    let t0 = Instant::now();
                    let deadline = t0 + Duration::from_secs(10);
                    while count() == 0 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert_eq!(count(), 1, "{codec:?}: first frame not shown on its own");
                    println!("{codec:?}: first picture after {:?}", t0.elapsed());
                }
                let deadline = Instant::now() + Duration::from_secs(20);
                while !v.rx.is_empty() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            assert!(
                v.finish_input(Duration::from_secs(20)),
                "{codec:?}: mpv didn't exit at EOF"
            );
            v.stop();
            assert_eq!(count(), pushed, "{codec:?}: frames shown");
            println!("{codec:?}: {pushed} frames shown");
        }
    }

    /// Removes a test's scratch folder, also when an assertion fails.
    struct RemoveOnDrop(PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// (major, minor) from `mpv --version` ("mpv v0.41.0-..." or "mpv 0.37.0").
    fn mpv_version(mpv: &Path) -> Option<(u32, u32)> {
        let (_, out, _) =
            crate::tools::run_short(mpv, &["--version".into()], Duration::from_secs(20))?;
        let v = out.split_whitespace().nth(1)?.trim_start_matches('v');
        let mut parts = v.split(['.', '-']).map(|p| p.parse::<u32>().ok());
        Some((parts.next()??, parts.next()??))
    }

    /// mpv reports its errors on stdout (on Windows); they reach the log.
    #[test]
    fn viewer_output_is_kept() {
        let (program, a) = if cfg!(windows) {
            (
                PathBuf::from("cmd"),
                vec![
                    "/C".to_string(),
                    "echo out-line & echo err-line 1>&2".into(),
                ],
            )
        } else {
            (
                PathBuf::from("/bin/sh"),
                vec!["-c".to_string(), "echo out-line; echo err-line >&2".into()],
            )
        };
        let (mut child, output) = spawn_child(&program, &a, "echo").unwrap();
        assert!(child.wait().unwrap().success());
        let lines: Vec<String> = output
            .into_iter()
            .flat_map(|t| t.finish(Duration::from_secs(5)))
            .collect();
        assert_eq!(lines, ["out-line", "err-line"]);
    }

    /// A program that runs for 30 s and never reads stdin.
    fn idle_program() -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            let a = ["-n", "30", "127.0.0.1"].map(String::from).to_vec();
            ("ping".into(), a)
        } else {
            ("/bin/sh".into(), vec!["-c".into(), "exec sleep 30".into()])
        }
    }

    #[test]
    fn overflow_skips_to_next_keyframe() {
        // The pipe and then the queue fill up.
        let (program, a) = idle_program();
        let mut v = ViewerInner::spawn(&program, &a, VideoCodec::H264, "idle").unwrap();
        assert!(v.push(&synthetic(false, 100)));
        assert_eq!(
            v.rx.len(),
            0,
            "frames before the first keyframe are skipped"
        );
        assert!(v.push(&synthetic(true, 1 << 20)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !v.rx.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The writer is now stuck on the 1 MB frame. Behave as if the
        // viewer had been reading before (see the start-up test).
        v.reading.store(true, Ordering::Relaxed);
        for _ in 0..QUEUE {
            assert!(v.push(&synthetic(false, 100)));
        }
        assert_eq!(v.rx.len(), QUEUE);
        assert!(v.push(&synthetic(false, 100)));
        assert_eq!(v.rx.len(), 0, "overflow clears the queue");
        assert!(v.push(&synthetic(false, 100)));
        assert_eq!(v.rx.len(), 0, "then waits for a keyframe");
        assert!(v.push(&synthetic(true, 100)));
        assert_eq!(v.rx.len(), 1);
        let started = Instant::now();
        v.stop();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!v.is_open());
        assert!(!v.push(&synthetic(true, 100)));
    }

    fn wait_until(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(done(), "timed out");
    }

    #[test]
    fn reading_needs_more_than_a_pipe_full() {
        // A 60 KB keyframe fits in the pipe of an mpv that isn't reading yet.
        assert!(!is_reading(60_000, Duration::from_millis(5)));
        assert!(!is_reading(PIPE_SLACK, Duration::from_secs(2)));
        assert!(is_reading(PIPE_SLACK + 1, Duration::ZERO));
        // A quiet screen: tiny frames, but writes kept going.
        assert!(is_reading(5_000, READING_AFTER));
    }

    /// While the viewer starts (mpv: ~0.6 s), frames from the opening
    /// keyframe on wait for it instead of being dropped after 6.
    #[test]
    fn starting_viewer_keeps_frames_from_the_keyframe() {
        let (program, a) = idle_program();
        let mut v = ViewerInner::spawn(&program, &a, VideoCodec::H264, "idle").unwrap();
        // Never read: the writer is stuck on the first (big) keyframe.
        assert!(v.push(&synthetic(true, 1 << 20)));
        wait_until(|| v.rx.is_empty());
        assert!(!v.reading.load(Ordering::Relaxed));
        for _ in 0..60 {
            assert!(v.push(&synthetic(false, 1000)));
        }
        assert_eq!(v.rx.len(), 60, "a second of frames is kept");
        assert_eq!(v.queued_bytes.load(Ordering::Relaxed), 60 * 1000);

        // A newer keyframe makes the older frames unnecessary.
        assert!(v.push(&synthetic(true, 1000)));
        assert!(v.push(&synthetic(false, 1000)));
        assert_eq!(v.rx.len(), 2);
        assert!(is_decodable_start(
            VideoCodec::H264,
            &v.rx.try_recv().unwrap()
        ));
        v.queued_bytes.fetch_sub(1000, Ordering::Relaxed);

        // The start-up queue has a limit in bytes too.
        assert!(v.push(&synthetic(false, STARTUP_BYTES - 1500)));
        assert!(v.push(&synthetic(false, 1000)));
        assert!(v.rx.is_empty(), "over the limit: skip to the next keyframe");
        assert_eq!(v.queued_bytes.load(Ordering::Relaxed), 0);
        assert!(v.push(&synthetic(false, 1000)));
        assert!(v.rx.is_empty());
        assert!(v.push(&synthetic(true, 1000)));
        assert_eq!(v.rx.len(), 1);
        v.stop();
    }

    #[test]
    fn exit_of_viewer_closes_it() {
        let (program, a) = if cfg!(windows) {
            ("cmd".into(), vec!["/C".to_string(), "exit 0".into()])
        } else {
            (
                PathBuf::from("/bin/sh"),
                vec!["-c".to_string(), "exit 0".into()],
            )
        };
        let mut v = ViewerInner::spawn(&program, &a, VideoCodec::H264, "exit").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while v.is_open() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!v.is_open());
        assert!(!v.push(&synthetic(true, 100)));
        v.stop();
    }

    #[test]
    fn missing_program_is_a_plain_error() {
        let err = ViewerInner::spawn(
            Path::new("gloorpflorp-no-such-mpv"),
            &[],
            VideoCodec::Av1,
            "x",
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("Couldn't open the viewer window"), "{err}");
    }
}
