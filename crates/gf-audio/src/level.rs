//! Loudness helpers for meters and the voice-activity gate.

use std::sync::atomic::{AtomicU32, Ordering};

/// Level reported for digital silence.
pub(crate) const SILENCE_DBFS: f32 = -120.0;

/// RMS level in dB relative to full scale (a full-scale sine is about -3).
pub(crate) fn rms_dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return SILENCE_DBFS;
    }
    let ms = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    if ms.is_finite() && ms > 1e-12 {
        (10.0 * ms.log10()).max(SILENCE_DBFS)
    } else {
        SILENCE_DBFS
    }
}

/// Maps -60..0 dBFS onto 0..1 for a meter.
pub(crate) fn meter_value(dbfs: f32) -> f32 {
    ((dbfs + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Meter ballistics: jumps up at once, falls back over about half a second.
#[derive(Default)]
pub(crate) struct LevelMeter {
    value: f32,
}

impl LevelMeter {
    /// One 10 ms frame's level in, smoothed 0..1 meter value out.
    pub fn update(&mut self, dbfs: f32) -> f32 {
        let v = meter_value(dbfs);
        self.value = if v >= self.value {
            v
        } else {
            (self.value - 0.02).max(v)
        };
        self.value
    }

    pub fn reset(&mut self) {
        self.value = 0.0;
    }
}

/// An `f32` that can be shared between threads without a lock.
#[derive(Default)]
pub(crate) struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(rms_dbfs(&[]), SILENCE_DBFS);
        assert_eq!(rms_dbfs(&[0.0; 480]), SILENCE_DBFS);
        assert_eq!(rms_dbfs(&[f32::NAN; 4]), SILENCE_DBFS);
        assert!((rms_dbfs(&[1.0; 480])).abs() < 1e-4);
        assert!((rms_dbfs(&[0.1; 480]) + 20.0).abs() < 1e-3);
        assert_eq!(meter_value(-90.0), 0.0);
        assert_eq!(meter_value(-30.0), 0.5);
        assert_eq!(meter_value(6.0), 1.0);
    }

    #[test]
    fn meter_attack_and_release() {
        let mut m = LevelMeter::default();
        assert_eq!(m.update(0.0), 1.0);
        let after = m.update(SILENCE_DBFS);
        assert!(after > 0.9 && after < 1.0);
        for _ in 0..60 {
            m.update(SILENCE_DBFS);
        }
        assert_eq!(m.update(SILENCE_DBFS), 0.0);
        m.update(-6.0);
        m.reset();
        assert_eq!(m.update(SILENCE_DBFS), 0.0);
    }

    #[test]
    fn atomic_f32() {
        let a = AtomicF32::default();
        a.store(1.5);
        assert_eq!(a.load(), 1.5);
        a.store(-0.25);
        assert_eq!(a.load(), -0.25);
        assert_eq!(AtomicF32::default().load(), 0.0);
    }
}
