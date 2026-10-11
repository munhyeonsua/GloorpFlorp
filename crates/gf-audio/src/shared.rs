//! State shared between the engine's threads: live settings, meters, errors.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};

use crate::level::AtomicF32;
use crate::mixer::{sanitize_volume, MixParams};
use crate::playout::Incoming;
use crate::{AudioSettings, VoiceMode};

/// Locks a mutex, carrying on if another thread panicked while holding it.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Which sonora stages are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Processing {
    pub echo_cancel: bool,
    pub noise_suppress: bool,
    pub auto_gain: bool,
}

/// The settings the audio threads read every frame, as atomics so that
/// `set_settings` takes effect immediately without locks.
pub(crate) struct Live {
    voice_mode: AtomicU8,
    ptt_key: AtomicU32,
    vad_sensitivity: AtomicF32,
    mic_muted: AtomicBool,
    deafened: AtomicBool,
    echo_cancel: AtomicBool,
    noise_suppress: AtomicBool,
    auto_gain: AtomicBool,
    voice_volume: AtomicF32,
    game_volume: AtomicF32,
    voice_bitrate_kbps: AtomicU32,
    game_bitrate_kbps: AtomicU32,
}

impl Live {
    pub fn new(s: &AudioSettings) -> Live {
        let live = Live {
            voice_mode: AtomicU8::new(0),
            ptt_key: AtomicU32::new(0),
            vad_sensitivity: AtomicF32::default(),
            mic_muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            echo_cancel: AtomicBool::new(false),
            noise_suppress: AtomicBool::new(false),
            auto_gain: AtomicBool::new(false),
            voice_volume: AtomicF32::default(),
            game_volume: AtomicF32::default(),
            voice_bitrate_kbps: AtomicU32::new(0),
            game_bitrate_kbps: AtomicU32::new(0),
        };
        live.apply(s);
        live
    }

    pub fn apply(&self, s: &AudioSettings) {
        let mode = match s.voice_mode {
            VoiceMode::PushToTalk => 0,
            VoiceMode::VoiceActivity => 1,
            VoiceMode::AlwaysOn => 2,
        };
        self.voice_mode.store(mode, Relaxed);
        self.ptt_key.store(s.ptt_key, Relaxed);
        self.vad_sensitivity.store(s.vad_sensitivity);
        self.mic_muted.store(s.mic_muted, Relaxed);
        self.deafened.store(s.deafened, Relaxed);
        self.echo_cancel.store(s.echo_cancel, Relaxed);
        self.noise_suppress.store(s.noise_suppress, Relaxed);
        self.auto_gain.store(s.auto_gain, Relaxed);
        self.voice_volume.store(sanitize_volume(s.voice_volume));
        self.game_volume.store(sanitize_volume(s.game_volume));
        self.voice_bitrate_kbps.store(s.voice_bitrate_kbps, Relaxed);
        self.game_bitrate_kbps.store(s.game_bitrate_kbps, Relaxed);
    }

    pub fn voice_mode(&self) -> VoiceMode {
        match self.voice_mode.load(Relaxed) {
            0 => VoiceMode::PushToTalk,
            1 => VoiceMode::VoiceActivity,
            _ => VoiceMode::AlwaysOn,
        }
    }

    pub fn ptt_key(&self) -> u32 {
        self.ptt_key.load(Relaxed)
    }

    pub fn vad_sensitivity(&self) -> f32 {
        self.vad_sensitivity.load()
    }

    pub fn mic_muted(&self) -> bool {
        self.mic_muted.load(Relaxed)
    }

    pub fn processing(&self) -> Processing {
        Processing {
            echo_cancel: self.echo_cancel.load(Relaxed),
            noise_suppress: self.noise_suppress.load(Relaxed),
            auto_gain: self.auto_gain.load(Relaxed),
        }
    }

    pub fn mix(&self) -> MixParams {
        MixParams {
            voice_volume: self.voice_volume.load(),
            game_volume: self.game_volume.load(),
            deafened: self.deafened.load(Relaxed),
        }
    }

    pub fn voice_bitrate_kbps(&self) -> u32 {
        self.voice_bitrate_kbps.load(Relaxed)
    }

    pub fn game_bitrate_kbps(&self) -> u32 {
        self.game_bitrate_kbps.load(Relaxed)
    }
}

/// Meter values for [`crate::AudioStatus`], written by the audio threads.
#[derive(Default)]
pub(crate) struct Meters {
    pub mic_level: AtomicF32,
    pub transmitting: AtomicBool,
    pub peer_voice_level: AtomicF32,
    pub peer_talking: AtomicBool,
    pub game_level: AtomicF32,
}

pub(crate) struct Shared {
    pub live: Live,
    pub meters: Meters,
    pub incoming: Incoming,
    /// Set when the engine is dropped; every thread polls it.
    pub stop: AtomicBool,
    /// Plain-language problems with the microphone / speakers.
    pub input_error: Mutex<Option<String>>,
    pub output_error: Mutex<Option<String>>,
    /// Set if one of our threads crashed.
    pub internal_error: Mutex<Option<String>>,
    /// Game-audio packet counters, kept across capture restarts so the
    /// friend's jitter buffer sees one continuous stream.
    pub game_seq: AtomicU32,
    pub game_ts: AtomicU32,
}

impl Shared {
    pub fn new(settings: &AudioSettings) -> Shared {
        Shared {
            live: Live::new(settings),
            meters: Meters::default(),
            incoming: Incoming::new(),
            stop: AtomicBool::new(false),
            input_error: Mutex::new(None),
            output_error: Mutex::new(None),
            internal_error: Mutex::new(None),
            game_seq: AtomicU32::new(0),
            game_ts: AtomicU32::new(0),
        }
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_settings_round_trip() {
        let mut s = AudioSettings {
            voice_mode: VoiceMode::PushToTalk,
            ptt_key: 0x56,
            vad_sensitivity: 0.25,
            mic_muted: true,
            deafened: true,
            echo_cancel: false,
            voice_volume: 1.5,
            game_volume: f32::NAN,
            voice_bitrate_kbps: 32,
            ..Default::default()
        };
        let live = Live::new(&s);
        assert_eq!(live.voice_mode(), VoiceMode::PushToTalk);
        assert_eq!(live.ptt_key(), 0x56);
        assert_eq!(live.vad_sensitivity(), 0.25);
        assert!(live.mic_muted());
        assert_eq!(
            live.processing(),
            Processing {
                echo_cancel: false,
                noise_suppress: true,
                auto_gain: true
            }
        );
        assert_eq!(
            live.mix(),
            MixParams {
                voice_volume: 1.5,
                game_volume: 1.0,
                deafened: true
            }
        );
        assert_eq!(live.voice_bitrate_kbps(), 32);
        assert_eq!(live.game_bitrate_kbps(), 160);
        for mode in [VoiceMode::AlwaysOn, VoiceMode::VoiceActivity] {
            s.voice_mode = mode;
            live.apply(&s);
            assert_eq!(live.voice_mode(), mode);
        }
    }
}
