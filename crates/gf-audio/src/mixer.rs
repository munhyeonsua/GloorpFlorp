//! Mixing the friend's voice and game audio, and fitting the result to the
//! output device's channel layout.

use gf_proto::AUDIO_FRAME_SAMPLES;

/// Playback settings read once per frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MixParams {
    pub voice_volume: f32,
    pub game_volume: f32,
    pub deafened: bool,
}

impl Default for MixParams {
    fn default() -> Self {
        MixParams {
            voice_volume: 1.0,
            game_volume: 1.0,
            deafened: false,
        }
    }
}

/// Volume setting to a safe linear gain (0..=2, anything odd counts as 1).
pub(crate) fn sanitize_volume(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 2.0)
    } else {
        1.0
    }
}

/// Mixes one 10 ms frame: `voice` is mono (480), `game` and `out` are
/// interleaved stereo (960). Voice goes to both ears. The result is clipped
/// to -1..1.
pub(crate) fn mix(voice: &[f32], game: &[f32], p: MixParams, out: &mut [f32]) {
    debug_assert_eq!(voice.len(), AUDIO_FRAME_SAMPLES);
    debug_assert_eq!(game.len(), AUDIO_FRAME_SAMPLES * 2);
    debug_assert_eq!(out.len(), AUDIO_FRAME_SAMPLES * 2);
    if p.deafened {
        out.fill(0.0);
        return;
    }
    let (vv, gv) = (
        sanitize_volume(p.voice_volume),
        sanitize_volume(p.game_volume),
    );
    for ((o, g), &v) in out
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(game.as_chunks::<2>().0)
        .zip(voice)
    {
        o[0] = (v * vv + g[0] * gv).clamp(-1.0, 1.0);
        o[1] = (v * vv + g[1] * gv).clamp(-1.0, 1.0);
    }
}

/// Stereo to mono (the echo canceller's reference).
pub(crate) fn downmix(stereo: &[f32], mono: &mut [f32]) {
    for (m, s) in mono.iter_mut().zip(stereo.as_chunks::<2>().0) {
        *m = 0.5 * (s[0] + s[1]);
    }
}

/// Interleaved stereo to a device with `channels` channels: mono devices get
/// the average, stereo is copied, and larger layouts get left/right on the
/// first two (front) channels and silence elsewhere.
pub(crate) fn spread(stereo: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    match channels {
        0 => {}
        1 => out.extend(
            stereo
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| 0.5 * (s[0] + s[1])),
        ),
        2 => out.extend_from_slice(stereo),
        n => {
            for s in stereo.as_chunks::<2>().0 {
                out.extend_from_slice(s);
                out.extend(std::iter::repeat_n(0.0, n - 2));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = AUDIO_FRAME_SAMPLES;

    #[test]
    fn volumes_scale_each_stream() {
        let voice = vec![0.2f32; N];
        let game: Vec<f32> = (0..N).flat_map(|_| [0.1f32, -0.1]).collect();
        let mut out = vec![0f32; 2 * N];
        mix(&voice, &game, MixParams::default(), &mut out);
        assert!(out
            .chunks(2)
            .all(|f| (f[0] - 0.3).abs() < 1e-6 && (f[1] - 0.1).abs() < 1e-6));

        let p = MixParams {
            voice_volume: 0.5,
            game_volume: 2.0,
            deafened: false,
        };
        mix(&voice, &game, p, &mut out);
        assert!(out
            .chunks(2)
            .all(|f| (f[0] - 0.3).abs() < 1e-6 && (f[1] + 0.1).abs() < 1e-6));

        let p = MixParams {
            voice_volume: 0.0,
            game_volume: 0.0,
            deafened: false,
        };
        mix(&voice, &game, p, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn deafen_silences_everything() {
        let mut out = vec![1f32; 2 * N];
        let p = MixParams {
            deafened: true,
            ..Default::default()
        };
        mix(&[0.5; N], &[0.5; 2 * N], p, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn clips_and_sanitizes() {
        let mut out = vec![0f32; 2 * N];
        let p = MixParams {
            voice_volume: 2.0,
            game_volume: f32::NAN,
            deafened: false,
        };
        mix(&[0.9; N], &[-0.5; 2 * N], p, &mut out);
        assert!(
            out.iter().all(|&s| (s - 1.0).abs() < 1e-6),
            "1.8 - 0.5 clips to 1"
        );
        assert_eq!(sanitize_volume(-1.0), 0.0);
        assert_eq!(sanitize_volume(5.0), 2.0);
        assert_eq!(sanitize_volume(f32::INFINITY), 1.0);
    }

    #[test]
    fn channel_layouts() {
        let stereo = [0.2f32, 0.4, -0.2, -0.4];
        let mut out = Vec::new();
        spread(&stereo, 1, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.3).abs() < 1e-6 && (out[1] + 0.3).abs() < 1e-6);
        spread(&stereo, 2, &mut out);
        assert_eq!(out, stereo);
        spread(&stereo, 6, &mut out);
        assert_eq!(
            out,
            [0.2, 0.4, 0.0, 0.0, 0.0, 0.0, -0.2, -0.4, 0.0, 0.0, 0.0, 0.0]
        );
        spread(&stereo, 0, &mut out);
        assert!(out.is_empty());

        let mut mono = [0f32; 2];
        downmix(&stereo, &mut mono);
        assert!((mono[0] - 0.3).abs() < 1e-6 && (mono[1] + 0.3).abs() < 1e-6);
    }
}
