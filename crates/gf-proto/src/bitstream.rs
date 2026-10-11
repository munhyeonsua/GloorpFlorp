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
//!
//! For AV1 the delimiter is a temporal delimiter followed by an 8-byte
//! padding OBU: FFmpeg's `obu` demuxer reads 10 bytes before it looks at an
//! OBU header, so a bare 2-byte temporal delimiter would sit unread until the
//! next frame arrives. Decoders ignore padding OBUs.

use crate::VideoCodec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemuxedFrame {
    pub codec: VideoCodec,
    /// From the FLV frame type (1 = keyframe).
    pub keyframe: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum BitstreamError {
    #[error("not an FLV stream")]
    NotFlv,
    #[error("unsupported video codec in FLV: {0}")]
    UnsupportedCodec(String),
    #[error("corrupt FLV data: {0}")]
    Corrupt(String),
}

fn corrupt(what: &str) -> BitstreamError {
    BitstreamError::Corrupt(what.to_string())
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const H264_AUD: [u8; 6] = [0, 0, 0, 1, 0x09, 0xF0];
const HEVC_AUD: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x50];
/// Temporal delimiter, then a padding OBU (6 payload bytes ending in the
/// trailing one bit) so the delimiter is 10 bytes long.
const AV1_DELIMITER: [u8; 10] = [0x12, 0x00, 0x7a, 0x06, 0, 0, 0, 0, 0, 0x80];

const H264_NAL_AUD: u8 = 9;
const HEVC_NAL_AUD: u8 = 35;
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;

/// Unit types (NAL / OBU) a keyframe needs so decoding can start there.
fn required_params(codec: VideoCodec) -> u64 {
    match codec {
        VideoCodec::H264 => 1 << 7 | 1 << 8,
        VideoCodec::Hevc => 1 << 32 | 1 << 33 | 1 << 34,
        VideoCodec::Av1 => 1 << OBU_SEQUENCE_HEADER,
    }
}

fn nal_type(codec: VideoCodec, header: u8) -> u8 {
    match codec {
        VideoCodec::Hevc => (header >> 1) & 0x3f,
        _ => header & 0x1f,
    }
}

/// Bounds-checked big-endian reader over one FLV tag.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BitstreamError> {
        let (head, rest) = self
            .data
            .split_at_checked(n)
            .ok_or_else(|| corrupt("a video tag ends in the middle of a field"))?;
        self.data = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, BitstreamError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, BitstreamError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
}

/// Decoder configuration from the latest sequence header.
struct Config {
    codec: VideoCodec,
    /// Bytes in each NAL length prefix (H.264 / HEVC).
    nal_len: usize,
    /// Parameter sets in canonical form, with their NAL / OBU type.
    units: Vec<(u8, Vec<u8>)>,
}

enum State {
    Header,
    Tags,
    Failed(BitstreamError),
}

/// Incremental FLV parser. Feed it bytes in any chunk sizes.
pub struct FlvDemuxer {
    buf: Vec<u8>,
    state: State,
    config: Option<Config>,
}

impl FlvDemuxer {
    pub fn new() -> Self {
        FlvDemuxer {
            buf: Vec::new(),
            state: State::Header,
            config: None,
        }
    }

    /// Returns every frame completed by these bytes, in order. Frames that
    /// arrive before any sequence header are dropped. After an error the
    /// demuxer is unusable (the encoder output is garbage).
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<DemuxedFrame>, BitstreamError> {
        if let State::Failed(e) = &self.state {
            return Err(e.clone());
        }
        let mut buf = std::mem::take(&mut self.buf);
        buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut pos = 0;
        let result = self.parse(&buf, &mut pos, &mut frames);
        buf.drain(..pos);
        self.buf = buf;
        match result {
            Ok(()) => Ok(frames),
            Err(e) => {
                self.state = State::Failed(e.clone());
                self.buf = Vec::new();
                Err(e)
            }
        }
    }

    fn parse(
        &mut self,
        buf: &[u8],
        pos: &mut usize,
        frames: &mut Vec<DemuxedFrame>,
    ) -> Result<(), BitstreamError> {
        loop {
            let rest = &buf[*pos..];
            match self.state {
                State::Header => {
                    let sig = b"FLV\x01";
                    let n = rest.len().min(4);
                    if rest[..n] != sig[..n] {
                        return Err(BitstreamError::NotFlv);
                    }
                    if rest.len() < 9 {
                        return Ok(());
                    }
                    let header_len =
                        u32::from_be_bytes([rest[5], rest[6], rest[7], rest[8]]) as usize;
                    if !(9..=1024).contains(&header_len) {
                        return Err(BitstreamError::NotFlv);
                    }
                    // Header, then PreviousTagSize0.
                    if rest.len() < header_len + 4 {
                        return Ok(());
                    }
                    *pos += header_len + 4;
                    self.state = State::Tags;
                }
                State::Tags => {
                    if rest.len() < 11 {
                        return Ok(());
                    }
                    let size = u32::from_be_bytes([0, rest[1], rest[2], rest[3]]) as usize;
                    let total = 11 + size + 4;
                    if rest.len() < total {
                        return Ok(());
                    }
                    let prev =
                        u32::from_be_bytes(rest[11 + size..total].try_into().unwrap_or_default());
                    if prev as usize != 11 + size && prev != 0 {
                        return Err(corrupt("tag sizes don't add up"));
                    }
                    // Upper bits: filter (encrypted) and reserved.
                    if rest[0] & 0x1f == 9 && rest[0] & 0x20 == 0 {
                        self.video_tag(&rest[11..11 + size], frames)?;
                    }
                    *pos += total;
                }
                State::Failed(ref e) => return Err(e.clone()),
            }
        }
    }

    fn video_tag(
        &mut self,
        data: &[u8],
        frames: &mut Vec<DemuxedFrame>,
    ) -> Result<(), BitstreamError> {
        let Some((&b0, rest)) = data.split_first() else {
            return Ok(());
        };
        let mut r = Reader { data: rest };
        if b0 & 0x80 != 0 {
            // Enhanced FLV.
            let frame_type = (b0 >> 4) & 7;
            let mut packet_type = b0 & 0x0f;
            while packet_type == 7 {
                // ModEx: a size-prefixed blob we don't need, then the real type.
                let mut n = r.u8()? as usize + 1;
                if n == 256 {
                    n = r.u16()? as usize + 1;
                }
                r.take(n)?;
                packet_type = r.u8()? & 0x0f;
            }
            // 5 = command frame; packet types other than SequenceStart (0),
            // CodedFrames (1) and CodedFramesX (3) carry nothing we need.
            if frame_type == 5 || !matches!(packet_type, 0 | 1 | 3) {
                return Ok(());
            }
            let codec = match r.take(4)? {
                b"avc1" => VideoCodec::H264,
                b"hvc1" => VideoCodec::Hevc,
                b"av01" => VideoCodec::Av1,
                other => {
                    return Err(BitstreamError::UnsupportedCodec(
                        String::from_utf8_lossy(other).into_owned(),
                    ))
                }
            };
            match packet_type {
                0 => self.set_config(codec, r.data),
                1 => {
                    if codec != VideoCodec::Av1 {
                        r.take(3)?; // composition time
                    }
                    self.frame(codec, frame_type == 1, r.data, frames)
                }
                _ => self.frame(codec, frame_type == 1, r.data, frames),
            }
        } else {
            let frame_type = b0 >> 4;
            if frame_type == 5 {
                return Ok(());
            }
            let codec = match b0 & 0x0f {
                7 => VideoCodec::H264,
                12 => VideoCodec::Hevc,
                id => {
                    return Err(BitstreamError::UnsupportedCodec(format!(
                        "FLV codec id {id}"
                    )))
                }
            };
            let packet_type = r.u8()?;
            r.take(3)?; // composition time
            match packet_type {
                0 => self.set_config(codec, r.data),
                1 => self.frame(codec, frame_type == 1, r.data, frames),
                _ => Ok(()),
            }
        }
    }

    fn set_config(&mut self, codec: VideoCodec, data: &[u8]) -> Result<(), BitstreamError> {
        let mut r = Reader { data };
        let mut units = Vec::new();
        let mut nal_len = 4;
        let mut push_nal = |nal: &[u8]| {
            if let Some(&h) = nal.first() {
                let mut unit = START_CODE.to_vec();
                unit.extend_from_slice(nal);
                units.push((nal_type(codec, h), unit));
            }
        };
        match codec {
            VideoCodec::H264 => {
                r.take(4)?; // version, profile, compatibility, level
                nal_len = (r.u8()? & 3) as usize + 1;
                for _ in 0..r.u8()? & 0x1f {
                    let len = r.u16()? as usize;
                    push_nal(r.take(len)?);
                }
                for _ in 0..r.u8()? {
                    let len = r.u16()? as usize;
                    push_nal(r.take(len)?);
                }
            }
            VideoCodec::Hevc => {
                r.take(21)?;
                nal_len = (r.u8()? & 3) as usize + 1;
                for _ in 0..r.u8()? {
                    r.u8()?; // array completeness + NAL type
                    for _ in 0..r.u16()? {
                        let len = r.u16()? as usize;
                        push_nal(r.take(len)?);
                    }
                }
            }
            VideoCodec::Av1 => {
                r.take(4)?;
                for_each_obu(r.data, |h, ext, payload| {
                    let mut unit = Vec::new();
                    write_obu(&mut unit, h, ext, payload);
                    units.push((obu_type(h), unit));
                })?;
            }
        }
        self.config = Some(Config {
            codec,
            nal_len,
            units,
        });
        Ok(())
    }

    fn frame(
        &mut self,
        codec: VideoCodec,
        keyframe: bool,
        data: &[u8],
        frames: &mut Vec<DemuxedFrame>,
    ) -> Result<(), BitstreamError> {
        let Some(cfg) = self.config.as_ref().filter(|c| c.codec == codec) else {
            return Ok(()); // no sequence header yet: a decoder couldn't use it
        };
        let mut body = Vec::with_capacity(data.len() + 16);
        let mut present = 0u64;
        if codec == VideoCodec::Av1 {
            for_each_obu(data, |h, ext, payload| {
                let t = obu_type(h);
                if t != OBU_TEMPORAL_DELIMITER {
                    present |= 1 << t;
                    write_obu(&mut body, h, ext, payload);
                }
            })?;
        } else {
            let aud = if codec == VideoCodec::H264 {
                H264_NAL_AUD
            } else {
                HEVC_NAL_AUD
            };
            let mut rest = data;
            while !rest.is_empty() {
                let (len, tail) = rest
                    .split_at_checked(cfg.nal_len)
                    .ok_or_else(|| corrupt("truncated NAL unit length"))?;
                let len = len.iter().fold(0usize, |acc, &b| acc << 8 | b as usize);
                let (nal, tail) = tail
                    .split_at_checked(len)
                    .ok_or_else(|| corrupt("a NAL unit runs past the end of its FLV tag"))?;
                rest = tail;
                let Some(&h) = nal.first() else { continue };
                let t = nal_type(codec, h);
                if t == aud {
                    continue;
                }
                present |= 1 << t;
                body.extend_from_slice(&START_CODE);
                body.extend_from_slice(nal);
            }
        }
        if body.is_empty() {
            return Ok(());
        }
        let missing = required_params(codec) & !present;
        let data = if keyframe && missing != 0 {
            let mut out = Vec::with_capacity(body.len() + 256);
            for (t, unit) in &cfg.units {
                if missing & (1 << t) != 0 {
                    out.extend_from_slice(unit);
                }
            }
            out.extend_from_slice(&body);
            out
        } else {
            body
        };
        frames.push(DemuxedFrame {
            codec,
            keyframe,
            data,
        });
        Ok(())
    }
}

impl Default for FlvDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

fn obu_type(header: u8) -> u8 {
    (header >> 3) & 0x0f
}

/// Calls `f(header, extension, payload)` for each low-overhead OBU in `data`.
/// OBUs without a size field extend to the end of `data`.
fn for_each_obu(
    mut data: &[u8],
    mut f: impl FnMut(u8, Option<u8>, &[u8]),
) -> Result<(), BitstreamError> {
    while let Some((&h, mut rest)) = data.split_first() {
        if h & 0x80 != 0 {
            return Err(corrupt("AV1 OBU with the forbidden bit set"));
        }
        let mut ext = None;
        if h & 0x04 != 0 {
            let (&e, r) = rest
                .split_first()
                .ok_or_else(|| corrupt("truncated AV1 OBU header"))?;
            ext = Some(e);
            rest = r;
        }
        let size = if h & 0x02 != 0 {
            let (size, r) = read_leb128(rest).ok_or_else(|| corrupt("bad AV1 OBU size"))?;
            rest = r;
            size
        } else {
            rest.len() as u64
        };
        if size > rest.len() as u64 {
            return Err(corrupt("an AV1 OBU runs past the end of its FLV tag"));
        }
        let (payload, tail) = rest.split_at(size as usize);
        f(h, ext, payload);
        data = tail;
    }
    Ok(())
}

fn read_leb128(data: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for (i, &b) in data.iter().enumerate().take(8) {
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, &data[i + 1..]));
        }
    }
    None
}

/// Writes an OBU in canonical form (with a size field).
fn write_obu(out: &mut Vec<u8>, header: u8, ext: Option<u8>, payload: &[u8]) {
    out.push(header | 0x02);
    out.extend(ext);
    let mut n = payload.len() as u64;
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
    out.extend_from_slice(payload);
}

/// Bytes to write to the viewer before the first frame: the delimiter that
/// starts a frame (H.264 AUD `00 00 00 01 09 F0`, HEVC AUD
/// `00 00 00 01 46 01 50`, AV1 temporal delimiter + padding OBU
/// `12 00 7A 06 00 00 00 00 00 80`).
pub fn viewer_prologue(codec: VideoCodec) -> Vec<u8> {
    delimiter(codec).to_vec()
}

/// Appends one frame (canonical form, as produced by [`FlvDemuxer`]) followed
/// by the delimiter that starts the next frame.
pub fn write_viewer_frame(codec: VideoCodec, frame: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(frame);
    out.extend_from_slice(delimiter(codec));
}

fn delimiter(codec: VideoCodec) -> &'static [u8] {
    match codec {
        VideoCodec::H264 => &H264_AUD,
        VideoCodec::Hevc => &HEVC_AUD,
        VideoCodec::Av1 => &AV1_DELIMITER,
    }
}

/// Header bytes of each NAL unit in Annex-B data.
fn annexb_nal_headers(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    data.windows(4)
        .filter(|w| w[..3] == [0, 0, 1])
        .map(|w| w[3])
}

/// True if the canonical frame data is a keyframe that also carries the
/// parameter sets / sequence header needed to start decoding. Used by
/// receivers as a sanity check before starting a viewer.
pub fn is_decodable_start(codec: VideoCodec, frame: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 | VideoCodec::Hevc => {
            let mut present = 0u64;
            let mut random_access = false;
            for h in annexb_nal_headers(frame) {
                let t = nal_type(codec, h);
                present |= 1 << t;
                random_access |= match codec {
                    VideoCodec::H264 => t == 5,  // IDR slice
                    _ => (16..=23).contains(&t), // IRAP (BLA / IDR / CRA)
                };
            }
            random_access && present & required_params(codec) == required_params(codec)
        }
        VideoCodec::Av1 => {
            let mut reduced_still = None;
            let mut key = None;
            let parsed = for_each_obu(frame, |h, _, payload| match obu_type(h) {
                OBU_SEQUENCE_HEADER => {
                    reduced_still.get_or_insert(payload.first().is_some_and(|b| b & 0x08 != 0));
                }
                OBU_FRAME_HEADER | OBU_FRAME if key.is_none() => {
                    // show_existing_frame = 0 and frame_type = KEY_FRAME (0),
                    // or any frame with a reduced still picture header.
                    let b = payload.first().copied().unwrap_or(0xff);
                    key = Some(reduced_still == Some(true) || b & 0xe0 == 0);
                }
                _ => {}
            });
            parsed.is_ok() && reduced_still.is_some() && key == Some(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1f, 0xac];
    const PPS: &[u8] = &[0x68, 0xee, 0x3c, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00];
    const P_SLICE: &[u8] = &[0x41, 0x9a, 0x02];
    const SEI: &[u8] = &[0x06, 0x05, 0x01];
    const AUD: &[u8] = &[0x09, 0xf0];
    const VPS_H: &[u8] = &[0x40, 0x01, 0x0c];
    const SPS_H: &[u8] = &[0x42, 0x01, 0x01];
    const PPS_H: &[u8] = &[0x44, 0x01, 0xc1];
    const IDR_H: &[u8] = &[0x26, 0x01, 0xaf];
    const CRA_H: &[u8] = &[0x2a, 0x01, 0xaf];
    const TRAIL_H: &[u8] = &[0x02, 0x01, 0xd0];
    const AUD_H: &[u8] = &[0x46, 0x01, 0x50];

    fn flv_header() -> Vec<u8> {
        b"FLV\x01\x01\0\0\0\x09\0\0\0\0".to_vec()
    }

    fn tag(kind: u8, data: &[u8]) -> Vec<u8> {
        let n = data.len() as u32;
        let mut t = vec![
            kind,
            (n >> 16) as u8,
            (n >> 8) as u8,
            n as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        t.extend_from_slice(data);
        t.extend_from_slice(&(n + 11).to_be_bytes());
        t
    }

    fn length_prefixed(nals: &[&[u8]], len_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&(n.len() as u32).to_be_bytes()[4 - len_size..]);
            out.extend_from_slice(n);
        }
        out
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| START_CODE.iter().chain(n.iter()).copied())
            .collect()
    }

    fn avcc(len_size: usize) -> Vec<u8> {
        let mut c = vec![1, 0x64, 0, 0x1f, 0xfc | (len_size as u8 - 1), 0xe1];
        c.extend_from_slice(&(SPS.len() as u16).to_be_bytes());
        c.extend_from_slice(SPS);
        c.push(1);
        c.extend_from_slice(&(PPS.len() as u16).to_be_bytes());
        c.extend_from_slice(PPS);
        c
    }

    fn hvcc() -> Vec<u8> {
        let mut c = vec![1; 21];
        c.push(0xf0 | 3);
        c.push(3);
        for nal in [VPS_H, SPS_H, PPS_H] {
            c.push(0x80 | nal_type(VideoCodec::Hevc, nal[0]));
            c.extend_from_slice(&1u16.to_be_bytes());
            c.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            c.extend_from_slice(nal);
        }
        c
    }

    fn obu(t: u8, payload: &[u8], sized: bool) -> Vec<u8> {
        let mut o = Vec::new();
        if sized {
            write_obu(&mut o, t << 3, None, payload);
        } else {
            o.push(t << 3);
            o.extend_from_slice(payload);
        }
        o
    }

    const SEQ_HDR: &[u8] = &[0x00, 0x00, 0x00, 0x02, 0xaf];
    const KEY_FRAME: &[u8] = &[0x10, 0x01, 0x02];
    const INTER_FRAME: &[u8] = &[0x30, 0x05];

    fn av1c() -> Vec<u8> {
        let mut c = vec![0x81, 0x00, 0x0c, 0x00];
        c.extend(obu(OBU_SEQUENCE_HEADER, SEQ_HDR, true));
        c
    }

    fn legacy(frame_type: u8, codec_id: u8, packet_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![frame_type << 4 | codec_id, packet_type, 0, 0, 0];
        v.extend_from_slice(payload);
        v
    }

    fn enhanced(frame_type: u8, packet_type: u8, fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x80 | frame_type << 4 | packet_type];
        v.extend_from_slice(fourcc);
        v.extend_from_slice(payload);
        v
    }

    fn with_cts(payload: &[u8]) -> Vec<u8> {
        [&[0, 0, 0][..], payload].concat()
    }

    /// A small legacy H.264 stream: config, key (with AUD + SEI), inter, key.
    fn h264_stream() -> Vec<u8> {
        let mut s = flv_header();
        s.extend(tag(18, b"\x02\x00\x0aonMetaData"));
        s.extend(tag(9, &legacy(1, 7, 0, &avcc(4))));
        s.extend(tag(8, &[0xaf, 0x01, 0x21]));
        s.extend(tag(
            9,
            &legacy(1, 7, 1, &length_prefixed(&[AUD, SEI, IDR], 4)),
        ));
        s.extend(tag(9, &legacy(2, 7, 1, &length_prefixed(&[P_SLICE], 4))));
        s.extend(tag(9, &legacy(1, 7, 1, &length_prefixed(&[IDR], 4))));
        s.extend(tag(9, &legacy(1, 7, 2, &[])));
        s
    }

    #[test]
    fn legacy_h264() {
        let frames = FlvDemuxer::new().push(&h264_stream()).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0],
            DemuxedFrame {
                codec: VideoCodec::H264,
                keyframe: true,
                data: annexb(&[SPS, PPS, SEI, IDR])
            }
        );
        assert_eq!(
            frames[1],
            DemuxedFrame {
                codec: VideoCodec::H264,
                keyframe: false,
                data: annexb(&[P_SLICE])
            }
        );
        assert_eq!(frames[2].data, annexb(&[SPS, PPS, IDR]));
        assert!(is_decodable_start(VideoCodec::H264, &frames[0].data));
        assert!(!is_decodable_start(VideoCodec::H264, &frames[1].data));
        assert!(is_decodable_start(VideoCodec::H264, &frames[2].data));
    }

    #[test]
    fn any_chunking_gives_same_frames() {
        let stream = h264_stream();
        let whole = FlvDemuxer::new().push(&stream).unwrap();
        let mut d = FlvDemuxer::new();
        let mut bytewise = Vec::new();
        for b in &stream {
            bytewise.extend(d.push(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(bytewise, whole);
        let mut rng = StdRng::seed_from_u64(2);
        for _ in 0..50 {
            let mut d = FlvDemuxer::new();
            let mut got = Vec::new();
            let mut rest = &stream[..];
            while !rest.is_empty() {
                let n = rng.random_range(0..=rest.len().min(40));
                got.extend(d.push(&rest[..n]).unwrap());
                rest = &rest[n..];
            }
            assert_eq!(got, whole);
        }
    }

    #[test]
    fn frames_complete_without_waiting_for_more_bytes() {
        let mut s = flv_header();
        s.extend(tag(9, &legacy(1, 7, 0, &avcc(4))));
        s.extend(tag(9, &legacy(1, 7, 1, &length_prefixed(&[IDR], 4))));
        let mut d = FlvDemuxer::new();
        // The frame is out as soon as its PreviousTagSize has arrived.
        assert!(d.push(&s[..s.len() - 1]).unwrap().is_empty());
        assert_eq!(d.push(&s[s.len() - 1..]).unwrap().len(), 1);
    }

    #[test]
    fn short_nal_lengths_and_existing_parameter_sets() {
        let mut s = flv_header();
        s.extend(tag(9, &legacy(1, 7, 0, &avcc(2))));
        s.extend(tag(
            9,
            &legacy(1, 7, 1, &length_prefixed(&[SPS, PPS, IDR], 2)),
        ));
        let frames = FlvDemuxer::new().push(&s).unwrap();
        assert_eq!(
            frames[0].data,
            annexb(&[SPS, PPS, IDR]),
            "no duplicate parameter sets"
        );
    }

    #[test]
    fn frames_before_sequence_header_dropped() {
        let mut s = flv_header();
        s.extend(tag(9, &legacy(1, 7, 1, &length_prefixed(&[IDR], 4))));
        s.extend(tag(
            9,
            &enhanced(1, 1, b"av01", &obu(OBU_FRAME, KEY_FRAME, true)),
        ));
        s.extend(tag(9, &legacy(1, 7, 0, &avcc(4))));
        // A frame of another codec than the configured one is dropped too.
        s.extend(tag(
            9,
            &enhanced(1, 1, b"av01", &obu(OBU_FRAME, KEY_FRAME, true)),
        ));
        s.extend(tag(9, &legacy(2, 7, 1, &length_prefixed(&[P_SLICE], 4))));
        let frames = FlvDemuxer::new().push(&s).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, annexb(&[P_SLICE]));
    }

    #[test]
    fn legacy_hevc_codec_12() {
        let mut s = flv_header();
        s.extend(tag(9, &legacy(1, 12, 0, &hvcc())));
        s.extend(tag(
            9,
            &legacy(1, 12, 1, &length_prefixed(&[AUD_H, IDR_H], 4)),
        ));
        let frames = FlvDemuxer::new().push(&s).unwrap();
        assert_eq!(frames[0].codec, VideoCodec::Hevc);
        assert_eq!(frames[0].data, annexb(&[VPS_H, SPS_H, PPS_H, IDR_H]));
    }

    #[test]
    fn enhanced_hevc() {
        let mut s = flv_header();
        s.extend(tag(9, &enhanced(1, 0, b"hvc1", &hvcc())));
        s.extend(tag(
            9,
            &enhanced(
                1,
                1,
                b"hvc1",
                &with_cts(&length_prefixed(&[AUD_H, VPS_H, SPS_H, PPS_H, IDR_H], 4)),
            ),
        ));
        s.extend(tag(
            9,
            &enhanced(2, 3, b"hvc1", &length_prefixed(&[TRAIL_H], 4)),
        ));
        s.extend(tag(
            9,
            &enhanced(1, 3, b"hvc1", &length_prefixed(&[CRA_H], 4)),
        ));
        s.extend(tag(9, &enhanced(1, 2, b"hvc1", &[])));
        let frames = FlvDemuxer::new().push(&s).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].data, annexb(&[VPS_H, SPS_H, PPS_H, IDR_H]));
        assert_eq!(frames[1].data, annexb(&[TRAIL_H]));
        assert!(!frames[1].keyframe);
        assert_eq!(frames[2].data, annexb(&[VPS_H, SPS_H, PPS_H, CRA_H]));
        for (f, decodable) in frames.iter().zip([true, false, true]) {
            assert_eq!(is_decodable_start(VideoCodec::Hevc, &f.data), decodable);
        }
        assert!(
            !is_decodable_start(VideoCodec::Hevc, &annexb(&[SPS_H, PPS_H, IDR_H])),
            "VPS missing"
        );
    }

    #[test]
    fn enhanced_av1() {
        let mut s = flv_header();
        s.extend(tag(9, &enhanced(1, 0, b"av01", &av1c())));
        // Temporal delimiter, then a frame OBU without a size field.
        let key = [
            obu(OBU_TEMPORAL_DELIMITER, &[], true),
            obu(OBU_FRAME, KEY_FRAME, false),
        ]
        .concat();
        s.extend(tag(9, &enhanced(1, 1, b"av01", &key)));
        s.extend(tag(
            9,
            &enhanced(2, 1, b"av01", &obu(OBU_FRAME, INTER_FRAME, true)),
        ));
        // Keyframe that already carries its sequence header (with an
        // extension byte on the frame OBU).
        let mut ext_frame = vec![OBU_FRAME << 3 | 0x04 | 0x02, 0x08, KEY_FRAME.len() as u8];
        ext_frame.extend_from_slice(KEY_FRAME);
        // Sequence header with a padded 2-byte leb128 size.
        let mut key2 = vec![
            OBU_SEQUENCE_HEADER << 3 | 0x02,
            SEQ_HDR.len() as u8 | 0x80,
            0x00,
        ];
        key2.extend_from_slice(SEQ_HDR);
        key2.extend_from_slice(&ext_frame);
        s.extend(tag(9, &enhanced(1, 3, b"av01", &key2)));
        let frames = FlvDemuxer::new().push(&s).unwrap();
        assert_eq!(frames.len(), 3);
        let seq = obu(OBU_SEQUENCE_HEADER, SEQ_HDR, true);
        assert_eq!(
            frames[0].data,
            [seq.clone(), obu(OBU_FRAME, KEY_FRAME, true)].concat()
        );
        assert_eq!(frames[1].data, obu(OBU_FRAME, INTER_FRAME, true));
        assert_eq!(frames[2].data, [seq, ext_frame].concat());
        assert!(is_decodable_start(VideoCodec::Av1, &frames[0].data));
        assert!(!is_decodable_start(VideoCodec::Av1, &frames[1].data));
        assert!(is_decodable_start(VideoCodec::Av1, &frames[2].data));
    }

    #[test]
    fn av1_decodable_start_rules() {
        let seq = obu(OBU_SEQUENCE_HEADER, SEQ_HDR, true);
        let key = obu(OBU_FRAME, KEY_FRAME, true);
        assert!(is_decodable_start(
            VideoCodec::Av1,
            &[seq.clone(), key.clone()].concat()
        ));
        assert!(
            !is_decodable_start(VideoCodec::Av1, &key),
            "no sequence header"
        );
        let show_existing = obu(OBU_FRAME_HEADER, &[0x80, 0x00], true);
        assert!(!is_decodable_start(
            VideoCodec::Av1,
            &[seq.clone(), show_existing].concat()
        ));
        let frame_header_key = obu(OBU_FRAME_HEADER, KEY_FRAME, true);
        assert!(is_decodable_start(
            VideoCodec::Av1,
            &[seq.clone(), frame_header_key].concat()
        ));
        let intra_only = obu(OBU_FRAME, &[0x50], true);
        assert!(!is_decodable_start(
            VideoCodec::Av1,
            &[seq, intra_only.clone()].concat()
        ));
        let reduced = obu(OBU_SEQUENCE_HEADER, &[0x18, 0x00], true);
        assert!(is_decodable_start(
            VideoCodec::Av1,
            &[reduced, intra_only].concat()
        ));
        assert!(
            !is_decodable_start(VideoCodec::Av1, &[0xff, 0xff]),
            "garbage"
        );
        assert!(!is_decodable_start(VideoCodec::Av1, &[]));
    }

    #[test]
    fn h264_decodable_start_rules() {
        assert!(is_decodable_start(
            VideoCodec::H264,
            &annexb(&[SPS, PPS, IDR])
        ));
        assert!(!is_decodable_start(VideoCodec::H264, &annexb(&[SPS, IDR])));
        assert!(!is_decodable_start(
            VideoCodec::H264,
            &annexb(&[SPS, PPS, P_SLICE])
        ));
        assert!(!is_decodable_start(VideoCodec::H264, &[]));
        // 3-byte start codes work too.
        assert!(is_decodable_start(
            VideoCodec::H264,
            &[0, 0, 1, 0x67, 0, 0, 1, 0x68, 0, 0, 1, 0x65, 0x88]
        ));
    }

    #[test]
    fn skips_modex_metadata_multitrack_and_commands() {
        let mut s = flv_header();
        s.extend(tag(9, &enhanced(1, 0, b"avc1", &avcc(4))));
        // ModEx with a 2-byte blob wrapping CodedFramesX.
        s.extend(tag(
            9,
            &[
                0x80 | 1 << 4 | 7,
                1,
                0xaa,
                0xbb,
                0x03,
                b'a',
                b'v',
                b'c',
                b'1',
                0,
                0,
                0,
                4,
                0x65,
                0x88,
                0x84,
                0x00,
            ],
        ));
        // Metadata, Multitrack, MPEG2TSSequenceStart, unknown packet type.
        s.extend(tag(
            9,
            &enhanced(
                0,
                4,
                b"avc1",
                &[
                    2, 0, 9, b'c', b'o', b'l', b'o', b'r', b'I', b'n', b'f', b'o',
                ],
            ),
        ));
        s.extend(tag(
            9,
            &[0x80 | 1 << 4 | 6, 0x00, b'a', b'v', b'c', b'1', 0xff],
        ));
        s.extend(tag(9, &enhanced(1, 5, b"av01", &[1, 2, 3])));
        s.extend(tag(9, &enhanced(1, 12, b"xxxx", &[])));
        // Command frames, legacy and enhanced.
        s.extend(tag(9, &[0x57, 0x00]));
        s.extend(tag(9, &[0x80 | 5 << 4 | 1, 0x00]));
        // Encrypted (filter bit) video tag and an empty video tag.
        s.extend(tag(9 | 0x20, &legacy(1, 7, 1, &length_prefixed(&[IDR], 4))));
        s.extend(tag(9, &[]));
        s.extend(tag(
            9,
            &enhanced(2, 1, b"avc1", &with_cts(&length_prefixed(&[P_SLICE], 4))),
        ));
        let frames = FlvDemuxer::new().push(&s).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, annexb(&[SPS, PPS, IDR]));
        assert!(frames[0].keyframe);
        assert_eq!(frames[1].data, annexb(&[P_SLICE]));
    }

    #[test]
    fn not_flv() {
        assert_eq!(
            FlvDemuxer::new().push(b"GIF89a"),
            Err(BitstreamError::NotFlv)
        );
        assert_eq!(FlvDemuxer::new().push(b"F"), Ok(vec![]));
        assert_eq!(
            FlvDemuxer::new().push(b"FLV\x02"),
            Err(BitstreamError::NotFlv)
        );
        assert_eq!(
            FlvDemuxer::new().push(b"FLV\x01\x01\0\0\0\x05"),
            Err(BitstreamError::NotFlv)
        );
        let mut d = FlvDemuxer::new();
        assert_eq!(d.push(b"\x00\x00\x00\x01\x67"), Err(BitstreamError::NotFlv));
        assert_eq!(
            d.push(&flv_header()),
            Err(BitstreamError::NotFlv),
            "errors are sticky"
        );
    }

    #[test]
    fn corrupt_streams_are_errors() {
        let base = || {
            let mut s = flv_header();
            s.extend(tag(9, &legacy(1, 7, 0, &avcc(4))));
            s
        };
        // NAL length beyond the tag.
        let mut s = base();
        let mut bad = length_prefixed(&[IDR], 4);
        bad[3] = 50;
        s.extend(tag(9, &legacy(1, 7, 1, &bad)));
        assert!(matches!(
            FlvDemuxer::new().push(&s),
            Err(BitstreamError::Corrupt(_))
        ));
        // Truncated length prefix.
        let mut s = base();
        s.extend(tag(9, &legacy(1, 7, 1, &[0, 0])));
        assert!(matches!(
            FlvDemuxer::new().push(&s),
            Err(BitstreamError::Corrupt(_))
        ));
        // PreviousTagSize mismatch.
        let mut s = base();
        let mut t = tag(9, &legacy(2, 7, 1, &length_prefixed(&[P_SLICE], 4)));
        let n = t.len();
        t[n - 1] ^= 0x55;
        s.extend(t);
        assert!(matches!(
            FlvDemuxer::new().push(&s),
            Err(BitstreamError::Corrupt(_))
        ));
        // Truncated avcC.
        let mut s = flv_header();
        s.extend(tag(9, &legacy(1, 7, 0, &avcc(4)[..8])));
        assert!(matches!(
            FlvDemuxer::new().push(&s),
            Err(BitstreamError::Corrupt(_))
        ));
        // AV1 forbidden bit and oversized OBU.
        for payload in [vec![0x80 | OBU_FRAME << 3], vec![OBU_FRAME << 3 | 2, 9, 1]] {
            let mut s = flv_header();
            s.extend(tag(9, &enhanced(1, 0, b"av01", &av1c())));
            s.extend(tag(9, &enhanced(1, 1, b"av01", &payload)));
            assert!(matches!(
                FlvDemuxer::new().push(&s),
                Err(BitstreamError::Corrupt(_))
            ));
        }
    }

    #[test]
    fn unsupported_codecs() {
        let mut s = flv_header();
        s.extend(tag(9, &legacy(1, 2, 0, &[])));
        assert_eq!(
            FlvDemuxer::new().push(&s),
            Err(BitstreamError::UnsupportedCodec("FLV codec id 2".into()))
        );
        let mut s = flv_header();
        s.extend(tag(9, &enhanced(1, 0, b"vp09", &[])));
        assert_eq!(
            FlvDemuxer::new().push(&s),
            Err(BitstreamError::UnsupportedCodec("vp09".into()))
        );
    }

    #[test]
    fn fuzz_never_panics() {
        let mut rng = StdRng::seed_from_u64(4);
        let valid = h264_stream();
        let mut av1 = flv_header();
        av1.extend(tag(9, &enhanced(1, 0, b"av01", &av1c())));
        av1.extend(tag(
            9,
            &enhanced(1, 1, b"av01", &obu(OBU_FRAME, KEY_FRAME, true)),
        ));
        for i in 0..20_000 {
            let mut d = FlvDemuxer::new();
            let input = match i % 3 {
                0 => {
                    let mut b = flv_header();
                    let mut junk = vec![0u8; rng.random_range(0..200)];
                    rng.fill_bytes(&mut junk);
                    b.extend(junk);
                    b
                }
                _ => {
                    let mut b = if i % 3 == 1 {
                        valid.clone()
                    } else {
                        av1.clone()
                    };
                    for _ in 0..rng.random_range(1..5) {
                        let p = rng.random_range(13..b.len());
                        b[p] = rng.random();
                    }
                    b
                }
            };
            for chunk in input.chunks(rng.random_range(1..64)) {
                if d.push(chunk).is_err() {
                    break;
                }
            }
            let mut tagged = flv_header();
            tagged.extend(tag(9, &input[13..]));
            let _ = FlvDemuxer::new().push(&tagged);
            let _ = is_decodable_start(VideoCodec::Av1, &input);
            let _ = is_decodable_start(VideoCodec::Hevc, &input);
        }
    }

    #[test]
    fn viewer_writer() {
        assert_eq!(viewer_prologue(VideoCodec::H264), [0, 0, 0, 1, 0x09, 0xf0]);
        assert_eq!(
            viewer_prologue(VideoCodec::Hevc),
            [0, 0, 0, 1, 0x46, 0x01, 0x50]
        );
        assert_eq!(&viewer_prologue(VideoCodec::Av1)[..2], [0x12, 0x00]);
        for codec in VideoCodec::ALL {
            let mut out = vec![7];
            write_viewer_frame(codec, &[1, 2, 3], &mut out);
            assert_eq!(out, [&[7, 1, 2, 3][..], &viewer_prologue(codec)].concat());
        }
        // The AV1 delimiter parses as a temporal delimiter + padding OBU.
        let mut types = Vec::new();
        for_each_obu(&viewer_prologue(VideoCodec::Av1), |h, _, _| {
            types.push(obu_type(h))
        })
        .unwrap();
        assert_eq!(types, [OBU_TEMPORAL_DELIMITER, 15]);
    }
}
