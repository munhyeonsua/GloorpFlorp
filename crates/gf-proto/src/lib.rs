//! Shared types for GloorpFlorp: the wire protocol spoken between the two
//! PCs, room codes, video bitstream splitting and the audio jitter buffer.
//!
//! Everything here is pure Rust with no I/O so it can be unit tested on any OS.

pub mod bitstream;
pub mod jitter;
pub mod room;
pub mod wire;

use serde::{Deserialize, Serialize};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u16 = 1;
/// TCP and UDP port the host listens on (on its Tailscale address only).
pub const DEFAULT_PORT: u16 = 47800;
/// All audio inside the app runs at this rate.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
/// Audio is processed and sent in 10 ms frames.
pub const AUDIO_FRAME_MS: u32 = 10;
/// Samples per channel in one audio frame (480).
pub const AUDIO_FRAME_SAMPLES: usize = (AUDIO_SAMPLE_RATE / 1000 * AUDIO_FRAME_MS) as usize;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    Av1,
    Hevc,
    H264,
}

impl VideoCodec {
    /// Best first.
    pub const ALL: [VideoCodec; 3] = [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];

    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::Av1 => "AV1",
            VideoCodec::Hevc => "HEVC",
            VideoCodec::H264 => "H.264",
        }
    }
}

/// One encoded picture (an access unit / temporal unit).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub codec: VideoCodec,
    /// True when a decoder can start from this frame. Keyframes always carry
    /// the parameter sets / sequence header needed to start decoding.
    pub keyframe: bool,
    /// Sender's monotonic clock in microseconds when the frame left the encoder.
    pub sent_us: u64,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// The two audio streams sent between the PCs.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioStream {
    /// Microphone, mono Opus.
    Voice,
    /// Game / PC sound, stereo Opus.
    Game,
}
