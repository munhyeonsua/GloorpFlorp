//! Incoming audio loss estimate from sequence-number gaps.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gf_proto::AudioStream;

const BUCKET: Duration = Duration::from_secs(1);
/// The estimate covers roughly the last 5 seconds.
const BUCKETS: usize = 5;
/// A jump bigger than this (≈10 s of 10 ms packets) means the sender restarted
/// its sequence numbers rather than lost packets.
const MAX_JUMP: i64 = 1000;

#[derive(Clone, Copy)]
struct Bucket {
    start: Instant,
    received: u64,
    min: i64,
    max: i64,
}

#[derive(Default)]
struct StreamLoss {
    /// Highest unwrapped sequence number seen.
    highest: Option<i64>,
    buckets: VecDeque<Bucket>,
}

impl StreamLoss {
    fn record(&mut self, seq: u16, now: Instant) {
        let ext = match self.highest {
            None => i64::from(seq),
            Some(h) => {
                let diff = i64::from(seq.wrapping_sub(h as u16) as i16);
                if diff.abs() > MAX_JUMP {
                    self.buckets.clear();
                    self.highest = None;
                    i64::from(seq)
                } else {
                    h + diff
                }
            }
        };
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        self.prune(now);
        match self.buckets.back_mut() {
            Some(b) if now.duration_since(b.start) < BUCKET => {
                b.received += 1;
                b.min = b.min.min(ext);
                b.max = b.max.max(ext);
            }
            _ => self.buckets.push_back(Bucket {
                start: now,
                received: 1,
                min: ext,
                max: ext,
            }),
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .buckets
            .front()
            .is_some_and(|b| now.duration_since(b.start) >= BUCKET * BUCKETS as u32)
        {
            self.buckets.pop_front();
        }
    }

    /// (expected, received) over the window.
    fn counts(&mut self, now: Instant) -> (u64, u64) {
        self.prune(now);
        let received: u64 = self.buckets.iter().map(|b| b.received).sum();
        let (Some(min), Some(max)) = (
            self.buckets.iter().map(|b| b.min).min(),
            self.buckets.iter().map(|b| b.max).max(),
        ) else {
            return (0, 0);
        };
        ((max - min + 1) as u64, received)
    }
}

/// Tracks both incoming audio streams.
#[derive(Default)]
pub(crate) struct LossMeter {
    voice: StreamLoss,
    game: StreamLoss,
}

impl LossMeter {
    pub fn record(&mut self, stream: AudioStream, seq: u16, now: Instant) {
        match stream {
            AudioStream::Voice => self.voice.record(seq, now),
            AudioStream::Game => self.game.record(seq, now),
        }
    }

    /// Estimated loss over both streams in the last few seconds, 0-100.
    pub fn loss_pct(&mut self, now: Instant) -> f32 {
        let (e1, r1) = self.voice.counts(now);
        let (e2, r2) = self.game.counts(now);
        let expected = e1 + e2;
        if expected == 0 {
            return 0.0;
        }
        let lost = expected.saturating_sub(r1 + r2);
        (lost as f64 * 100.0 / expected as f64) as f32
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS10: Duration = Duration::from_millis(10);

    #[test]
    fn no_packets_no_loss() {
        let mut m = LossMeter::default();
        assert_eq!(m.loss_pct(Instant::now()), 0.0);
    }

    #[test]
    fn counts_gaps_across_wrap() {
        let mut m = LossMeter::default();
        let t0 = Instant::now();
        // 200 packets spanning the u16 wrap, every 10th lost.
        for i in 0..200u32 {
            if i % 10 == 5 {
                continue;
            }
            let seq = (65_500 + i) as u16;
            m.record(AudioStream::Voice, seq, t0 + MS10 * i);
        }
        let pct = m.loss_pct(t0 + MS10 * 200);
        assert!((pct - 10.0).abs() < 0.01, "{pct}");
    }

    #[test]
    fn reordering_and_streams_combine() {
        let mut m = LossMeter::default();
        let t0 = Instant::now();
        for (i, seq) in [0u16, 2, 1, 3, 5, 4].into_iter().enumerate() {
            m.record(AudioStream::Voice, seq, t0 + MS10 * i as u32);
        }
        // Game: 10..=19 with two missing.
        for seq in (10u16..20).filter(|s| *s != 12 && *s != 17) {
            m.record(AudioStream::Game, seq, t0);
        }
        let pct = m.loss_pct(t0 + MS10 * 10);
        assert!((pct - 2.0 / 16.0 * 100.0).abs() < 0.01, "{pct}");
    }

    #[test]
    fn old_loss_leaves_the_window() {
        let mut m = LossMeter::default();
        let t0 = Instant::now();
        // Heavy loss early on...
        for i in (0..100u32).step_by(2) {
            m.record(AudioStream::Game, i as u16, t0 + MS10 * i);
        }
        assert!(m.loss_pct(t0 + Duration::from_secs(1)) > 40.0);
        // ...then 6 s of clean packets.
        for i in 100..700u32 {
            m.record(AudioStream::Game, i as u16, t0 + MS10 * i);
        }
        assert_eq!(m.loss_pct(t0 + MS10 * 700), 0.0);
        // Everything ages out after silence.
        assert_eq!(m.loss_pct(t0 + Duration::from_secs(20)), 0.0);
    }

    #[test]
    fn sender_restart_is_not_loss() {
        let mut m = LossMeter::default();
        let t0 = Instant::now();
        for i in 0..50u16 {
            m.record(AudioStream::Voice, 30_000 + i, t0);
        }
        for i in 0..50u16 {
            m.record(AudioStream::Voice, i, t0 + MS10);
        }
        assert_eq!(m.loss_pct(t0 + MS10 * 2), 0.0);
        // Duplicates never make loss negative.
        for _ in 0..10 {
            m.record(AudioStream::Voice, 49, t0 + MS10 * 3);
        }
        assert_eq!(m.loss_pct(t0 + MS10 * 4), 0.0);
        m.reset();
        assert_eq!(m.loss_pct(t0 + MS10 * 4), 0.0);
    }
}
