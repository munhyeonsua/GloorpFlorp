//! The connection between the two PCs. Plain std threads, no async runtime.
//!
//! Tailscale already encrypts and authenticates everything (WireGuard), so
//! there is no extra TLS. The room secret only stops accidental joins.
//!
//! Host: listens on `bind_ip:port` (its Tailscale address only, never 0.0.0.0)
//! for TCP and UDP. Accepts one peer at a time; a second joiner gets a
//! "room full" `Reject`. After a peer leaves, keeps listening so the friend
//! can rejoin with the same code. A listener whose `accept()` keeps failing
//! for 5 s (e.g. Tailscale replaced its network adapter) ends the session
//! with [`NetEvent::Error`].
//!
//! Joiner: connects (5 s timeout), sends `Hello`, waits for `Welcome`. The
//! first attempt retries "room full" for 12 s (after a crash or sleep the
//! host still holds the old connection until it times out). If an
//! established connection drops, retries every 2 s for 30 s
//! (`Disconnected { will_retry: true }`), then gives up; a connection that
//! drops within 5 s of reconnecting doesn't restart the 30 s.
//!
//! Handshake checks: protocol version (read from `Hello`'s frozen start, see
//! [`gf_proto::wire::hello_version`]), secret. On mismatch the host sends
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
//! Dropped frames are counted in [`NetStats`]. Sending `ShareStarted` or
//! `ShareStopped` discards the queued frames (they belong to the old share).
//!
//! UDP: one socket. The joiner sends `Ping` datagrams every 500 ms (keeps
//! the path alive, measures RTT); the host learns the joiner's UDP address
//! from the first datagram with the right token and answers `Pong`. Both
//! sides answer pings. Datagrams with a wrong token or malformed header are
//! ignored. Audio loss is estimated from gaps in per-stream sequence numbers.
//! The host pings too once it knows the joiner's address, so both sides see
//! the round-trip time.

mod framer;
mod loss;
mod queue;
mod session;
#[cfg(test)]
mod tests;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

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
    /// Host: port to listen on for TCP and UDP; 0 lets the OS pick one
    /// (reported in [`NetEvent::Listening`]). Joiner: the host's port.
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
    /// Host is ready for the friend to join (again after each disconnect).
    /// `addr` has the real port, also when [`NetConfig::port`] was 0.
    Listening {
        addr: SocketAddr,
    },
    /// Handshake done.
    Connected {
        peer_name: String,
        peer_encoders: Vec<VideoCodec>,
    },
    /// Join refused by the host (wrong code, room full, version mismatch).
    Rejected {
        reason: String,
    },
    /// Any control message other than Hello/Welcome/Reject/Ping/Pong/Video/Bye.
    Msg(TcpMsg),
    Video(VideoFrame),
    Audio {
        stream: AudioStream,
        seq: u16,
        ts: u32,
        payload: Vec<u8>,
        /// When the UDP thread received it (the jitter buffer sizes itself
        /// from arrival times; the event may be handled a bit later).
        arrived: Instant,
    },
    /// Sent about once per second while running.
    Stats(NetStats),
    /// The peer left or the connection died. Host keeps listening afterwards
    /// (`will_retry: false`, then `Listening`). Joiner: `will_retry: true` while
    /// it tries to reconnect, `false` once it has given up or the friend left.
    Disconnected {
        reason: String,
        will_retry: bool,
    },
    /// Fatal problem (e.g. the joiner can't reach the host, or the host's
    /// network adapter went away). The session has stopped. The text is
    /// plain language for the UI.
    Error(String),
}

/// A running session. Dropping it closes everything (like [`NetTx::close`]),
/// even if clones of `tx` are still around. Because of that `Drop`, fields
/// can't be moved out; clone `events` / `tx` instead.
pub struct Net {
    pub events: Receiver<NetEvent>,
    pub tx: NetTx,
}

impl Drop for Net {
    fn drop(&mut self) {
        self.tx.close();
    }
}

/// Cheap to clone; all methods are non-blocking and safe from any thread.
/// Calls while not connected are silently ignored.
#[derive(Clone)]
pub struct NetTx {
    shared: Arc<session::Shared>,
}

impl NetTx {
    /// Control message (anything except `Video`; a `Video` message is
    /// treated like [`NetTx::send_video`]).
    pub fn send(&self, msg: TcpMsg) {
        self.shared.send(msg)
    }

    pub fn send_video(&self, frame: VideoFrame) {
        self.shared.send_video(frame)
    }

    pub fn send_audio(&self, stream: AudioStream, seq: u16, ts: u32, payload: &[u8]) {
        self.shared.send_audio(stream, seq, ts, payload)
    }

    /// Sends `Bye` and shuts the session down. Returns within about 150 ms;
    /// the background threads finish on their own. No events follow.
    pub fn close(&self) {
        self.shared.close()
    }
}

/// Starts hosting or joining. Never blocks on the network: the host binds
/// its port here (a failure is returned as a plain-language error, e.g.
/// "Another program (maybe a second GloorpFlorp window) is already using
/// port 47800. …"), the joiner connects in the background and reports the
/// outcome as an event.
pub fn start(cfg: NetConfig) -> anyhow::Result<Net> {
    start_with(cfg, session::Timing::default())
}

fn start_with(cfg: NetConfig, timing: session::Timing) -> anyhow::Result<Net> {
    let (events, shared) = session::start(cfg, timing)?;
    Ok(Net {
        events,
        tx: NetTx { shared },
    })
}
