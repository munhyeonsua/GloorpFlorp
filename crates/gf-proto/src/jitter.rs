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
    /// Ring of packets indexed by extended sequence number; every buffered
    /// packet lies in `[head, head + slots.len())`.
    slots: Vec<Option<Vec<u8>>>,
    /// Extended sequence number of the next packet to play.
    head: u64,
    /// Highest extended sequence number accepted so far (unwraps new seqs).
    highest: Option<u64>,
    /// Last extended sequence number played or skipped; anything at or
    /// before it is late.
    played: Option<u64>,
    buffered: usize,
    primed: bool,
    target: usize,
    min_packets: usize,
    max_packets: usize,
    jitter_ms: f64,
    last_arrival: Option<(Instant, u64)>,
    consecutive_late: u32,
    received: u64,
    lost: u64,
    late_dropped: u64,
}

/// Extra packets above the target tolerated before dropping (drift control).
const DRIFT_SLACK: usize = 3;
/// This many late packets in a row means the sender restarted its sequence
/// numbers: start over instead of discarding everything.
const RESYNC_AFTER_LATE: u32 = 50;
/// Unwrapped sequence numbers start here so they never go below zero.
const EXT_BASE: u64 = 1 << 32;

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        let frame = cfg.frame_ms.max(1);
        let min_packets = cfg.min_delay_ms.div_ceil(frame).max(1) as usize;
        let max_packets = ((cfg.max_delay_ms / frame) as usize).max(min_packets);
        let cap = (2 * (max_packets + DRIFT_SLACK) + 8).max(64).next_power_of_two();
        JitterBuffer {
            cfg: JitterConfig { frame_ms: frame, ..cfg },
            slots: vec![None; cap],
            head: 0,
            highest: None,
            played: None,
            buffered: 0,
            primed: false,
            target: min_packets,
            min_packets,
            max_packets,
            jitter_ms: 0.0,
            last_arrival: None,
            consecutive_late: 0,
            received: 0,
            lost: 0,
            late_dropped: 0,
        }
    }

    /// Adds a packet that arrived at `now`. Duplicates are ignored and not
    /// counted as received.
    pub fn push(&mut self, seq: u16, payload: Vec<u8>, now: Instant) {
        let ext = self.extend(seq);
        self.update_jitter(ext, now);

        if self.played.is_some_and(|p| ext <= p) {
            self.received += 1;
            self.late_dropped += 1;
            self.consecutive_late += 1;
            if self.consecutive_late < RESYNC_AFTER_LATE {
                return;
            }
            // The sender restarted: forget the old stream and take this
            // packet as the first of a new one.
            self.clear_stream();
            self.update_jitter(EXT_BASE + u64::from(seq), now);
            return self.push(seq, payload, now);
        }
        self.consecutive_late = 0;

        let cap = self.slots.len() as u64;
        if self.buffered == 0 && !self.primed {
            self.head = ext;
        } else if ext < self.head {
            // Only reachable while priming: an earlier packet arrived late.
            if self.highest.is_some_and(|h| h - ext >= cap) {
                self.received += 1;
                self.late_dropped += 1;
                return;
            }
            self.head = ext;
        } else if ext >= self.head + cap {
            if self.buffered > 0 {
                // Huge jump forward: drop what we have and start over there.
                self.late_dropped += self.buffered as u64;
                self.slots.iter_mut().for_each(|s| *s = None);
                self.buffered = 0;
                self.primed = false;
            }
            self.head = ext;
        }

        let slot = &mut self.slots[(ext % cap) as usize];
        if slot.is_some() {
            return;
        }
        *slot = Some(payload);
        self.buffered += 1;
        self.received += 1;
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
    }

    /// Next 10 ms of audio, called on the playout clock.
    pub fn pop(&mut self) -> Pop {
        let cap = self.slots.len() as u64;
        if !self.primed {
            if self.buffered == 0 || self.buffered < self.target {
                return Pop::Empty;
            }
            self.primed = true;
            // `head` is the oldest buffered packet; anything skipped since
            // the last one played never arrived.
            if let Some(p) = self.played {
                let gap = self.head.saturating_sub(p + 1);
                if gap < cap {
                    self.lost += gap;
                }
            }
        }
        while self.buffered > self.target + DRIFT_SLACK {
            match self.slots[(self.head % cap) as usize].take() {
                Some(_) => {
                    self.buffered -= 1;
                    self.late_dropped += 1;
                }
                None => self.lost += 1,
            }
            self.played = Some(self.head);
            self.head += 1;
        }
        if self.buffered == 0 {
            self.primed = false;
            return Pop::Empty;
        }
        let idx = (self.head % cap) as usize;
        self.played = Some(self.head);
        self.head += 1;
        match self.slots[idx].take() {
            Some(p) => {
                self.buffered -= 1;
                Pop::Frame(p)
            }
            None => {
                self.lost += 1;
                Pop::Lost { next: self.slots[(self.head % cap) as usize].clone() }
            }
        }
    }

    /// Forgets everything (new peer or new stream), including statistics.
    pub fn reset(&mut self) {
        *self = JitterBuffer::new(self.cfg);
    }

    pub fn stats(&self) -> JitterStats {
        JitterStats {
            received: self.received,
            lost: self.lost,
            late_dropped: self.late_dropped,
            delay_ms: self.target as u32 * self.cfg.frame_ms,
            buffered: self.buffered,
        }
    }

    fn extend(&self, seq: u16) -> u64 {
        match self.highest {
            None => EXT_BASE + u64::from(seq),
            Some(h) => h.wrapping_add_signed(i64::from(seq.wrapping_sub(h as u16) as i16)),
        }
    }

    fn clear_stream(&mut self) {
        self.slots.iter_mut().for_each(|s| *s = None);
        self.buffered = 0;
        self.primed = false;
        self.highest = None;
        self.played = None;
        self.last_arrival = None;
        self.consecutive_late = 0;
    }

    /// RFC 3550 interarrival jitter, using sequence numbers as the sender
    /// clock, then `target = ceil((2 * jitter + frame) / frame)` clamped to
    /// the configured range. Gaps longer than a talk-spurt pause are not
    /// counted (the sender simply wasn't sending).
    fn update_jitter(&mut self, ext: u64, now: Instant) {
        let frame = f64::from(self.cfg.frame_ms);
        if let Some((t0, e0)) = self.last_arrival {
            let dt_ms = now.saturating_duration_since(t0).as_secs_f64() * 1000.0;
            let dseq = ext as i64 - e0 as i64;
            let pause_ms = f64::from((2 * self.cfg.max_delay_ms).max(250));
            if dt_ms < pause_ms && dseq.unsigned_abs() < 100 && dseq != 0 {
                let d = dt_ms - dseq as f64 * frame;
                self.jitter_ms += (d.abs() - self.jitter_ms) / 16.0;
            }
        }
        if self.last_arrival.is_none_or(|(_, e0)| ext != e0) {
            self.last_arrival = Some((now, ext));
        }
        let wanted = ((2.0 * self.jitter_ms + frame) / frame).ceil() as usize;
        self.target = wanted.clamp(self.min_packets, self.max_packets);
    }
}
