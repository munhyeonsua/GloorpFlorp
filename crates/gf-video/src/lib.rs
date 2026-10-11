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
//! Non-Windows (development and tests only): the encoder uses FFmpeg's
//! `testsrc2` pattern and software encoders (libx264 / libx265 / libsvtav1),
//! and the viewer runs `mpv` if it exists, else `ffmpeg -f null` as a sink.

use std::path::PathBuf;

use crossbeam_channel::Receiver;
use gf_proto::{VideoCodec, VideoFrame};

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
        todo!()
    }
}

/// Which NVENC encoders actually work on this PC, best first. Runs a tiny
/// test encode for each codec (a few seconds total, call off the UI thread).
/// Non-Windows: the software encoders available in the local FFmpeg.
pub fn probe_encoders(tools: &Tools) -> Vec<VideoCodec> {
    todo!()
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
    Started { codec: VideoCodec, width: u32, height: u32, fps: u32 },
    Frame(VideoFrame),
    /// The encoder exited. `error` is a plain-language explanation followed by
    /// the last lines FFmpeg printed, or `None` after a normal `stop()`.
    Stopped { error: Option<String> },
}

/// A running FFmpeg capture+encode process. Stops (kills FFmpeg) when dropped.
///
/// Start-up tries a short ladder of FFmpeg command lines, moving to the next
/// if FFmpeg exits or produces no frame within a few seconds:
/// 1. GPU all the way: ddagrab (D3D11) straight into NVENC; when scaling is
///    needed, scale on the GPU.
/// 2. Same but scaling via CUDA (`hwmap=derive_device=cuda,scale_cuda`).
/// 3. Safe fallback: `hwdownload` to system memory, CPU scale, then NVENC.
/// NVENC flags aim for low latency: `-preset p4 -tune ull -rc cbr`, no
/// B-frames, `-zerolatency 1`, `-delay 0`, `-g keyframe_interval`. Output is
/// FLV on stdout (`-f flv -flvflags no_duration_filesize -flush_packets 1`)
/// parsed by `gf_proto::bitstream::FlvDemuxer`.
pub struct Encoder {}

impl Encoder {
    pub fn start(
        tools: &Tools,
        settings: &EncoderSettings,
    ) -> anyhow::Result<(Encoder, Receiver<EncoderEvent>)> {
        todo!()
    }

    pub fn stop(&mut self) {
        todo!()
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
pub struct Viewer {}

impl Viewer {
    pub fn start(tools: &Tools, codec: VideoCodec, title: &str) -> anyhow::Result<Viewer> {
        todo!()
    }

    /// Never blocks. Returns false once the window has been closed.
    pub fn push(&self, frame: &VideoFrame) -> bool {
        todo!()
    }

    /// False after the user closed the window or mpv exited.
    pub fn is_open(&self) -> bool {
        todo!()
    }

    pub fn stop(&mut self) {
        todo!()
    }
}
