//! Audio jitter buffer: reorders packets that arrive out of order, hides
//! network timing wobble, and tells the decoder when a packet was lost.
//!
//! Count-based design: the playout side calls [`JitterBuffer::pop`] whenever
//! it needs the next 10 ms frame (driven by the sound card's clock). The
//! buffer aims to hold `target` packets; the target adapts between the
//! configured min and max from measured arrival jitter.
//!
//! - Priming: after creation, `reset`, or a talk spurt starting after silence,
//!   `pop` returns `Empty` until `target` packets are buffered.
//! - Loss: if the next expected sequence number is missing but later packets
//!   are buffered, `pop` returns `Lost` (with the following packet if present,
//!   so Opus in-band FEC can recover it) and advances.
//! - Silence: if the buffer runs dry, `pop` returns `Empty` and the buffer
//!   re-primes when packets resume (this is normal with push-to-talk).
//! - Drift: if more than `target + 3` packets are buffered, the oldest is
//!   dropped (counted as `late_dropped`) to bring latency back down.
//! - Packets older than what was already played are discarded.
//! - Sequence numbers are u16 and wrap around.

use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct JitterConfig {
    pub frame_ms: u32,
    pub min_delay_ms: u32,
    pub max_delay_ms: u32,
}

impl Default for JitterConfig {
    fn default() -> Self {
        JitterConfig { frame_ms: 10, min_delay_ms: 20, max_delay_ms: 200 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pop {
    /// Next packet in order: decode it normally.
    Frame(Vec<u8>),
    /// Expected packet is missing. `next` is the packet after it, if already
    /// here, for FEC decoding; it is *not* consumed.
    Lost { next: Option<Vec<u8>> },
    /// Nothing to play (priming or silence): output silence, don't run PLC.
    Empty,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JitterStats {
    pub received: u64,
    pub lost: u64,
    pub late_dropped: u64,
    /// Current target delay.
    pub delay_ms: u32,
    /// Packets currently buffered.
    pub buffered: usize,
}

pub struct JitterBuffer {
    cfg: JitterConfig,
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        todo!()
    }

    pub fn push(&mut self, seq: u16, payload: Vec<u8>, now: Instant) {
        todo!()
    }

    pub fn pop(&mut self) -> Pop {
        todo!()
    }

    pub fn reset(&mut self) {
        todo!()
    }

    pub fn stats(&self) -> JitterStats {
        todo!()
    }
}
