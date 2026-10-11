//! All audio. Everything internal runs at 48 kHz in 10 ms frames
//! ([`gf_proto::AUDIO_FRAME_SAMPLES`] = 480 samples per channel).
//!
//! ```text
//! OUTGOING
//!  mic (cpal, any format) -> mono 48k -> sonora (echo cancel, noise
//!    suppression, auto gain, high-pass) -> gate (push-to-talk / voice
//!    activity / always on, mute) -> Opus mono VOIP, in-band FEC -> sink(Voice)
//!  game (gf_sys::LoopbackCapture, stereo 48k) -> Opus stereo AUDIO,
//!    in-band FEC -> sink(Game)
//!
//! INCOMING (per stream: Voice, Game)
//!  receive() -> JitterBuffer -> Opus decode (FEC / PLC on loss) -> ring
//!    buffer -> output callback mixes Voice (mono to both ears) + Game
//!    (stereo) with their volumes -> speakers (cpal, resampled to device rate)
//!  The mixed output is also fed to the echo canceller as its reference.
//! ```
//!
//! Real-time rules: the cpal callbacks never lock, allocate or block. They
//! only move samples through `rtrb` ring buffers; Opus, sonora and resampling
//! run on ordinary threads.
//!
//! Devices: `None` means the system default. If the chosen device disappears
//! or fails, report it in [`AudioStatus::error`] and keep the rest working
//! (e.g. no microphone must not stop you hearing your friend).

use std::sync::Arc;

use gf_proto::jitter::JitterStats;
use gf_proto::AudioStream;
use gf_sys::GameAudioSource;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceMode {
    /// Send only while the push-to-talk key is held (works in-game).
    PushToTalk,
    /// Send when speech is detected.
    VoiceActivity,
    AlwaysOn,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSettings {
    /// Device names from [`list_input_devices`]; `None` = system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub voice_mode: VoiceMode,
    /// Windows virtual-key code (see `gf_sys::keys`).
    pub ptt_key: u32,
    /// Voice activity sensitivity, 0.0 (needs loud speech) to 1.0 (very sensitive).
    pub vad_sensitivity: f32,
    pub mic_muted: bool,
    /// Mute everything we play (friend's voice and game audio).
    pub deafened: bool,
    pub echo_cancel: bool,
    pub noise_suppress: bool,
    pub auto_gain: bool,
    /// 0.0..=2.0, 1.0 = unchanged.
    pub voice_volume: f32,
    pub game_volume: f32,
    pub voice_bitrate_kbps: u32,
    pub game_bitrate_kbps: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        AudioSettings {
            input_device: None,
            output_device: None,
            voice_mode: VoiceMode::VoiceActivity,
            ptt_key: 0x06, // Mouse 5
            vad_sensitivity: 0.5,
            mic_muted: false,
            deafened: false,
            echo_cancel: true,
            noise_suppress: true,
            auto_gain: true,
            voice_volume: 1.0,
            game_volume: 1.0,
            voice_bitrate_kbps: 48,
            game_bitrate_kbps: 160,
        }
    }
}

/// Where encoded packets go: `(stream, seq, sample_timestamp, opus_packet)`.
pub type PacketSink = Arc<dyn Fn(AudioStream, u16, u32, &[u8]) + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioStatus {
    /// Mic level after processing, 0..1 (for a meter).
    pub mic_level: f32,
    /// True while voice is actually being sent.
    pub transmitting: bool,
    /// Friend's voice level, 0..1.
    pub peer_voice_level: f32,
    pub peer_talking: bool,
    /// Our outgoing game audio level, 0..1.
    pub game_level: f32,
    pub game_capturing: bool,
    pub voice_jitter: JitterStats,
    pub game_jitter: JitterStats,
    /// Plain-language problem to show the user (missing device, etc.).
    pub error: Option<String>,
}

pub fn list_input_devices() -> Vec<String> {
    todo!()
}

pub fn list_output_devices() -> Vec<String> {
    todo!()
}

/// Running audio. Stops everything when dropped.
pub struct AudioEngine {}

impl AudioEngine {
    pub fn start(settings: AudioSettings, sink: PacketSink) -> anyhow::Result<AudioEngine> {
        todo!()
    }

    /// Apply new settings live. Device changes restart only the affected stream.
    pub fn set_settings(&self, settings: AudioSettings) {
        todo!()
    }

    /// Packet from the network.
    pub fn receive(&self, stream: AudioStream, seq: u16, ts: u32, payload: Vec<u8>) {
        todo!()
    }

    /// Clear all incoming state (call when the friend disconnects).
    pub fn reset_peer(&self) {
        todo!()
    }

    pub fn start_game_audio(&self, source: GameAudioSource) -> anyhow::Result<()> {
        todo!()
    }

    pub fn stop_game_audio(&self) {
        todo!()
    }

    pub fn status(&self) -> AudioStatus {
        todo!()
    }
}
