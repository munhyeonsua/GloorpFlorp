//! Video bitstream handling, designed so that no stage has to wait for the
//! *next* frame to know the current one is complete (that would add a whole
//! frame of delay at each end).
//!
//! **Sender side.** FFmpeg writes FLV to stdout (`-f flv -flvflags
//! no_duration_filesize -flush_packets 1`). FLV tags carry explicit lengths,
//! so [`FlvDemuxer`] can hand out each frame the moment its last byte
//! arrives. It supports legacy FLV (H.264, codec id 7) and enhanced FLV
//! (`IsExHeader` bit, FourCC `avc1` / `hvc1` / `av01`; packet types
//! SequenceStart, CodedFrames (with 24-bit composition time for avc1/hvc1
//! only), CodedFramesX, SequenceEnd; Metadata and anything unknown ignored).
//! Audio and script tags are ignored.
//!
//! Frames are converted to a canonical form, [`DemuxedFrame::data`]:
//! - H.264 / HEVC: Annex-B (4-byte `00 00 00 01` start codes). Length-prefixed
//!   NAL units are converted using the NAL length size from the decoder
//!   configuration record (avcC / hvcC).
//! - AV1: low-overhead OBUs, each with `obu_has_size_field` = 1.
//! - Never contains access unit delimiters (H.264 NAL 9, HEVC NAL 35) or AV1
//!   temporal delimiters (OBU type 2): they are stripped. The viewer writer
//!   adds its own (see [`viewer_prologue`]).
//! - Every keyframe starts with the parameter sets from the latest
//!   SequenceStart / AVC sequence header (H.264 SPS+PPS, HEVC VPS+SPS+PPS,
//!   AV1 sequence header OBU) unless the frame already contains them, so a
//!   viewer can start decoding from any keyframe.
//!
//! **Viewer side.** mpv reads a raw elementary stream on stdin (lavf demuxer
//! `h264` / `hevc` / `obu`). Those parsers only finish a frame when they see
//! the start of the next one, so after each frame we immediately write the
//! delimiter that starts the next one (AUD or temporal delimiter). The
//! parser then completes the frame without waiting for more data.

use crate::VideoCodec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemuxedFrame {
    pub codec: VideoCodec,
    /// From the FLV frame type (1 = keyframe).
    pub keyframe: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BitstreamError {
    #[error("not an FLV stream")]
    NotFlv,
    #[error("unsupported video codec in FLV: {0}")]
    UnsupportedCodec(String),
    #[error("corrupt FLV data: {0}")]
    Corrupt(String),
}

/// Incremental FLV parser. Feed it bytes in any chunk sizes.
pub struct FlvDemuxer {}

impl FlvDemuxer {
    pub fn new() -> Self {
        todo!()
    }

    /// Returns every frame completed by these bytes, in order. Frames that
    /// arrive before any sequence header are dropped. After an error the
    /// demuxer is unusable (the encoder output is garbage).
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<DemuxedFrame>, BitstreamError> {
        todo!()
    }
}

impl Default for FlvDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// Bytes to write to the viewer before the first frame: the delimiter that
/// starts a frame (H.264 AUD `00 00 00 01 09 F0`, HEVC AUD
/// `00 00 00 01 46 01 50`, AV1 temporal delimiter `12 00`).
pub fn viewer_prologue(codec: VideoCodec) -> Vec<u8> {
    todo!()
}

/// Appends one frame (canonical form, as produced by [`FlvDemuxer`]) followed
/// by the delimiter that starts the next frame.
pub fn write_viewer_frame(codec: VideoCodec, frame: &[u8], out: &mut Vec<u8>) {
    todo!()
}

/// True if the canonical frame data is a keyframe that also carries the
/// parameter sets / sequence header needed to start decoding. Used by
/// receivers as a sanity check before starting a viewer.
pub fn is_decodable_start(codec: VideoCodec, frame: &[u8]) -> bool {
    todo!()
}
