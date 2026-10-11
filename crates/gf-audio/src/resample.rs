//! Streaming sample-rate conversion between device rates and our 48 kHz.
//! Takes any number of input samples per call and hands out whatever whole
//! output chunks are ready. Allocates only when created.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler as _};

pub(crate) struct Resampler {
    /// `None` when the rates match (samples pass straight through).
    fft: Option<Box<Fft<f32>>>,
    channels: usize,
    /// Interleaved input collected until a full chunk is there.
    pending: Vec<f32>,
    chunk_samples: usize,
    out: Vec<f32>,
}

/// About 10 ms of input per chunk, rounded to a whole number of the FFT's
/// smallest block for this rate pair so no input waits for the next chunk
/// (keeps the added delay to roughly one chunk).
fn chunk_frames(from: usize, to: usize) -> usize {
    let gcd = {
        let (mut a, mut b) = (from, to);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    let block = from / gcd;
    let ten_ms = (from / 100).max(1);
    if block <= 2 * ten_ms {
        ((ten_ms + block / 2) / block).max(1) * block
    } else {
        // Unusual rate pair with a huge minimum block: rubato copes.
        ten_ms
    }
}

impl Resampler {
    /// Interleaved `channels`-channel audio from `from` Hz to `to` Hz.
    pub fn new(from: u32, to: u32, channels: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            from > 0 && to > 0 && channels > 0,
            "unusable audio format ({from} Hz → {to} Hz, {channels} channels)"
        );
        if from == to {
            return Ok(Resampler {
                fft: None,
                channels,
                pending: Vec::new(),
                chunk_samples: 0,
                out: Vec::new(),
            });
        }
        let chunk = chunk_frames(from as usize, to as usize);
        let fft = Fft::<f32>::new(
            from as usize,
            to as usize,
            chunk,
            channels,
            FixedSync::Input,
        )?;
        let out = vec![0.0; fft.output_frames_max() * channels];
        Ok(Resampler {
            fft: Some(Box::new(fft)),
            channels,
            pending: Vec::with_capacity(chunk * channels),
            chunk_samples: chunk * channels,
            out,
        })
    }

    /// Feeds interleaved samples (a whole number of frames) and calls `emit`
    /// with each block of resampled interleaved output as it becomes ready.
    pub fn process(&mut self, mut input: &[f32], mut emit: impl FnMut(&[f32])) {
        let Some(fft) = self.fft.as_mut() else {
            emit(input);
            return;
        };
        while !input.is_empty() {
            let take = (self.chunk_samples - self.pending.len()).min(input.len());
            self.pending.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.pending.len() < self.chunk_samples {
                break;
            }
            let frames_in = self.chunk_samples / self.channels;
            let frames_out = self.out.len() / self.channels;
            let produced = match (
                InterleavedSlice::new(&self.pending[..], self.channels, frames_in),
                InterleavedSlice::new_mut(&mut self.out[..], self.channels, frames_out),
            ) {
                (Ok(src), Ok(mut dst)) => fft.process_into_buffer(&src, &mut dst, None),
                _ => Ok((0, 0)),
            };
            self.pending.clear();
            match produced {
                Ok((_, n)) => emit(&self.out[..n * self.channels]),
                Err(e) => log::debug!("resampler error: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::tests::zero_crossing_hz;

    fn tone(rate: u32, hz: f64, secs: f64, channels: usize) -> Vec<f32> {
        let n = (f64::from(rate) * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let s =
                    (0.5 * (i as f64 * hz * std::f64::consts::TAU / f64::from(rate)).sin()) as f32;
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    #[test]
    fn chunking_at_common_rates() {
        for (from, to, channels) in [
            (44_100, 48_000, 1),
            (48_000, 48_000, 1),
            (96_000, 48_000, 1),
            (48_000, 44_100, 2),
            (48_000, 96_000, 2),
            (22_050, 48_000, 1),
        ] {
            let input = tone(from, 1000.0, 1.0, channels);
            let mut rs = Resampler::new(from, to, channels).unwrap();
            let mut out = Vec::new();
            // Irregular piece sizes, like a sound card would deliver.
            let mut rest = &input[..];
            let mut step = 7usize;
            while !rest.is_empty() {
                let n = (step * channels).min(rest.len());
                rs.process(&rest[..n], |c| out.extend_from_slice(c));
                rest = &rest[n..];
                step = (step * 31 + 13) % 997 + 1;
            }
            assert_eq!(out.len() % channels, 0);
            let frames = out.len() / channels;
            let expected = f64::from(to);
            // Up to one chunk can still be waiting inside.
            assert!(
                (frames as f64 - expected).abs() <= expected / 50.0 + 1.0,
                "{from}->{to}: {frames} frames"
            );
            // Skip the resampler's start-up delay.
            let tail = &out[(to as usize / 50) * channels..];
            let hz = zero_crossing_hz(tail, channels, to);
            assert!((hz - 1000.0).abs() < 5.0, "{from}->{to}: {hz} Hz");
            let peak = tail.iter().fold(0f32, |m, s| m.max(s.abs()));
            assert!((peak - 0.5).abs() < 0.05, "{from}->{to}: peak {peak}");
        }
    }

    #[test]
    fn chunk_sizes() {
        assert_eq!(chunk_frames(44_100, 48_000), 441);
        assert_eq!(chunk_frames(48_000, 44_100), 480);
        assert_eq!(chunk_frames(96_000, 48_000), 960);
        assert_eq!(chunk_frames(22_050, 48_000), 147);
        assert_eq!(chunk_frames(48_000, 22_050), 640);
        assert_eq!(chunk_frames(44_056, 48_000), 440);
    }

    #[test]
    fn rejects_bad_formats() {
        assert!(Resampler::new(0, 48_000, 1).is_err());
        assert!(Resampler::new(48_000, 48_000, 0).is_err());
    }
}
