//! Video: FFmpeg captures the screen (`ddagrab`, Desktop Duplication, frames
//! stay on the GPU) and encodes it with NVENC; mpv decodes on the GPU and
//! shows it in its own window. Both are bundled programs that this crate
//! starts as child processes and talks to through pipes:
//!
//! ```text
//! sharer:  ffmpeg (ddagrab -> *_nvenc -> FLV) --stdout--> FlvDemuxer --> VideoFrame
//! viewer:  VideoFrame --> writer thread --stdin--> mpv (raw stream, hw decode, low-latency)
//! ```
//!
//! Non-Windows (development and tests only), or Windows with
//! `GLOORP_FAKE_VIDEO=1`: the encoder uses FFmpeg's `testsrc2` pattern and
//! software encoders (libx264 / libx265 / libsvtav1), and the viewer runs
//! `mpv` if it exists (and, off Windows, a display does), else
//! `ffmpeg -f <fmt> -i pipe:0 -f null -` as a sink.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use crossbeam_channel::Receiver;
use gf_proto::{VideoCodec, VideoFrame};

mod args;
mod encoder;
mod proc;
mod tools;
mod viewer;

pub use args::output_size;

/// Microseconds on a process-wide monotonic clock (the clock behind
/// `VideoFrame::sent_us` for frames produced by [`Encoder`]).
pub fn clock_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Size of monitor `index` (`gf_sys::Monitor::index`) as the encoder sees it;
/// 1920×1080 if it can't be found. Together with [`output_size`] this gives
/// the stream size before `EncoderEvent::Started` arrives.
pub fn monitor_size(index: u32) -> (u32, u32) {
    encoder::monitor_size(index)
}

/// Paths to the bundled programs.
#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub mpv: Option<PathBuf>,
}

impl Tools {
    /// Looks next to the running exe first (`ffmpeg/ffmpeg.exe`,
    /// `ffmpeg.exe`, `mpv/mpv.exe`, `mpv.exe`), then on PATH. Errors with a
    /// plain-language message if FFmpeg is missing. mpv being missing is not
    /// an error here (only viewing needs it).
    pub fn locate() -> anyhow::Result<Tools> {
        tools::locate()
    }
}

/// Which NVENC encoders actually work on this PC, best first. Runs a tiny
/// test encode for each codec (a few seconds total, call off the UI thread).
/// Non-Windows: the software encoders available in the local FFmpeg.
///
/// Each probe encodes 3 frames of 640×360 black and is killed after 8 s.
pub fn probe_encoders(tools: &Tools) -> Vec<VideoCodec> {
    tools::probe_encoders(tools)
}

#[derive(Clone, Debug, PartialEq)]
pub struct EncoderSettings {
    pub codec: VideoCodec,
    /// `gf_sys::Monitor::index`.
    pub monitor: u32,
    pub fps: u32,
    /// Scale down (keeping aspect ratio) if the monitor is taller than this.
    pub max_height: Option<u32>,
    pub bitrate_kbps: u32,
    /// Keyframe every N frames (default 2 s worth). A viewer that opens its
    /// window mid-stream waits at most this long for a picture.
    pub keyframe_interval: u32,
}

#[derive(Clone, Debug)]
pub enum EncoderEvent {
    /// First frame produced; this is the actual output size.
    Started {
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
    },
    Frame(VideoFrame),
    /// The encoder exited. `error` is a plain-language explanation followed by
    /// the last lines FFmpeg printed, or `None` after a normal `stop()`.
    Stopped {
        error: Option<String>,
    },
}

/// A running FFmpeg capture+encode process. Stops (kills FFmpeg) when dropped.
///
/// Start-up tries a short ladder of FFmpeg command lines, moving to the next
/// if FFmpeg exits or produces no frame within a few seconds:
/// 1. GPU all the way: ddagrab (D3D11) straight into NVENC; when scaling is
///    needed, scale on the GPU.
/// 2. Same but scaling via CUDA (`hwmap=derive_device=cuda,scale_cuda`).
/// 3. Safe fallback: `hwdownload` to system memory, CPU scale, then NVENC.
///
/// NVENC flags aim for low latency: `-preset p4 -tune ull -rc cbr`, no
/// B-frames, `-zerolatency 1`, `-delay 0`, `-g keyframe_interval`. Output is
/// FLV on stdout (`-f flv -flvflags no_duration_filesize -flush_packets 1`)
/// parsed by `gf_proto::bitstream::FlvDemuxer`.
///
/// The monitor size comes from `gf_sys::list_monitors()` (1920×1080 if the
/// monitor isn't listed). `start` returns once the first FFmpeg process is
/// running; the ladder continues in the background. Frames are dropped
/// (until the next keyframe) if the receiver isn't read fast enough.
/// Non-Windows, or with `GLOORP_FAKE_VIDEO=1`: `testsrc2` + software encoder.
pub struct Encoder {
    inner: encoder::EncoderInner,
}

impl Encoder {
    pub fn start(
        tools: &Tools,
        settings: &EncoderSettings,
    ) -> anyhow::Result<(Encoder, Receiver<EncoderEvent>)> {
        let (inner, rx) = encoder::EncoderInner::start(tools, settings)?;
        Ok((Encoder { inner }, rx))
    }

    /// Kills FFmpeg and waits for it; the last event is `Stopped { error: None }`
    /// (unless the encoder had already stopped by itself).
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}

/// The viewer window (mpv), fed through stdin. Stops when dropped.
///
/// mpv runs with low-latency options (`--profile=low-latency --untimed
/// --no-cache`, tiny probe size, the right raw demuxer for the codec:
/// `h264` / `hevc` / `obu`), fed with `gf_proto::bitstream::viewer_prologue`
/// and `write_viewer_frame`,
/// hardware decoding (`--hwdec=auto-safe`), no audio, a resizable window
/// (double-click or `f` for fullscreen), titled with the friend's name.
///
/// Frames are handed to a writer thread through a small queue. If mpv falls
/// behind (queue full) the queue is cleared and frames are skipped until the
/// next keyframe. Frames before the first keyframe are skipped.
///
/// Non-Windows without a display (or with `GLOORP_FAKE_VIDEO=1` and no mpv):
/// `ffmpeg -f <fmt> -i pipe:0 -f null -` stands in for mpv.
pub struct Viewer {
    inner: viewer::ViewerInner,
}

impl Viewer {
    pub fn start(tools: &Tools, codec: VideoCodec, title: &str) -> anyhow::Result<Viewer> {
        viewer::ViewerInner::start(tools, codec, title).map(|inner| Viewer { inner })
    }

    /// Never blocks. Returns false once the window has been closed.
    pub fn push(&self, frame: &VideoFrame) -> bool {
        self.inner.push(frame)
    }

    /// False after the user closed the window or mpv exited.
    pub fn is_open(&self) -> bool {
        self.inner.is_open()
    }

    /// Closes the window (kills mpv) and waits for it.
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}
