//! The viewer process (mpv, or an FFmpeg null sink in dev/test mode) and
//! the thread that feeds it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use gf_proto::bitstream::{is_decodable_start, viewer_prologue, write_viewer_frame};
use gf_proto::{VideoCodec, VideoFrame};

use crate::args;
use crate::proc::{self, StderrTail};
use crate::{tools, Tools};

/// Frames waiting for the viewer. Small on purpose: more would only add delay.
const QUEUE: usize = 6;

pub(crate) struct ViewerInner {
    codec: VideoCodec,
    open: Arc<AtomicBool>,
    /// Set by `stop()` so the watcher knows the exit was intended.
    stopping: Arc<AtomicBool>,
    /// Skip frames until the next decodable keyframe.
    need_key: AtomicBool,
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
        let mut cmd = Command::new(program);
        cmd.args(a)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
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
        let stderr = StderrTail::spawn(name, child.stderr.take());
        let stdin = child.stdin.take();
        let child = Arc::new(Mutex::new(child));
        let open = Arc::new(AtomicBool::new(true));
        let stopping = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(QUEUE);

        let spawned = (|| -> std::io::Result<(JoinHandle<()>, JoinHandle<()>)> {
            let writer = {
                let (rx, open) = (rx.clone(), open.clone());
                std::thread::Builder::new()
                    .name("viewer-writer".into())
                    .spawn(move || write_frames(stdin, codec, &rx, &open))?
            };
            let watcher = {
                let (child, open, stopping) = (child.clone(), open.clone(), stopping.clone());
                std::thread::Builder::new()
                    .name("viewer-watch".into())
                    .spawn(move || watch(&child, &open, &stopping, stderr, name))?
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
        let startable = || frame.keyframe && is_decodable_start(self.codec, &frame.data);
        if self.need_key.load(Ordering::Relaxed) {
            if !startable() {
                return true;
            }
            self.need_key.store(false, Ordering::Relaxed);
        }
        match tx.try_send(frame.data.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(data)) => {
                log::debug!("viewer is behind; skipping to the next keyframe");
                while self.rx.try_recv().is_ok() {}
                if startable() {
                    let _ = tx.try_send(data);
                } else {
                    self.need_key.store(true, Ordering::Relaxed);
                }
            }
            Err(TrySendError::Disconnected(_)) => self.open.store(false, Ordering::Relaxed),
        }
        self.is_open()
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

fn write_frames(
    stdin: Option<ChildStdin>,
    codec: VideoCodec,
    rx: &Receiver<Vec<u8>>,
    open: &AtomicBool,
) {
    let Some(mut stdin) = stdin else {
        open.store(false, Ordering::Relaxed);
        return;
    };
    // The prologue goes out together with the first frame.
    let mut buf = viewer_prologue(codec);
    let mut written = 0u64;
    while let Ok(frame) = rx.recv() {
        write_viewer_frame(codec, &frame, &mut buf);
        if let Err(e) = stdin.write_all(&buf).and_then(|()| stdin.flush()) {
            if open.swap(false, Ordering::Relaxed) {
                log::info!("viewer closed its input ({e})");
            }
            break;
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
    stderr: StderrTail,
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
    let lines = stderr.finish(Duration::from_secs(1));
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
    use std::time::Instant;

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
        let dir = std::env::temp_dir().join(format!("gf-video-mpv-{}", std::process::id()));
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
        let _ = std::fs::remove_dir_all(&dir);
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
        // The writer is now stuck on the 1 MB frame.
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
