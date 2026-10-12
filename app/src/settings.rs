//! User settings, saved as JSON in the user's config folder
//! (%APPDATA%\GloorpFlorp\settings.json on Windows).
//!
//! Every field has a default (`#[serde(default)]`), so a file from an older
//! or newer version loads with whatever it has. A file that can't be parsed
//! is ignored with a warning and the defaults are used.

use std::path::{Path, PathBuf};

use gf_audio::{AudioSettings, VoiceMode};
use gf_proto::VideoCodec;
use serde::{Deserialize, Serialize};

/// Allowed screen-share bitrates in Mbps.
pub const MIN_BITRATE_MBPS: u32 = 5;
pub const MAX_BITRATE_MBPS: u32 = 80;
/// Frame rates offered in the UI.
pub const FPS_CHOICES: [u32; 4] = [30, 60, 120, 144];
/// Resolution caps offered in the UI (`None` = native).
pub const HEIGHT_CHOICES: [Option<u32>; 4] = [None, Some(1440), Some(1080), Some(720)];

/// Which codec to share with.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodecChoice {
    /// The best one this PC can encode (AV1, then HEVC, then H.264).
    #[default]
    Auto,
    Av1,
    Hevc,
    H264,
}

impl CodecChoice {
    pub const ALL: [CodecChoice; 4] = [
        CodecChoice::Auto,
        CodecChoice::Av1,
        CodecChoice::Hevc,
        CodecChoice::H264,
    ];

    /// `None` for Auto.
    pub fn codec(self) -> Option<VideoCodec> {
        match self {
            CodecChoice::Auto => None,
            CodecChoice::Av1 => Some(VideoCodec::Av1),
            CodecChoice::Hevc => Some(VideoCodec::Hevc),
            CodecChoice::H264 => Some(VideoCodec::H264),
        }
    }

    pub fn label(self) -> &'static str {
        match self.codec() {
            None => "Auto (best available)",
            Some(c) => c.label(),
        }
    }
}

/// Default bitrate for 1440p60 with each codec.
pub fn default_bitrate_mbps(codec: VideoCodec) -> u32 {
    match codec {
        VideoCodec::Av1 => 20,
        VideoCodec::Hevc => 25,
        VideoCodec::H264 => 35,
    }
}

/// What to send as game audio. Whether it is being sent is not a setting:
/// game audio always starts off and is switched on with "Start".
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub enum GameAudioMode {
    /// Everything the PC plays except GloorpFlorp itself.
    #[default]
    AllExceptThisApp,
    /// Only this program (exe name, e.g. "EscapeFromTarkov.exe"). It is
    /// looked up among the running programs when game audio starts.
    App { exe: String },
}

/// Serializable mirror of [`gf_audio::VoiceMode`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceModeChoice {
    PushToTalk,
    VoiceActivity,
    AlwaysOn,
}

impl From<VoiceMode> for VoiceModeChoice {
    fn from(m: VoiceMode) -> Self {
        match m {
            VoiceMode::PushToTalk => VoiceModeChoice::PushToTalk,
            VoiceMode::VoiceActivity => VoiceModeChoice::VoiceActivity,
            VoiceMode::AlwaysOn => VoiceModeChoice::AlwaysOn,
        }
    }
}

impl From<VoiceModeChoice> for VoiceMode {
    fn from(m: VoiceModeChoice) -> Self {
        match m {
            VoiceModeChoice::PushToTalk => VoiceMode::PushToTalk,
            VoiceModeChoice::VoiceActivity => VoiceMode::VoiceActivity,
            VoiceModeChoice::AlwaysOn => VoiceMode::AlwaysOn,
        }
    }
}

/// Serializable mirror of [`gf_audio::AudioSettings`] (field for field).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AudioPrefs {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub voice_mode: VoiceModeChoice,
    pub ptt_key: u32,
    pub vad_sensitivity: f32,
    pub mic_muted: bool,
    pub deafened: bool,
    pub echo_cancel: bool,
    pub noise_suppress: bool,
    pub auto_gain: bool,
    pub voice_volume: f32,
    pub game_volume: f32,
    pub voice_bitrate_kbps: u32,
    pub game_bitrate_kbps: u32,
}

impl Default for AudioPrefs {
    fn default() -> Self {
        AudioPrefs::from(&AudioSettings::default())
    }
}

impl From<&AudioSettings> for AudioPrefs {
    fn from(a: &AudioSettings) -> Self {
        AudioPrefs {
            input_device: a.input_device.clone(),
            output_device: a.output_device.clone(),
            voice_mode: a.voice_mode.into(),
            ptt_key: a.ptt_key,
            vad_sensitivity: a.vad_sensitivity,
            mic_muted: a.mic_muted,
            deafened: a.deafened,
            echo_cancel: a.echo_cancel,
            noise_suppress: a.noise_suppress,
            auto_gain: a.auto_gain,
            voice_volume: a.voice_volume,
            game_volume: a.game_volume,
            voice_bitrate_kbps: a.voice_bitrate_kbps,
            game_bitrate_kbps: a.game_bitrate_kbps,
        }
    }
}

impl From<&AudioPrefs> for AudioSettings {
    fn from(a: &AudioPrefs) -> Self {
        AudioSettings {
            input_device: a.input_device.clone(),
            output_device: a.output_device.clone(),
            voice_mode: a.voice_mode.into(),
            ptt_key: a.ptt_key,
            vad_sensitivity: a.vad_sensitivity,
            mic_muted: a.mic_muted,
            deafened: a.deafened,
            echo_cancel: a.echo_cancel,
            noise_suppress: a.noise_suppress,
            auto_gain: a.auto_gain,
            voice_volume: a.voice_volume,
            game_volume: a.game_volume,
            voice_bitrate_kbps: a.voice_bitrate_kbps,
            game_bitrate_kbps: a.game_bitrate_kbps,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Shown to the friend.
    pub name: String,
    /// TCP and UDP port the host listens on (and the joiner connects to).
    pub port: u16,
    /// `gf_sys::Monitor::index` of the monitor to share.
    pub monitor: u32,
    /// Share only this program's window instead of the monitor (exe name,
    /// looked up among the running programs when sharing starts).
    pub share_app: Option<String>,
    /// Scale the shared screen down to this height (`None` = native).
    pub max_height: Option<u32>,
    pub fps: u32,
    pub codec: CodecChoice,
    /// `None` = the codec's default (AV1 20, HEVC 25, H.264 35 Mbps).
    pub bitrate_mbps: Option<u32>,
    /// Seconds between keyframes (how long a newly opened viewer may wait).
    pub keyframe_secs: f32,
    pub game_audio: GameAudioMode,
    /// Open the viewer window by itself when the friend starts sharing.
    pub auto_open_viewer: bool,
    /// The room code last typed into Join.
    pub last_code: String,
    pub audio: AudioPrefs,
    /// Key (Windows virtual-key code) that turns Mute on and off from
    /// anywhere; `None` = not set.
    pub mute_key: Option<u32>,
    /// Same for Deafen.
    pub deafen_key: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: default_name(),
            port: gf_proto::DEFAULT_PORT,
            monitor: 0,
            share_app: None,
            max_height: Some(1440),
            fps: 60,
            codec: CodecChoice::Auto,
            bitrate_mbps: None,
            keyframe_secs: 2.0,
            game_audio: GameAudioMode::default(),
            auto_open_viewer: true,
            last_code: String::new(),
            audio: AudioPrefs::default(),
            mute_key: None,
            deafen_key: None,
        }
    }
}

/// This PC's name, used when "Your name" is left empty.
pub fn default_name() -> String {
    clean_name(&gf_sys::computer_name())
}

/// Names are shown in a window title and the friend's UI: one line, short.
pub fn clean_name(name: &str) -> String {
    let n: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(40)
        .collect();
    n.trim().to_string()
}

fn finite_or(v: f32, default: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        default
    }
}

impl Settings {
    /// Brings every value back into its allowed range (hand-edited or
    /// damaged files).
    pub fn sanitize(&mut self) {
        self.name = clean_name(&self.name);
        if self.name.is_empty() {
            self.name = default_name();
        }
        if self.port == 0 {
            self.port = gf_proto::DEFAULT_PORT;
        }
        if self.share_app.as_ref().is_some_and(|e| e.trim().is_empty()) {
            self.share_app = None;
        }
        if !FPS_CHOICES.contains(&self.fps) {
            self.fps = 60;
        }
        if let Some(h) = self.max_height {
            if !(240..=4320).contains(&h) {
                self.max_height = Some(1440);
            }
        }
        if let Some(b) = self.bitrate_mbps {
            self.bitrate_mbps = Some(b.clamp(MIN_BITRATE_MBPS, MAX_BITRATE_MBPS));
        }
        self.keyframe_secs = finite_or(self.keyframe_secs, 2.0).clamp(0.5, 10.0);
        let a = &mut self.audio;
        let da = AudioPrefs::default();
        a.vad_sensitivity = finite_or(a.vad_sensitivity, da.vad_sensitivity).clamp(0.0, 1.0);
        a.voice_volume = finite_or(a.voice_volume, da.voice_volume).clamp(0.0, 2.0);
        a.game_volume = finite_or(a.game_volume, da.game_volume).clamp(0.0, 2.0);
        a.voice_bitrate_kbps = a.voice_bitrate_kbps.clamp(16, 128);
        a.game_bitrate_kbps = a.game_bitrate_kbps.clamp(32, 256);
        if a.ptt_key == 0 || a.ptt_key > 0xFE {
            a.ptt_key = da.ptt_key;
        }
        // Each key does one thing (the key buttons refuse a taken key).
        let ptt = Some(a.ptt_key);
        self.mute_key = self
            .mute_key
            .filter(|&k| gf_sys::keys::bindable(k) && Some(k) != ptt);
        self.deafen_key = self
            .deafen_key
            .filter(|&k| gf_sys::keys::bindable(k) && Some(k) != ptt && Some(k) != self.mute_key);
    }

    /// Bitrate to share with for `codec`, in kbps.
    pub fn bitrate_kbps(&self, codec: VideoCodec) -> u32 {
        self.bitrate_mbps
            .unwrap_or_else(|| default_bitrate_mbps(codec))
            .clamp(MIN_BITRATE_MBPS, MAX_BITRATE_MBPS)
            * 1000
    }

    /// Keyframe interval in frames.
    pub fn keyframe_interval(&self) -> u32 {
        ((self.fps as f32 * self.keyframe_secs).round() as u32).max(1)
    }

    /// Loads `path`. Missing file: defaults, no warning. Unreadable or
    /// corrupt file: defaults plus a plain-language warning for the user.
    pub fn load(path: &Path) -> (Settings, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("no settings file yet at {}; using defaults", path.display());
                return (Settings::default(), None);
            }
            Err(e) => {
                log::warn!("couldn't read {}: {e}", path.display());
                return (
                    Settings::default(),
                    Some("Your saved settings couldn't be read, so the defaults are used.".into()),
                );
            }
        };
        // Notepad and PowerShell 5.1 can save with a byte-order mark.
        match serde_json::from_str::<Settings>(text.trim_start_matches('\u{feff}')) {
            Ok(mut s) => {
                s.sanitize();
                log::info!("settings loaded from {}", path.display());
                (s, None)
            }
            Err(e) => {
                log::warn!(
                    "settings file {} is damaged ({e}); using defaults",
                    path.display()
                );
                (
                    Settings::default(),
                    Some(
                        "Your saved settings were damaged, so the defaults are used. \
                         Check your choices below."
                            .into(),
                    ),
                )
            }
        }
    }

    /// Writes the file (via a temporary file, so a crash never leaves half of one).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// GloorpFlorp's folder for settings and the log: %APPDATA%\GloorpFlorp on
/// Windows, ~/.config/GloorpFlorp elsewhere.
pub fn app_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().join("GloorpFlorp"))
}

pub fn settings_path() -> Option<PathBuf> {
    app_dir().map(|d| d.join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gloorpflorp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trip() {
        let dir = scratch("settings-rt");
        let path = dir.join("settings.json");
        let mut s = Settings {
            name: "Alex".into(),
            port: 47900,
            monitor: 1,
            share_app: Some("mpv.exe".into()),
            max_height: Some(1080),
            fps: 144,
            codec: CodecChoice::Hevc,
            bitrate_mbps: Some(30),
            keyframe_secs: 1.5,
            game_audio: GameAudioMode::App {
                exe: "EscapeFromTarkov.exe".into(),
            },
            auto_open_viewer: false,
            last_code: "K7QM-2XPA-9TD".into(),
            ..Settings::default()
        };
        s.audio.input_device = Some("Headset Microphone".into());
        s.audio.voice_mode = VoiceModeChoice::PushToTalk;
        s.audio.ptt_key = 0x05;
        s.audio.game_volume = 1.5;
        s.audio.deafened = true;
        s.mute_key = Some(0x77);
        s.deafen_key = Some(0x04);
        s.save(&path).unwrap();
        let (loaded, warning) = Settings::load(&path);
        assert_eq!(warning, None);
        assert_eq!(loaded, s);
        // Saving again replaces the file.
        s.fps = 30;
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).0.fps, 30);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_or_missing_file_gives_defaults() {
        let dir = scratch("settings-bad");
        let path = dir.join("settings.json");
        let (s, warning) = Settings::load(&path);
        assert_eq!(s, Settings::default());
        assert_eq!(warning, None);

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{ \"fps\": 60, \"name\": ").unwrap();
        let (s, warning) = Settings::load(&path);
        assert_eq!(s, Settings::default());
        assert!(warning.unwrap().contains("defaults"));

        std::fs::write(&path, [0xFF, 0xFE, 0x00]).unwrap();
        let (s, warning) = Settings::load(&path);
        assert_eq!(s, Settings::default());
        assert!(warning.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_saved_with_a_byte_order_mark_loads() {
        let dir = scratch("settings-bom");
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = "\u{feff}".as_bytes().to_vec();
        bytes.extend_from_slice(br#"{ "name": "Alex", "fps": 30 }"#);
        std::fs::write(&path, bytes).unwrap();
        let (s, warning) = Settings::load(&path);
        assert_eq!(warning, None);
        assert_eq!((s.name.as_str(), s.fps), ("Alex", 30));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_and_out_of_range_values() {
        let dir = scratch("settings-partial");
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{ "fps": 59, "bitrate_mbps": 500, "keyframe_secs": -3,
                 "port": 0, "name": "  ", "unknown_field": 1, "share_app": " ",
                 "audio": { "voice_volume": 9.0, "vad_sensitivity": -1 } }"#,
        )
        .unwrap();
        let (s, warning) = Settings::load(&path);
        assert_eq!(warning, None);
        let d = Settings::default();
        assert_eq!(s.fps, 60);
        assert_eq!(s.bitrate_mbps, Some(MAX_BITRATE_MBPS));
        assert_eq!(s.keyframe_secs, 0.5);
        assert_eq!(s.port, d.port);
        assert_eq!(s.name, d.name);
        assert_eq!(s.audio.voice_volume, 2.0);
        assert_eq!(s.audio.vad_sensitivity, 0.0);
        assert_eq!(s.audio.game_volume, d.audio.game_volume);
        assert_eq!(s.max_height, Some(1440));
        // Files from before the mute/deafen shortcuts: not set.
        assert_eq!((s.mute_key, s.deafen_key), (None, None));
        assert_eq!(s.share_app, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shortcut_keys_are_valid_and_distinct() {
        let mut s = Settings {
            mute_key: Some(0x01), // left mouse button
            deafen_key: Some(0x100),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!((s.mute_key, s.deafen_key), (None, None));

        // A key push-to-talk or the mute shortcut already uses is cleared.
        s.audio.ptt_key = 0x06;
        s.mute_key = Some(0x06);
        s.deafen_key = Some(0x77);
        s.sanitize();
        assert_eq!((s.mute_key, s.deafen_key), (None, Some(0x77)));
        s.mute_key = Some(0x77);
        s.sanitize();
        assert_eq!((s.mute_key, s.deafen_key), (Some(0x77), None));
        s.deafen_key = Some(0x05);
        s.sanitize();
        assert_eq!((s.mute_key, s.deafen_key), (Some(0x77), Some(0x05)));
    }

    #[test]
    fn derived_values() {
        let mut s = Settings::default();
        assert_eq!(s.bitrate_kbps(VideoCodec::Av1), 20_000);
        assert_eq!(s.bitrate_kbps(VideoCodec::Hevc), 25_000);
        assert_eq!(s.bitrate_kbps(VideoCodec::H264), 35_000);
        s.bitrate_mbps = Some(12);
        assert_eq!(s.bitrate_kbps(VideoCodec::H264), 12_000);
        assert_eq!(s.keyframe_interval(), 120);
        s.fps = 144;
        s.keyframe_secs = 0.5;
        assert_eq!(s.keyframe_interval(), 72);
        let audio = AudioSettings::from(&AudioPrefs::default());
        assert_eq!(audio, AudioSettings::default());
        assert_eq!(clean_name("  Bob\n's PC "), "Bob 's PC");
    }
}
