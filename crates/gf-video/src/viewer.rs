//! The viewer process (mpv, or an FFmpeg null sink in dev/test mode) and
//! the thread that feeds it.

use std::io::Write;
use std::path::PathBuf;
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
fn choose_program(tools: &Tools, codec: VideoCodec, title: &str) -> anyhow::Result<(PathBuf, Vec<String>, bool)> {
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
    pub(crate) fn start(tools: &Tools, codec: VideoCodec, title: &str) -> anyhow::Result<ViewerInner> {
        let title = args::clean_title(title);
        let (program, a, is_mpv) = choose_program(tools, codec, &title)?;
        let name = if is_mpv { "mpv" } else { "ffmpeg-sink" };
        log::info!("opening viewer ({name}) for {}: {}", codec.label(), a.join(" "));
        let mut cmd = Command::new(&program);
        cmd.args(&a)
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
            log::trace!("viewer skipped a {} frame (expects {})", frame.codec.label(), self.codec.label());
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
        for t in [self.writer.take(), self.watcher.take()].into_iter().flatten() {
            let _ = t.join();
        }
    }
}

impl Drop for ViewerInner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_frames(stdin: Option<ChildStdin>, codec: VideoCodec, rx: &Receiver<Vec<u8>>, open: &AtomicBool) {
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

fn watch(child: &Mutex<Child>, open: &AtomicBool, stopping: &AtomicBool, stderr: StderrTail, name: &str) {
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
