//! Wire format between the two PCs.
//!
//! - **TCP** (host listens on its Tailscale IP, port [`crate::DEFAULT_PORT`]):
//!   control messages and video. Each message is a little-endian `u32` byte
//!   length followed by the postcard encoding of [`TcpMsg`].
//! - **UDP** (same port on the host): audio and pings. Each datagram is a
//!   12-byte [`UdpHeader`] followed by the payload (an Opus packet, or an
//!   8-byte little-endian timestamp for pings).

use serde::{Deserialize, Serialize};

use crate::{VideoCodec, VideoFrame};

/// Refuse TCP messages larger than this (protects against garbage input).
pub const MAX_TCP_MSG: usize = 16 * 1024 * 1024;
/// First byte of every UDP datagram.
pub const UDP_MAGIC: u8 = 0x47;
pub const UDP_HEADER_LEN: usize = 12;

/// Frozen across versions: `Hello` stays the first variant with `version` as
/// its first field (see [`hello_version`]), and `Reject` stays the third
/// variant with only `reason`, so two different versions can always tell
/// each other so. New variants go at the end.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TcpMsg {
    /// Joiner -> host, first message.
    Hello {
        version: u16,
        secret: u32,
        name: String,
        /// Video codecs this PC can encode, best first.
        encoders: Vec<VideoCodec>,
    },
    /// Host -> joiner, accepts the join. `udp_token` must be in every UDP datagram.
    Welcome {
        name: String,
        encoders: Vec<VideoCodec>,
        udp_token: u32,
    },
    /// Host -> joiner, refuses the join (wrong code, room full, version mismatch).
    Reject {
        reason: String,
    },
    /// Keepalive, sent every 2 s by both sides; answered with `Pong`.
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// Sender started sharing its screen; frames follow.
    ShareStarted {
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
    },
    ShareStopped,
    /// Viewer tells the sharer whether it wants frames (viewer window open).
    Watching {
        on: bool,
    },
    Video(VideoFrame),
    /// Sender started/stopped sending game audio.
    GameAudio {
        active: bool,
    },
    /// Polite goodbye before closing the connection.
    Bye,
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("message too large ({0} bytes)")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Length-prefixed encoding of one message, ready to write to the socket.
pub fn encode_tcp(msg: &TcpMsg) -> Vec<u8> {
    let mut out = postcard::to_extend(msg, vec![0u8; 4])
        .expect("serializing a TcpMsg into a Vec cannot fail");
    let len = (out.len() - 4) as u32;
    out[..4].copy_from_slice(&len.to_le_bytes());
    out
}

/// Blocking read of one message from a stream.
/// Messages larger than [`MAX_TCP_MSG`] are refused before anything is
/// allocated; the stream is then out of sync and should be closed.
pub fn read_tcp(r: &mut impl std::io::Read) -> Result<TcpMsg, WireError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_TCP_MSG {
        return Err(WireError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    decode_tcp_body(&body)
}

/// Decodes one message body (the bytes after the length prefix). The whole
/// slice must be exactly one message.
pub fn decode_tcp_body(body: &[u8]) -> Result<TcpMsg, WireError> {
    if body.len() > MAX_TCP_MSG {
        return Err(WireError::TooLarge(body.len()));
    }
    match postcard::take_from_bytes::<TcpMsg>(body) {
        Ok((msg, [])) => Ok(msg),
        Ok((_, rest)) => Err(WireError::Malformed(format!(
            "{} unexpected trailing bytes",
            rest.len()
        ))),
        Err(e) => Err(WireError::Malformed(e.to_string())),
    }
}

/// The frozen start of every `Hello`, past and future.
#[derive(Deserialize)]
enum HelloHead {
    Hello { version: u16 },
}

/// The protocol version from a `Hello` message body, reading only its
/// frozen start, so a `Hello` from another version whose later fields differ
/// still gets a "different version" answer. `None` if the body doesn't start
/// like a `Hello`.
pub fn hello_version(body: &[u8]) -> Option<u16> {
    match postcard::take_from_bytes::<HelloHead>(body) {
        Ok((HelloHead::Hello { version }, _)) => Some(version),
        Err(_) => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpKind {
    Voice = 1,
    Game = 2,
    Ping = 3,
    Pong = 4,
}

impl UdpKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => UdpKind::Voice,
            2 => UdpKind::Game,
            3 => UdpKind::Ping,
            4 => UdpKind::Pong,
            _ => return None,
        })
    }
}

/// Layout: magic u8, kind u8, token u32 LE, seq u16 LE, ts u32 LE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpHeader {
    pub kind: UdpKind,
    pub token: u32,
    pub seq: u16,
    /// Audio: sample timestamp of the first sample. Pings: unused (0).
    pub ts: u32,
}

/// Clears `out` and writes header + payload into it.
pub fn encode_udp(hdr: &UdpHeader, payload: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(UDP_HEADER_LEN + payload.len());
    out.push(UDP_MAGIC);
    out.push(hdr.kind as u8);
    out.extend_from_slice(&hdr.token.to_le_bytes());
    out.extend_from_slice(&hdr.seq.to_le_bytes());
    out.extend_from_slice(&hdr.ts.to_le_bytes());
    out.extend_from_slice(payload);
}

/// Returns the header and payload slice, or `None` for anything malformed.
pub fn decode_udp(buf: &[u8]) -> Option<(UdpHeader, &[u8])> {
    let (h, payload) = buf.split_at_checked(UDP_HEADER_LEN)?;
    if h[0] != UDP_MAGIC {
        return None;
    }
    let hdr = UdpHeader {
        kind: UdpKind::from_u8(h[1])?,
        token: u32::from_le_bytes([h[2], h[3], h[4], h[5]]),
        seq: u16::from_le_bytes([h[6], h[7]]),
        ts: u32::from_le_bytes([h[8], h[9], h[10], h[11]]),
    };
    Some((hdr, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
    use std::io::Cursor;

    fn all_msgs() -> Vec<TcpMsg> {
        vec![
            TcpMsg::Hello {
                version: crate::PROTOCOL_VERSION,
                secret: 0x3fff_ffff,
                name: "Gloorp ✓".into(),
                encoders: vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
            },
            TcpMsg::Welcome {
                name: String::new(),
                encoders: vec![],
                udp_token: u32::MAX,
            },
            TcpMsg::Reject {
                reason: "wrong code".into(),
            },
            TcpMsg::Ping { nonce: u64::MAX },
            TcpMsg::Pong { nonce: 0 },
            TcpMsg::ShareStarted {
                codec: VideoCodec::Hevc,
                width: 2560,
                height: 1440,
                fps: 60,
            },
            TcpMsg::ShareStopped,
            TcpMsg::Watching { on: true },
            TcpMsg::Watching { on: false },
            TcpMsg::Video(VideoFrame {
                codec: VideoCodec::H264,
                keyframe: true,
                sent_us: 123_456_789,
                data: vec![0, 0, 0, 1, 0x67],
            }),
            TcpMsg::GameAudio { active: true },
            TcpMsg::Bye,
        ]
    }

    #[test]
    fn tcp_round_trip_every_variant() {
        let msgs = all_msgs();
        let mut stream = Vec::new();
        for m in &msgs {
            let enc = encode_tcp(m);
            let len = u32::from_le_bytes(enc[..4].try_into().unwrap()) as usize;
            assert_eq!(len, enc.len() - 4);
            assert_eq!(&read_tcp(&mut Cursor::new(&enc)).unwrap(), m);
            stream.extend_from_slice(&enc);
        }
        // Back to back on one stream.
        let mut r = Cursor::new(&stream);
        for m in &msgs {
            assert_eq!(&read_tcp(&mut r).unwrap(), m);
        }
        assert!(matches!(read_tcp(&mut r), Err(WireError::Io(_))));
    }

    #[test]
    fn large_video_message() {
        let mut data = vec![0u8; 2 * 1024 * 1024];
        StdRng::seed_from_u64(3).fill_bytes(&mut data);
        let msg = TcpMsg::Video(VideoFrame {
            codec: VideoCodec::Av1,
            keyframe: false,
            sent_us: 9,
            data,
        });
        let enc = encode_tcp(&msg);
        assert!(
            enc.len() < 2 * 1024 * 1024 + 32,
            "video bytes should not be inflated"
        );
        assert_eq!(read_tcp(&mut Cursor::new(&enc)).unwrap(), msg);
    }

    #[test]
    fn truncated_input_is_an_error() {
        for m in all_msgs() {
            let enc = encode_tcp(&m);
            for cut in 0..enc.len() {
                assert!(
                    read_tcp(&mut Cursor::new(&enc[..cut])).is_err(),
                    "{m:?} cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn oversized_is_rejected_before_reading() {
        let mut buf = ((MAX_TCP_MSG + 1) as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&[0; 16]);
        assert!(
            matches!(read_tcp(&mut Cursor::new(&buf)), Err(WireError::TooLarge(n)) if n == MAX_TCP_MSG + 1)
        );
        let buf = u32::MAX.to_le_bytes();
        assert!(matches!(
            read_tcp(&mut Cursor::new(&buf)),
            Err(WireError::TooLarge(_))
        ));
    }

    #[test]
    fn garbage_is_malformed() {
        // Unknown enum variant.
        let mut buf = 1u32.to_le_bytes().to_vec();
        buf.push(200);
        assert!(matches!(
            read_tcp(&mut Cursor::new(&buf)),
            Err(WireError::Malformed(_))
        ));
        // Valid message followed by junk inside the same frame.
        let mut enc = encode_tcp(&TcpMsg::Bye);
        enc.push(0);
        let len = (enc.len() - 4) as u32;
        enc[..4].copy_from_slice(&len.to_le_bytes());
        assert!(matches!(
            read_tcp(&mut Cursor::new(&enc)),
            Err(WireError::Malformed(_))
        ));
        // Empty body.
        assert!(matches!(
            read_tcp(&mut Cursor::new(&[0, 0, 0, 0])),
            Err(WireError::Malformed(_))
        ));
        // Video claiming a huge payload that isn't there.
        let mut body = encode_tcp(&TcpMsg::Video(VideoFrame {
            codec: VideoCodec::H264,
            keyframe: false,
            sent_us: 0,
            data: vec![],
        }))[4..]
            .to_vec();
        body.pop();
        body.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert!(decode_tcp_body(&body).is_err());
    }

    #[test]
    fn hello_version_reads_only_the_frozen_start() {
        let hello = &all_msgs()[0];
        let body = &encode_tcp(hello)[4..];
        assert_eq!(hello_version(body), Some(crate::PROTOCOL_VERSION));
        // A future Hello with other fields after `version`.
        let mut future = vec![0u8];
        future.extend_from_slice(&postcard::to_stdvec(&700u16).unwrap());
        future.extend_from_slice(&[0xff; 9]);
        assert!(decode_tcp_body(&future).is_err());
        assert_eq!(hello_version(&future), Some(700));
        for other in &all_msgs()[1..] {
            assert_eq!(hello_version(&encode_tcp(other)[4..]), None, "{other:?}");
        }
        assert_eq!(hello_version(&[]), None);
        assert_eq!(hello_version(&[0]), None);
    }

    #[test]
    fn fuzz_tcp_decoding() {
        let mut rng = StdRng::seed_from_u64(11);
        let valid: Vec<Vec<u8>> = all_msgs().iter().map(encode_tcp).collect();
        for i in 0..50_000 {
            let len = rng.random_range(0..64);
            let mut buf = if i % 2 == 0 {
                let mut b = vec![0u8; len];
                rng.fill_bytes(&mut b);
                b
            } else {
                // Mutate a valid message so the fuzzer gets past the first bytes.
                let mut b = valid[rng.random_range(0..valid.len())].clone();
                for _ in 0..rng.random_range(1..4) {
                    let p = rng.random_range(0..b.len());
                    b[p] = rng.random();
                }
                b
            };
            let _ = decode_tcp_body(&buf);
            if buf.len() >= 4 && i % 4 == 1 {
                let body = (buf.len() - 4) as u32;
                buf[..4].copy_from_slice(&body.to_le_bytes());
            }
            let _ = read_tcp(&mut Cursor::new(&buf));
        }
    }

    #[test]
    fn udp_layout_is_exact() {
        let hdr = UdpHeader {
            kind: UdpKind::Game,
            token: 0x0403_0201,
            seq: 0x0605,
            ts: 0x0a09_0807,
        };
        let mut out = vec![9, 9, 9];
        encode_udp(&hdr, &[0xaa, 0xbb], &mut out);
        assert_eq!(out, [0x47, 2, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0xaa, 0xbb]);
        let (h, p) = decode_udp(&out).unwrap();
        assert_eq!(h, hdr);
        assert_eq!(p, &[0xaa, 0xbb]);
    }

    #[test]
    fn udp_round_trip_all_kinds() {
        let mut out = Vec::new();
        for kind in [UdpKind::Voice, UdpKind::Game, UdpKind::Ping, UdpKind::Pong] {
            let hdr = UdpHeader {
                kind,
                token: 77,
                seq: u16::MAX,
                ts: 480,
            };
            let payload = 1234u64.to_le_bytes();
            encode_udp(&hdr, &payload, &mut out);
            assert_eq!(out.len(), UDP_HEADER_LEN + 8);
            assert_eq!(decode_udp(&out), Some((hdr, &payload[..])));
            encode_udp(&hdr, &[], &mut out);
            assert_eq!(decode_udp(&out), Some((hdr, &[][..])));
        }
    }

    #[test]
    fn udp_rejects_malformed() {
        let hdr = UdpHeader {
            kind: UdpKind::Voice,
            token: 1,
            seq: 2,
            ts: 3,
        };
        let mut good = Vec::new();
        encode_udp(&hdr, &[1, 2, 3], &mut good);
        for cut in 0..UDP_HEADER_LEN {
            assert_eq!(decode_udp(&good[..cut]), None);
        }
        let mut bad = good.clone();
        bad[0] = 0x48;
        assert_eq!(decode_udp(&bad), None);
        for kind in [0u8, 5, 255] {
            let mut bad = good.clone();
            bad[1] = kind;
            assert_eq!(decode_udp(&bad), None);
        }
    }

    #[test]
    fn fuzz_udp_decoding() {
        let mut rng = StdRng::seed_from_u64(5);
        let mut accepted = 0;
        for _ in 0..200_000 {
            let mut b = vec![0u8; rng.random_range(0..40)];
            rng.fill_bytes(&mut b);
            if rng.random_bool(0.5) && !b.is_empty() {
                b[0] = UDP_MAGIC;
            }
            if let Some((h, p)) = decode_udp(&b) {
                accepted += 1;
                assert_eq!(p.len(), b.len() - UDP_HEADER_LEN);
                let mut re = Vec::new();
                encode_udp(&h, p, &mut re);
                assert_eq!(re, b);
            }
        }
        assert!(accepted > 0);
    }
}
