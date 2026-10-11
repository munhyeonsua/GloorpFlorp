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
    Reject { reason: String },
    /// Keepalive, sent every 2 s by both sides; answered with `Pong`.
    Ping { nonce: u64 },
    Pong { nonce: u64 },
    /// Sender started sharing its screen; frames follow.
    ShareStarted {
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
    },
    ShareStopped,
    /// Viewer tells the sharer whether it wants frames (viewer window open).
    Watching { on: bool },
    Video(VideoFrame),
    /// Sender started/stopped sending game audio.
    GameAudio { active: bool },
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
    todo!()
}

/// Blocking read of one message from a stream.
pub fn read_tcp(r: &mut impl std::io::Read) -> Result<TcpMsg, WireError> {
    todo!()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpKind {
    Voice = 1,
    Game = 2,
    Ping = 3,
    Pong = 4,
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
    todo!()
}

/// Returns the header and payload slice, or `None` for anything malformed.
pub fn decode_udp(buf: &[u8]) -> Option<(UdpHeader, &[u8])> {
    todo!()
}
