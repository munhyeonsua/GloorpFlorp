//! Decides, frame by frame, whether the microphone is being sent.
//! Voice activation is an energy gate on the processed signal (sonora 0.2
//! doesn't expose a voice-activity result); noise suppression and auto gain
//! keep background noise well below its threshold.

use crate::VoiceMode;

/// "Is this virtual key held?" (`gf_sys::keys::is_down`, replaceable in tests).
pub(crate) type KeyFn = Box<dyn Fn(u32) -> bool + Send>;

/// Push-to-talk keeps sending this many frames after the key is released,
/// so the end of the last word isn't cut off.
pub(crate) const PTT_TAIL_FRAMES: u32 = 15;
/// Voice activation stays open this long after the level drops.
pub(crate) const VAD_HANGOVER_FRAMES: u32 = 40;

/// Level (dBFS, after processing) that counts as speech: sensitivity 0.0 needs
/// -30 dBFS, 1.0 already opens at -55 dBFS.
pub(crate) fn vad_threshold_dbfs(sensitivity: f32) -> f32 {
    let s = if sensitivity.is_finite() {
        sensitivity.clamp(0.0, 1.0)
    } else {
        0.5
    };
    -30.0 - 25.0 * s
}

pub(crate) struct VoiceGate {
    key_down: KeyFn,
    /// Frames left before the gate closes on its own.
    hold: u32,
    mode: Option<VoiceMode>,
}

impl VoiceGate {
    pub fn new(key_down: KeyFn) -> Self {
        VoiceGate {
            key_down,
            hold: 0,
            mode: None,
        }
    }

    /// Called once per 10 ms frame with that frame's processed level.
    /// Returns true if the frame should be sent. Mute always wins.
    pub fn update(
        &mut self,
        mode: VoiceMode,
        ptt_key: u32,
        sensitivity: f32,
        muted: bool,
        level_dbfs: f32,
    ) -> bool {
        if self.mode != Some(mode) {
            self.mode = Some(mode);
            self.hold = 0;
        }
        let (triggered, tail) = match mode {
            VoiceMode::AlwaysOn => (true, 0),
            VoiceMode::PushToTalk => ((self.key_down)(ptt_key), PTT_TAIL_FRAMES),
            VoiceMode::VoiceActivity => (
                level_dbfs >= vad_threshold_dbfs(sensitivity),
                VAD_HANGOVER_FRAMES,
            ),
        };
        let open = if triggered {
            self.hold = tail;
            true
        } else if self.hold > 0 {
            self.hold -= 1;
            true
        } else {
            false
        };
        open && !muted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    const LOUD: f32 = -20.0;
    const QUIET: f32 = -70.0;

    fn gate_with_key() -> (VoiceGate, Arc<AtomicBool>) {
        let held = Arc::new(AtomicBool::new(false));
        let h = held.clone();
        let gate = VoiceGate::new(Box::new(move |vk| vk == 0x56 && h.load(Ordering::Relaxed)));
        (gate, held)
    }

    #[test]
    fn always_on() {
        let (mut g, _) = gate_with_key();
        for level in [LOUD, QUIET, f32::NAN] {
            assert!(g.update(VoiceMode::AlwaysOn, 0x56, 0.5, false, level));
        }
        assert!(!g.update(VoiceMode::AlwaysOn, 0x56, 0.5, true, LOUD));
    }

    #[test]
    fn push_to_talk_with_release_tail() {
        let (mut g, held) = gate_with_key();
        let ptt =
            |g: &mut VoiceGate, vk: u32| g.update(VoiceMode::PushToTalk, vk, 0.5, false, LOUD);
        assert!(
            !ptt(&mut g, 0x56),
            "loud speech alone doesn't open push-to-talk"
        );
        held.store(true, Ordering::Relaxed);
        assert!(!ptt(&mut g, 0x41), "another key is bound");
        for _ in 0..10 {
            assert!(ptt(&mut g, 0x56));
        }
        assert!(
            !g.update(VoiceMode::PushToTalk, 0x56, 0.5, true, LOUD),
            "mute wins"
        );
        held.store(false, Ordering::Relaxed);
        for i in 0..PTT_TAIL_FRAMES {
            assert!(ptt(&mut g, 0x56), "tail frame {i}");
        }
        assert!(!ptt(&mut g, 0x56));
        assert!(!ptt(&mut g, 0x56));
    }

    #[test]
    fn voice_activity_with_hangover() {
        let (mut g, _) = gate_with_key();
        let vad = |g: &mut VoiceGate, level: f32| {
            g.update(VoiceMode::VoiceActivity, 0x56, 0.5, false, level)
        };
        assert!(!vad(&mut g, QUIET));
        assert!(vad(&mut g, LOUD));
        for i in 0..VAD_HANGOVER_FRAMES {
            assert!(vad(&mut g, QUIET), "hangover frame {i}");
        }
        assert!(!vad(&mut g, QUIET));
        // Speech during the hangover restarts it.
        assert!(vad(&mut g, LOUD));
        for _ in 0..VAD_HANGOVER_FRAMES / 2 {
            vad(&mut g, QUIET);
        }
        assert!(vad(&mut g, LOUD));
        for _ in 0..VAD_HANGOVER_FRAMES {
            assert!(vad(&mut g, QUIET));
        }
        assert!(!vad(&mut g, QUIET));
        assert!(
            !g.update(VoiceMode::VoiceActivity, 0, 0.5, true, LOUD),
            "mute wins"
        );
    }

    #[test]
    fn sensitivity_maps_to_threshold() {
        assert_eq!(vad_threshold_dbfs(0.0), -30.0);
        assert_eq!(vad_threshold_dbfs(1.0), -55.0);
        assert_eq!(vad_threshold_dbfs(0.5), -42.5);
        assert_eq!(vad_threshold_dbfs(7.0), -55.0);
        assert_eq!(vad_threshold_dbfs(f32::NAN), -42.5);
        let (mut g, _) = gate_with_key();
        assert!(!g.update(VoiceMode::VoiceActivity, 0, 0.0, false, -40.0));
        assert!(g.update(VoiceMode::VoiceActivity, 0, 1.0, false, -40.0));
    }

    #[test]
    fn mode_change_drops_the_tail() {
        let (mut g, _) = gate_with_key();
        assert!(g.update(VoiceMode::VoiceActivity, 0, 0.5, false, LOUD));
        assert!(!g.update(VoiceMode::PushToTalk, 0x56, 0.5, false, QUIET));
    }
}
