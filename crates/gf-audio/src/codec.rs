//! Thin Opus wrappers fixed to our format: 48 kHz, 10 ms frames, in-band FEC.

use anyhow::Context;
use gf_proto::{AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE};
use opus::{Application, Bitrate, Channels};

/// Big enough for any single Opus frame (the format caps one frame at 1275 bytes).
pub(crate) const MAX_PACKET: usize = 1500;
/// Loss rate the encoder plans its in-band FEC for.
const EXPECTED_LOSS_PERCENT: i32 = 5;

fn opus_channels(channels: usize) -> Channels {
    if channels == 1 {
        Channels::Mono
    } else {
        Channels::Stereo
    }
}

pub(crate) struct OpusEncoder {
    enc: opus::Encoder,
    channels: usize,
    bitrate_kbps: u32,
}

impl OpusEncoder {
    /// Mono, tuned for speech.
    pub fn voice(bitrate_kbps: u32) -> anyhow::Result<Self> {
        Self::new(1, Application::Voip, bitrate_kbps)
    }

    /// Stereo, tuned for music and effects.
    pub fn game(bitrate_kbps: u32) -> anyhow::Result<Self> {
        Self::new(2, Application::Audio, bitrate_kbps)
    }

    fn new(channels: usize, app: Application, bitrate_kbps: u32) -> anyhow::Result<Self> {
        let mut enc = opus::Encoder::new(AUDIO_SAMPLE_RATE, opus_channels(channels), app)
            .context("couldn't create the Opus encoder")?;
        enc.set_inband_fec(true)?;
        enc.set_packet_loss_perc(EXPECTED_LOSS_PERCENT)?;
        let mut e = OpusEncoder {
            enc,
            channels,
            bitrate_kbps: 0,
        };
        e.set_bitrate(bitrate_kbps);
        Ok(e)
    }

    /// Changes the target bitrate (clamped to what Opus supports). Cheap when unchanged.
    pub fn set_bitrate(&mut self, kbps: u32) {
        let kbps = kbps.clamp(6, 510);
        if kbps == self.bitrate_kbps {
            return;
        }
        match self.enc.set_bitrate(Bitrate::Bits(kbps as i32 * 1000)) {
            Ok(()) => self.bitrate_kbps = kbps,
            Err(e) => log::warn!("couldn't set the Opus bitrate to {kbps} kbps: {e}"),
        }
    }

    /// Encodes one 10 ms frame (`480 * channels` interleaved samples) into `out`.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> anyhow::Result<usize> {
        anyhow::ensure!(
            pcm.len() == AUDIO_FRAME_SAMPLES * self.channels,
            "Opus encoder got {} samples, expected {}",
            pcm.len(),
            AUDIO_FRAME_SAMPLES * self.channels
        );
        Ok(self.enc.encode_float(pcm, out)?)
    }
}

pub(crate) struct OpusDecoder {
    dec: opus::Decoder,
    channels: usize,
}

impl OpusDecoder {
    pub fn new(channels: usize) -> anyhow::Result<Self> {
        let dec = opus::Decoder::new(AUDIO_SAMPLE_RATE, opus_channels(channels))
            .context("couldn't create the Opus decoder")?;
        Ok(OpusDecoder { dec, channels })
    }

    fn frame_len(&self) -> usize {
        AUDIO_FRAME_SAMPLES * self.channels
    }

    /// Decodes `packet` into exactly one frame (`out.len()` must be
    /// `480 * channels`). A shorter packet is padded with silence; a corrupt
    /// or over-long one is concealed instead. Returns false if it was concealed.
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> bool {
        debug_assert_eq!(out.len(), self.frame_len());
        if packet.is_empty() {
            self.conceal(out);
            return false;
        }
        match self.dec.decode_float(packet, out, false) {
            Ok(n) => {
                let decoded = (n * self.channels).min(out.len());
                out[decoded..].fill(0.0);
                true
            }
            Err(e) => {
                log::trace!("Opus packet rejected ({e}); concealing");
                self.conceal(out);
                false
            }
        }
    }

    /// Recovers the frame *before* `next` from `next`'s in-band FEC data
    /// (falls back to concealment if it has none).
    pub fn decode_fec(&mut self, next: &[u8], out: &mut [f32]) {
        debug_assert_eq!(out.len(), self.frame_len());
        if next.is_empty() || self.dec.decode_float(next, out, true).is_err() {
            self.conceal(out);
        }
    }

    /// Packet-loss concealment: extrapolates one frame from what came before.
    pub fn conceal(&mut self, out: &mut [f32]) {
        debug_assert_eq!(out.len(), self.frame_len());
        if self.dec.decode_float(&[], out, false).is_err() {
            out.fill(0.0);
        }
    }

    pub fn reset(&mut self) {
        if let Err(e) = self.dec.reset_state() {
            log::debug!("Opus decoder reset failed: {e}");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sine(hz: f64, amp: f32, start: usize, len: usize, channels: usize) -> Vec<f32> {
        (start..start + len)
            .flat_map(|i| {
                let s = (amp as f64
                    * (i as f64 * hz * std::f64::consts::TAU / AUDIO_SAMPLE_RATE as f64).sin())
                    as f32;
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    /// Frequency estimate from zero crossings of one channel.
    pub(crate) fn zero_crossing_hz(samples: &[f32], channels: usize, rate: u32) -> f64 {
        let ch: Vec<f32> = samples.iter().step_by(channels).copied().collect();
        let crossings = ch
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count() as f64;
        crossings / 2.0 / (ch.len() as f64 / f64::from(rate))
    }

    pub(crate) fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    fn encode_tone(enc: &mut OpusEncoder, channels: usize, frames: usize) -> Vec<Vec<u8>> {
        let mut buf = [0u8; MAX_PACKET];
        (0..frames)
            .map(|f| {
                let pcm = sine(
                    440.0,
                    0.3,
                    f * AUDIO_FRAME_SAMPLES,
                    AUDIO_FRAME_SAMPLES,
                    channels,
                );
                let n = enc.encode(&pcm, &mut buf).unwrap();
                buf[..n].to_vec()
            })
            .collect()
    }

    #[test]
    fn round_trip_mono_and_stereo() {
        for (channels, mut enc) in [
            (1, OpusEncoder::voice(48).unwrap()),
            (2, OpusEncoder::game(160).unwrap()),
        ] {
            let packets = encode_tone(&mut enc, channels, 100);
            let mut dec = OpusDecoder::new(channels).unwrap();
            let mut out = vec![0f32; AUDIO_FRAME_SAMPLES * channels];
            let mut all = Vec::new();
            for p in &packets {
                assert!(p.len() > 10 && p.len() < MAX_PACKET);
                assert!(dec.decode(p, &mut out));
                all.extend_from_slice(&out);
            }
            // Skip the codec's start-up.
            let tail = &all[20 * AUDIO_FRAME_SAMPLES * channels..];
            let hz = zero_crossing_hz(tail, channels, AUDIO_SAMPLE_RATE);
            assert!((hz - 440.0).abs() < 5.0, "{channels} ch: {hz} Hz");
            let level = rms(tail);
            assert!((level - 0.3 / 2f32.sqrt()).abs() < 0.05, "rms {level}");
            if channels == 2 {
                assert!(tail.chunks(2).all(|f| (f[0] - f[1]).abs() < 0.05));
            }
        }
    }

    #[test]
    fn fec_and_plc_fill_gaps() {
        let mut enc = OpusEncoder::voice(48).unwrap();
        let packets = encode_tone(&mut enc, 1, 60);
        let mut dec = OpusDecoder::new(1).unwrap();
        let mut out = vec![0f32; AUDIO_FRAME_SAMPLES];
        for p in &packets[..40] {
            dec.decode(p, &mut out);
        }
        // Packet 40 lost: recover it from packet 41's FEC data.
        dec.decode_fec(&packets[41], &mut out);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(rms(&out) > 0.05, "FEC frame rms {}", rms(&out));
        assert!(dec.decode(&packets[41], &mut out));
        // Packets 42 and 43 lost with nothing after them: concealment.
        dec.conceal(&mut out);
        assert!(rms(&out) > 0.02, "PLC frame rms {}", rms(&out));
        dec.conceal(&mut out);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(dec.decode(&packets[44], &mut out));
        assert!(rms(&out) > 0.05);
    }

    #[test]
    fn fresh_decoder_conceals_silence() {
        let mut dec = OpusDecoder::new(2).unwrap();
        let mut out = vec![1f32; AUDIO_FRAME_SAMPLES * 2];
        dec.conceal(&mut out);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn bad_packets_never_panic() {
        let mut dec = OpusDecoder::new(1).unwrap();
        let mut out = vec![0f32; AUDIO_FRAME_SAMPLES];
        for p in [
            vec![],
            vec![0xFF],
            vec![0xFF; 2000],
            vec![0x03, 0xFF, 0xFF, 0xFF],
            (0..300u16).map(|i| (i * 7) as u8).collect(),
        ] {
            dec.decode(&p, &mut out);
            dec.decode_fec(&p, &mut out);
            assert!(out.iter().all(|s| s.is_finite()));
        }
        // A valid 20 ms packet doesn't fit a 10 ms frame: concealed, no panic.
        let mut enc = opus::Encoder::new(48_000, Channels::Mono, Application::Voip).unwrap();
        let long = enc.encode_vec_float(&[0.1; 960], MAX_PACKET).unwrap();
        assert!(!dec.decode(&long, &mut out));
        // A 5 ms packet is padded with silence.
        let short = enc.encode_vec_float(&[0.1; 240], MAX_PACKET).unwrap();
        assert!(dec.decode(&short, &mut out));
        assert!(out[240..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn wrong_frame_size_is_an_error() {
        let mut enc = OpusEncoder::voice(48).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        assert!(enc.encode(&[0.0; 100], &mut buf).is_err());
        enc.set_bitrate(0);
        enc.set_bitrate(100_000);
        assert!(enc.encode(&[0.0; 480], &mut buf).is_ok());
    }
}
