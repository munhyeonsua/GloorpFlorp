//! The connection between the two PCs. Plain std threads, no async runtime.
//!
//! Tailscale already encrypts and authenticates everything (WireGuard), so
//! there is no extra TLS. The room secret only stops accidental joins.
//!
//! Host: listens on `bind_ip:port` (its Tailscale address only, never 0.0.0.0)
//! for TCP and UDP. Accepts one peer at a time; a second joiner gets
//! `Reject { reason: "room is full" }`. After a peer leaves, keeps listening
//! so the friend can rejoin with the same code.
//!
//! Joiner: connects (5 s timeout), sends `Hello`, waits for `Welcome`. If an
//! established connection drops, retries every 2 s for 30 s
//! (`Disconnected { will_retry: true }`), then gives up.
//!
//! Handshake checks: protocol version, secret. On mismatch the host sends
//! `Reject` with a plain-language reason and closes.
//!
//! TCP: `set_nodelay(true)`. One writer thread per connection sends control
//! messages before video. `Ping` every 2 s; if nothing is received for 10 s
//! the connection is considered dead.
//!
//! Video send queue (inside the writer): bounded by time. If more than
//! [`MAX_VIDEO_QUEUE`] frames are waiting (network slower than the encoder),
//! all queued frames are discarded and new frames are dropped until the next
//! keyframe, so the viewer catches up instead of falling further behind.
//! Dropped frames are counted in [`NetStats`].
//!
//! UDP: one socket. The joiner sends `Ping` datagrams every 500 ms (keeps
//! the path alive, measures RTT); the host learns the joiner's UDP address
//! from the first datagram with the right token and answers `Pong`. Both
//! sides answer pings. Datagrams with a wrong token or malformed header are
//! ignored. Audio loss is estimated from gaps in per-stream sequence numbers.

use std::net::{Ipv4Addr, SocketAddr};

use crossbeam_channel::Receiver;
use gf_proto::wire::TcpMsg;
use gf_proto::{AudioStream, VideoCodec, VideoFrame};

/// Above this many queued video frames the queue is flushed (≈0.5 s at 60 fps).
pub const MAX_VIDEO_QUEUE: usize = 30;

#[derive(Clone, Debug)]
pub enum Role {
    Host { bind_ip: Ipv4Addr },
    Join { host_ip: Ipv4Addr },
}

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub role: Role,
    pub port: u16,
    pub secret: u32,
    /// Display name sent to the friend.
    pub name: String,
    /// Codecs this PC can encode, best first.
    pub encoders: Vec<VideoCodec>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetStats {
    pub connected: bool,
    pub rtt_ms: Option<f32>,
    pub video_send_kbps: f32,
    pub video_recv_kbps: f32,
    pub audio_send_kbps: f32,
    pub audio_recv_kbps: f32,
    /// How far behind the outgoing video queue is, in frames.
    pub video_queue_frames: usize,
    pub video_frames_dropped: u64,
    /// Estimated incoming audio packet loss over the last few seconds, 0-100.
    pub audio_loss_pct: f32,
}

#[derive(Clone, Debug)]
pub enum NetEvent {
    /// Host is ready for the friend to join.
    Listening { addr: SocketAddr },
    /// Handshake done.
    Connected { peer_name: String, peer_encoders: Vec<VideoCodec> },
    /// Join refused by the host (wrong code, room full, version mismatch).
    Rejected { reason: String },
    /// Any control message other than Hello/Welcome/Reject/Ping/Pong/Video/Bye.
    Msg(TcpMsg),
    Video(VideoFrame),
    Audio { stream: AudioStream, seq: u16, ts: u32, payload: Vec<u8> },
    /// Sent about once per second while running.
    Stats(NetStats),
    /// The peer left or the connection died. Host keeps listening afterwards.
    Disconnected { reason: String, will_retry: bool },
    /// Fatal problem (e.g. cannot bind the port). The session has stopped.
    Error(String),
}

/// A running session. Dropping it closes everything.
pub struct Net {
    pub events: Receiver<NetEvent>,
    pub tx: NetTx,
}

/// Cheap to clone; all methods are non-blocking and safe from any thread.
/// Calls while not connected are silently ignored.
#[derive(Clone)]
pub struct NetTx {}

impl NetTx {
    /// Control message (anything except `Video`).
    pub fn send(&self, msg: TcpMsg) {
        todo!()
    }

    pub fn send_video(&self, frame: VideoFrame) {
        todo!()
    }

    pub fn send_audio(&self, stream: AudioStream, seq: u16, ts: u32, payload: &[u8]) {
        todo!()
    }

    /// Sends `Bye` and shuts the session down.
    pub fn close(&self) {
        todo!()
    }
}

pub fn start(cfg: NetConfig) -> anyhow::Result<Net> {
    todo!()
}
