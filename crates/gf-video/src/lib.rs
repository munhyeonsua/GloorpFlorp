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

/// Size of monitor `index` (`gf_sys::Monitor::index`) as the desktop shows
/// it (upright, also for a turned monitor); 1920×1080 if it can't be found.
/// Together with [`output_size`] this gives the stream size before
/// `EncoderEvent::Started` arrives.
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
    /// Frames per second sent. The capture is paced to at most one frame
    /// per 1/`fps` s slot and fills every slot while the capture keeps up
    /// (see [`Encoder`]), also on a still screen.
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
/// 1. GPU: at native size on an upright monitor, ddagrab (D3D11) straight
///    into NVENC. When the picture is scaled or turned, the CUDA way
///    instead: `hwdownload`, `hwupload_cuda`, then `scale_cuda` /
///    `transpose_cuda` (`args::CUDA_TRANSFORM`; `scale_d3d11` isn't tried,
///    as FFmpeg 8 can't create its frames on NVIDIA cards). If this scaling
///    step fails where step 2 then works, later starts in the same run of
///    the app go straight to step 2.
/// 2. Safe fallback: `hwdownload` to system memory, CPU scale/turn/convert
///    (BT.601, tagged), then NVENC.
///
/// A monitor that Windows shows turned (`gf_sys::Monitor::rotation`) is
/// captured unturned by Desktop Duplication; the picture is turned upright,
/// so the stream size is still [`output_size`] of [`monitor_size`].
///
/// Frame pacing: ddagrab is asked for 1001 fps (`args::CAPTURE_POLL_FPS`),
/// so it never waits on the coarse Windows timer, and `args::pace_filter`
/// keeps the first frame of each 1/`fps` s slot before any copying or
/// scaling and time-stamps it in slots (time base 1/`fps`, which is also
/// the rate NVENC budgets its bitrate for). Frames then come evenly at
/// `fps` (also 120/144, also on a still screen); the polling costs about
/// one extra CPU core. The output is variable-rate (`-fps_mode vfr`, no
/// `-r`): if the capture is held up, the stream has a gap and carries on
/// with the next new picture, with no copies of the old one and no burst,
/// so frames sent per second show what the capture really delivered.
/// (Asking ddagrab for `fps` itself gave ~31 ms gaps at 60 fps and only
/// ~72–95 fps at 120/144.)
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
/// mpv (0.41 or newer) runs with low-latency options (`--profile=low-latency
/// --untimed --no-cache`, tiny probe size, straight to the right raw demuxer
/// for the codec: `--demuxer=lavf`, `h264` / `hevc` / `obu`), fed with
/// `gf_proto::bitstream::viewer_prologue` and `write_viewer_frame`,
/// hardware decoding (`--hwdec=auto-safe --hwdec-threads=1`, so no frame is
/// held back), no audio, a resizable window (double-click or `f` for
/// fullscreen), titled with the friend's name (shown as is). mpv's messages
/// go to the log.
///
/// Frames are handed to a writer thread through a queue. While mpv starts
/// (~0.6 s on Windows) everything from the latest keyframe on waits for it
/// (up to 300 frames / 16 MB), so a new viewer shows the opening keyframe.
/// After that, if mpv falls behind (6 frames waiting) the queue is cleared
/// and frames are skipped until the next keyframe. Frames before the first
/// keyframe are skipped.
///
/// Non-Windows without a display (or with `GLOORP_FAKE_VIDEO=1` and no mpv):
/// `ffmpeg -f <fmt> -i pipe:0 -f null -` stands in for mpv.
pub struct Viewer {
    inner: viewer::ViewerInner,
}

impl Viewer {
    /// Starts the viewer process. Can block for several hundred ms (starting
    /// the large mpv.exe took ~0.36 s on an RTX 5080 PC, probably antivirus
    /// scanning), so call it off threads that must stay responsive.
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
