# app agent report (phase 2, cloud)

Built in a Linux cloud container against the real gf-audio. Nothing has run on Windows.

## Summary
- Structure: library (`src/lib.rs`: engine, settings, logging) + binary (`main.rs`, `ui.rs`).
- Engine: own thread selecting over commands, net events, encoder events, helper-thread results and a
  100 ms tick. UI only sends `Command`s and reads `Snapshot` (Arc<Mutex<_>>); important changes publish
  immediately and wake the UI via `on_change`. Slow work (Tailscale lookup + host bind with 5 s retry
  while the old port frees, encoder probe, Encoder::start, device/monitor/app listing, dropping
  encoder/viewer) runs on short helper threads.
- Lifecycle per ARCHITECTURE.md: frames only while the friend is Watching, from the next keyframe;
  viewer auto-opens (setting), stays closed after the user closes it, Open viewer reopens, closing sends
  Watching off; new viewer on codec/size change; reconnect re-sends ShareStarted + GameAudio; disconnect
  closes the viewer, calls reset_peer, clears peer state; my encoder and game audio stop only when the
  session really ends. Codec Auto = first probed; explicit must be available. Bitrate defaults AV1 20 /
  HEVC 25 / H.264 35 Mbps. Keyframe interval = fps * keyframe_secs. Start sharing before the probe
  finishes starts once it's done. Video-setting changes while sharing restart the encoder after 1.5 s.
  Game audio stops by itself if capture reports a problem for 1 s. Missing FFmpeg = clear error, voice
  still works.
- Settings: JSON at %APPDATA%\GloorpFlorp\settings.json (BaseDirs::config_dir()/GloorpFlorp, not
  ProjectDirs, to match the README), serde(default) + sanitize(); damaged file -> defaults + warning in
  the window; saved 1 s after a change and on shutdown via temp file + rename.
- Logging: simplelog to <app dir>/log.txt (previous run -> log.old.txt); our crates Debug+, others
  Warn+; terminal too in debug builds; panic hook logs thread + backtrace.
- UI: 460x760 dark scrollable window; status line, error banner with ×, info line; sections Room (Copy,
  New code), Your screen, Game audio, Friend's screen, Voice (PTT binding polls first_pressed every
  50 ms, Esc cancels), collapsible Connection details; tooltips everywhere. Game audio: "Everything except
  GloorpFlorp" / "Only one program" + list + Refresh, plus "Start/Stop sending game audio" button.
  Checked two real binaries under Xvfb with xdotool (Host -> code -> Join -> share -> viewer -> game
  audio -> talking indicators -> details -> PTT bind/Esc -> Leave). Fixed: egui default fonts draw "●"
  and "✕" as boxes, so dots are drawn shapes and close uses "×".

## API (all new)
- engine::{Command, Phase, ShareInfo, ShareState, Snapshot, EngineOptions, Engine, ChangeCallback,
  first_paragraph}; settings::{Settings, AudioPrefs, VoiceModeChoice, CodecChoice, GameAudioMode, ...};
  logging::{init, install_panic_hook}.
- Command::ApplySettings(Box<Settings>); DismissError added.
- Phase::Hosting{code, waiting}: code empty / waiting false while the room is opening.
- EngineOptions: bind_ip, join_ip, port (0 = OS picks), settings_path (None = never save), on_change.
- GameAudioMode has no Off: game audio always starts off and is toggled with the button.
- Dev-only env vars (main.rs, logged as a warning): GLOORP_DEV_IP=127.0.0.1 and GLOORP_DEV_PORT let two
  copies talk on one PC without Tailscale.

## Requests / follow-ups for integration
- gf-audio: document AudioStatus::game_capturing as "capture running and not failed" (the app treats
  false for 1 s as "capture died"; the current implementation matches).
- start_game_audio (up to ~10 s) and stop_game_audio (~2 s) run on the engine thread: UI never blocks,
  but net events wait meanwhile. Consider moving them to a helper thread.
- README/packaging: game audio button text; device lists say "System default" (README: "Default");
  details rows are "Bitrates: video" / "Bitrates: audio"; Leave (or friend leaving for good) stops your
  sharing and game audio; optionally document GLOORP_DEV_IP / GLOORP_DEV_PORT.
- gf-net: encoder lists in Hello/Welcome are fixed at session start; Host/Join before the probe
  finishes sends an empty list (UI hides it).

## Tests (Linux container)
- cargo test -p gloorpflorp: 5 unit + 3 integration pass (~7 s). end_to_end.rs covers: connect, voice
  both ways, game audio, sharing with viewer (FFmpeg null sink) and frames, fps change while sharing,
  stop sharing/game audio, joiner Leave + rejoin + host Leave, shutdown < 6 s; plain-language join
  errors + DismissError; settings saved. Video part skips with a note when FFmpeg is missing (checked
  with PATH=/nonexistent). 6/6 repeated runs pass.
- clippy (Linux and x86_64-pc-windows-gnu, --all-targets -D warnings): clean.
- cargo build -p gloorpflorp --target x86_64-pc-windows-gnu: links GloorpFlorp.exe. fmt --check: ok.

## Unverified
Windows egui window (DPI, theme, close -> shutdown within 3 s, no console window in release), %APPDATA%
paths, Host with real Tailscale + Firewall prompt, real mpv (close detection -> Watching off, window
title), real NVENC sharing, real WASAPI game audio incl. auto-stop when the program exits, PTT binding
with GetAsyncKeyState and Mouse 4/5, real device lists and live switching, joiner reconnect path (no test).

## Concerns
- A joiner's network blip ends the host's share; the host must click Start sharing again (a grace
  period would be friendlier).
- Game audio start/stop on the engine thread can delay net events up to ~10 s in the rare stuck case.
- Encoder restarts wait for the next keyframe (up to keyframe_secs of frozen picture).
- Handshake encoder list can be empty if Host/Join happens before probing.
- Shutdown waits at most 3 s, then exits (job object kills ffmpeg/mpv on Windows).
- The FFmpeg-missing error stays until dismissed.
