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
//! - Running dry: if the buffer empties while playing (a packet is late, or
//!   the talk spurt ended), `pop` returns `Lost { next: None }` for up to
//!   ~50 ms so the decoder conceals and fades out instead of cutting to
//!   silence. No sequence number is used up meanwhile: a packet that turns up
//!   then is played normally (a little later), and the extra delay is the
//!   cushion that stops it happening again. These frames don't count as lost.
//! - Silence: after that, `pop` returns `Empty` and the buffer re-primes when
//!   packets resume (this is normal with push-to-talk).
//! - Drift: if more than `target + 3` packets are buffered, the oldest are
//!   dropped (counted as `late_dropped`) to bring latency back down. The last
//!   one dropped is handed out as `Skip`, so the decoder stays in step and
//!   the jump can be cross-faded.
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
        JitterConfig {
            frame_ms: 10,
            min_delay_ms: 20,
            max_delay_ms: 200,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pop {
    /// Next packet in order: decode it normally.
    Frame(Vec<u8>),
    /// Expected packet is missing (or late, while the buffer has run dry):
    /// conceal it. `next` is the packet after it, if already here, for FEC
    /// decoding; it is *not* consumed.
    Lost { next: Option<Vec<u8>> },
    /// Nothing to play (priming or silence): output silence, don't run PLC.
    Empty,
    /// The buffer skipped ahead to cut the delay; this is the last packet
    /// skipped. Decode it without playing it (keeps the decoder's state
    /// continuous), then call `pop` again for this frame's audio.
    Skip(Vec<u8>),
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
    /// Frames concealed in a row because the buffer ran dry while playing.
    dry: usize,
    /// How many of those before giving up and going silent.
    dry_limit: usize,
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
/// How long to conceal when the buffer runs dry while playing.
const CONCEAL_DRY_MS: u32 = 50;
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
        let cap = (2 * (max_packets + DRIFT_SLACK) + 8)
            .max(64)
            .next_power_of_two();
        JitterBuffer {
            cfg: JitterConfig {
                frame_ms: frame,
                ..cfg
            },
            slots: vec![None; cap],
            head: 0,
            highest: None,
            played: None,
            buffered: 0,
            primed: false,
            dry: 0,
            dry_limit: CONCEAL_DRY_MS.div_ceil(frame).max(1) as usize,
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
            // From now on, anything before `head` is late.
            self.played = Some(self.head - 1);
            self.dry = 0;
        }
        if self.buffered > self.target + DRIFT_SLACK {
            // Too much delay: skip the oldest packets. The loop ends on a
            // packet (only those count in `buffered`), which is handed out.
            loop {
                let slot = self.slots[(self.head % cap) as usize].take();
                self.played = Some(self.head);
                self.head += 1;
                match slot {
                    Some(p) => {
                        self.buffered -= 1;
                        self.late_dropped += 1;
                        if self.buffered <= self.target + DRIFT_SLACK {
                            return Pop::Skip(p);
                        }
                    }
                    None => self.lost += 1,
                }
            }
        }
        if self.buffered == 0 {
            if self.dry < self.dry_limit {
                // Late packet or end of the talk spurt: conceal for now,
                // without giving up on the packet we're waiting for.
                self.dry += 1;
                return Pop::Lost { next: None };
            }
            self.primed = false;
            return Pop::Empty;
        }
        self.dry = 0;
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
                Pop::Lost {
                    next: self.slots[(self.head % cap) as usize].clone(),
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::time::Duration;

    struct Clock(Instant);
    impl Clock {
        fn at(&self, ms: u64) -> Instant {
            self.0 + Duration::from_millis(ms)
        }
    }

    fn pkt(seq: u16) -> Vec<u8> {
        seq.to_le_bytes().to_vec()
    }

    fn frame(seq: u16) -> Pop {
        Pop::Frame(pkt(seq))
    }

    /// Pushes `seqs` arriving 10 ms apart starting at `start_ms`.
    fn push_all(jb: &mut JitterBuffer, clock: &Clock, start_ms: u64, seqs: &[u16]) {
        for (i, &s) in seqs.iter().enumerate() {
            jb.push(s, pkt(s), clock.at(start_ms + 10 * i as u64));
        }
    }

    /// The buffer ran dry while playing: it conceals for ~50 ms (not counted
    /// as loss), then goes silent.
    fn assert_runs_dry(jb: &mut JitterBuffer) {
        let lost = jb.stats().lost;
        assert_eq!(jb.dry_limit, 5);
        for i in 0..jb.dry_limit {
            assert_eq!(jb.pop(), Pop::Lost { next: None }, "concealed frame {i}");
        }
        assert_eq!(jb.pop(), Pop::Empty);
        assert_eq!(jb.stats().lost, lost);
    }

    #[test]
    fn in_order_with_priming() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.stats().delay_ms, 20);
        assert_eq!(jb.pop(), Pop::Empty);
        jb.push(0, pkt(0), clock.at(0));
        assert_eq!(jb.pop(), Pop::Empty, "still priming with 1 of 2 packets");
        for s in 1..100u16 {
            jb.push(s, pkt(s), clock.at(10 * s as u64));
            assert_eq!(jb.pop(), frame(s - 1));
        }
        let st = jb.stats();
        assert_eq!(
            (st.received, st.lost, st.late_dropped, st.buffered),
            (100, 0, 0, 1)
        );
        assert_eq!(st.delay_ms, 20);
    }

    #[test]
    fn reorders() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[1, 0, 3, 2, 4]);
        for s in 0..5 {
            assert_eq!(jb.pop(), frame(s));
        }
        assert_eq!(jb.stats().lost, 0);
        assert_runs_dry(&mut jb);
    }

    #[test]
    fn loss_offers_next_packet_for_fec_without_consuming_it() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 1, 3, 4]);
        assert_eq!(jb.pop(), frame(0));
        assert_eq!(jb.pop(), frame(1));
        assert_eq!(jb.pop(), Pop::Lost { next: Some(pkt(3)) });
        assert_eq!(jb.stats().buffered, 2);
        assert_eq!(jb.pop(), frame(3));
        assert_eq!(jb.pop(), frame(4));
        assert_eq!(jb.stats().lost, 1);
    }

    #[test]
    fn burst_loss() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 1, 4]);
        assert_eq!(jb.pop(), frame(0));
        assert_eq!(jb.pop(), frame(1));
        assert_eq!(jb.pop(), Pop::Lost { next: None });
        assert_eq!(jb.pop(), Pop::Lost { next: Some(pkt(4)) });
        assert_eq!(jb.pop(), frame(4));
        assert_eq!(jb.stats().lost, 2);
    }

    #[test]
    fn sequence_wraparound() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let seqs: Vec<u16> = (0..20u16).map(|i| 65530u16.wrapping_add(i)).collect();
        jb.push(seqs[0], pkt(seqs[0]), clock.at(0));
        for (i, w) in seqs.windows(2).enumerate() {
            jb.push(w[1], pkt(w[1]), clock.at(10 * (i as u64 + 1)));
            assert_eq!(jb.pop(), frame(w[0]));
        }
        // Reordering across the wrap.
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[65534, 0, 65535, 1]);
        for s in [65534, 65535, 0, 1] {
            assert_eq!(jb.pop(), frame(s));
        }
        assert_eq!(jb.stats().lost, 0);
    }

    #[test]
    fn duplicates_ignored() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 1, 1, 0, 2]);
        assert_eq!(jb.stats().received, 3);
        assert_eq!(jb.pop(), frame(0));
        assert_eq!(jb.pop(), frame(1));
        assert_eq!(jb.pop(), frame(2));
        assert_runs_dry(&mut jb);
    }

    #[test]
    fn late_packets_discarded() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 2, 3]);
        assert_eq!(jb.pop(), frame(0));
        assert_eq!(jb.pop(), Pop::Lost { next: Some(pkt(2)) });
        jb.push(1, pkt(1), clock.at(40));
        jb.push(0, pkt(0), clock.at(41));
        assert_eq!(jb.stats().late_dropped, 2);
        assert_eq!(jb.pop(), frame(2));
        assert_eq!(jb.pop(), frame(3));
        assert_runs_dry(&mut jb);
    }

    #[test]
    fn silence_then_reprime() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 1, 2, 3, 4]);
        for s in 0..5 {
            assert_eq!(jb.pop(), frame(s));
        }
        // Push-to-talk released: the buffer runs dry.
        assert_runs_dry(&mut jb);
        for _ in 0..50 {
            assert_eq!(jb.pop(), Pop::Empty);
        }
        jb.push(5, pkt(5), clock.at(3000));
        assert_eq!(jb.pop(), Pop::Empty, "re-priming");
        jb.push(6, pkt(6), clock.at(3010));
        assert_eq!(jb.pop(), frame(5));
        jb.push(7, pkt(7), clock.at(3020));
        assert_eq!(jb.pop(), frame(6));
        let st = jb.stats();
        assert_eq!((st.lost, st.late_dropped), (0, 0));
        assert_eq!(st.delay_ms, 20, "the pause must not count as jitter");
    }

    #[test]
    fn reprime_skips_packets_lost_during_silence() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 1]);
        assert_eq!(jb.pop(), frame(0));
        assert_eq!(jb.pop(), frame(1));
        assert_runs_dry(&mut jb);
        push_all(&mut jb, &clock, 2000, &[5, 4]);
        assert_eq!(jb.pop(), frame(4));
        assert_eq!(jb.pop(), frame(5));
        assert_eq!(jb.stats().lost, 2);
    }

    #[test]
    fn drift_drops_oldest() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // A burst of 11 packets all at once (target stays 2 since the burst
        // carries no history yet).
        for s in 0..11u16 {
            jb.push(s, pkt(s), clock.at(0));
        }
        let target = (jb.stats().delay_ms / 10) as usize;
        let keep = target + DRIFT_SLACK;
        // The last packet dropped goes to the decoder; this frame's audio
        // comes from the next pop.
        assert_eq!(jb.pop(), Pop::Skip(pkt((10 - keep) as u16)));
        assert_eq!(jb.pop(), frame((11 - keep) as u16));
        let st = jb.stats();
        assert_eq!(st.late_dropped, (11 - keep) as u64);
        assert_eq!(st.buffered, keep - 1);
        assert_eq!(st.lost, 0);
        // Back under the limit: no more skipping.
        assert_eq!(jb.pop(), frame((12 - keep) as u16));
    }

    #[test]
    fn drift_skips_over_gaps_and_hands_out_a_packet() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // 1 is missing; 9 packets buffered, 5 (target + 3) allowed.
        push_all(&mut jb, &clock, 0, &[0, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(jb.pop(), Pop::Skip(pkt(4)));
        assert_eq!(jb.pop(), frame(5));
        let st = jb.stats();
        assert_eq!((st.late_dropped, st.lost, st.buffered), (4, 1, 4));
    }

    #[test]
    fn stall_is_concealed_and_nothing_is_lost() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        jb.push(0, pkt(0), clock.at(0));
        assert_eq!(jb.pop(), Pop::Empty);
        for s in 1..10u16 {
            jb.push(s, pkt(s), clock.at(10 * s as u64));
            assert_eq!(jb.pop(), frame(s - 1));
        }
        assert_eq!(jb.pop(), frame(9));
        // The network stalls for 30 ms mid-sentence: conceal, don't go silent
        // or re-prime.
        for _ in 0..3 {
            assert_eq!(jb.pop(), Pop::Lost { next: None });
        }
        // The late packets turn up together and play in order, a bit later.
        push_all(&mut jb, &clock, 130, &[10, 11, 12]);
        for s in 10..30u16 {
            assert_eq!(jb.pop(), frame(s));
            jb.push(s + 3, pkt(s + 3), clock.at(160 + 10 * (s - 10) as u64));
        }
        let st = jb.stats();
        assert_eq!((st.lost, st.late_dropped), (0, 0));
        assert_eq!(st.buffered, 3, "the stall left a cushion behind");
    }

    #[test]
    fn packet_late_beyond_concealment_starts_a_new_spurt() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 1]);
        assert_eq!(jb.pop(), frame(0));
        assert_eq!(jb.pop(), frame(1));
        assert_runs_dry(&mut jb);
        // Seq 2 wasn't skipped: it's the start of the next spurt, not late.
        push_all(&mut jb, &clock, 500, &[2, 3]);
        assert_eq!(jb.pop(), frame(2));
        assert_eq!(jb.pop(), frame(3));
        let st = jb.stats();
        assert_eq!((st.lost, st.late_dropped), (0, 0));
    }

    #[test]
    fn no_drift_drop_at_threshold() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 1, 2, 3, 4]);
        assert_eq!(jb.pop(), frame(0), "5 = target + 3 buffered is allowed");
        assert_eq!(jb.stats().late_dropped, 0);
    }

    #[test]
    fn target_adapts_to_jitter() {
        let clock = Clock(Instant::now());
        let cfg = JitterConfig::default();
        let mut jb = JitterBuffer::new(cfg);
        // Packets arrive in pairs every 20 ms, alternating 0 / 20 ms gaps
        // (|D| = 10 ms per packet -> jitter -> 10 ms -> target 3 packets).
        let mut t = 0;
        for s in 0..400u16 {
            jb.push(s, pkt(s), clock.at(t));
            if s % 2 == 1 {
                t += 20;
            }
            jb.pop();
        }
        assert_eq!(jb.stats().delay_ms, 30);
        // Big jitter (packets in bursts of 10 every 100 ms): clamped to max.
        for s in 400..2000u16 {
            jb.push(s, pkt(s), clock.at(t));
            if s % 10 == 9 {
                t += 100;
            }
        }
        let d = jb.stats().delay_ms;
        assert!(d >= 40 && d <= cfg.max_delay_ms, "delay {d}");
        // Smooth again: back to the minimum.
        for s in 2000..2400u16 {
            t += 10;
            jb.push(s, pkt(s), clock.at(t));
        }
        assert_eq!(jb.stats().delay_ms, cfg.min_delay_ms);
    }

    #[test]
    fn target_clamped_to_max() {
        let clock = Clock(Instant::now());
        let cfg = JitterConfig {
            frame_ms: 10,
            min_delay_ms: 20,
            max_delay_ms: 50,
        };
        let mut jb = JitterBuffer::new(cfg);
        let mut t = 0;
        for s in 0..500u16 {
            jb.push(s, pkt(s), clock.at(t));
            if s % 10 == 9 {
                t += 100;
            }
        }
        assert_eq!(jb.stats().delay_ms, 50);
    }

    #[test]
    fn sender_restart_resyncs() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        for s in 1000..1100u16 {
            jb.push(s, pkt(s), clock.at(10 * s as u64));
            jb.pop();
        }
        let mut frames = 0;
        for s in 0..200u16 {
            jb.push(s, pkt(s), clock.at(20_000 + 10 * s as u64));
            if matches!(jb.pop(), Pop::Frame(_)) {
                frames += 1;
            }
        }
        assert!(frames > 100, "only {frames} frames played after restart");
    }

    #[test]
    fn big_jump_forward() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[0, 1, 2]);
        assert_eq!(jb.pop(), frame(0));
        push_all(&mut jb, &clock, 30, &[20_000, 20_001]);
        assert_eq!(jb.pop(), frame(20_000));
        assert_eq!(jb.pop(), frame(20_001));
    }

    #[test]
    fn reset_forgets_everything() {
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        push_all(&mut jb, &clock, 0, &[100, 101, 102]);
        jb.pop();
        jb.reset();
        assert_eq!(
            jb.stats(),
            JitterStats {
                delay_ms: 20,
                ..Default::default()
            }
        );
        assert_eq!(jb.pop(), Pop::Empty);
        push_all(&mut jb, &clock, 100, &[5, 6]);
        assert_eq!(jb.pop(), frame(5));
    }

    #[test]
    fn random_fuzz_never_panics_and_keeps_order() {
        let mut rng = StdRng::seed_from_u64(9);
        let clock = Clock(Instant::now());
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut last: Option<u16> = None;
        let mut seq = rng.random::<u16>();
        let mut t = 0u64;
        for _ in 0..100_000 {
            match rng.random_range(0..10) {
                0..=4 => {
                    let s = seq.wrapping_add(rng.random_range(0..6)).wrapping_sub(3);
                    if rng.random_bool(0.9) {
                        seq = seq.wrapping_add(1);
                    }
                    t += rng.random_range(0..25);
                    jb.push(s, pkt(s), clock.at(t));
                }
                5 => jb.push(rng.random(), vec![], clock.at(t)),
                _ => match jb.pop() {
                    Pop::Frame(p) | Pop::Skip(p) if p.len() == 2 => {
                        let s = u16::from_le_bytes([p[0], p[1]]);
                        if let Some(l) = last {
                            let d = s.wrapping_sub(l) as i16;
                            assert!(!(-1000..=0).contains(&d), "went backwards {l} -> {s}");
                        }
                        last = Some(s);
                    }
                    _ => {}
                },
            }
            let st = jb.stats();
            assert!(st.buffered <= jb.slots.len());
            assert!((20..=200).contains(&st.delay_ms));
        }
    }
}
