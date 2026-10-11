export const meta = {
  name: 'finish-v1',
  description: 'Finish GloorpFlorp v1: gf-audio and the app in parallel, then integrate and verify the workspace',
  whenToUse: 'Run once after gf-proto, gf-sys, gf-net and gf-video are implemented (see docs/HANDOFF.md).',
  phases: [
    { title: 'Build', detail: 'gf-audio and app (parallel)' },
    { title: 'Integrate', detail: 'workspace build/test/clippy, end-to-end test, release build' },
  ],
}

// Resumes the original build after the session moved from a cloud machine to
// the owner's PC. Agent prompts are the ones used for the earlier crates, with
// the environment section updated.

const RESULT = {
  type: 'object',
  properties: {
    summary: { type: 'string' },
    files: { type: 'array', items: { type: 'string' } },
    api_changes: { type: 'array', items: { type: 'string' } },
    requests: { type: 'array', items: { type: 'string' } },
    tests: { type: 'string' },
    unverified: { type: 'array', items: { type: 'string' } },
    concerns: { type: 'array', items: { type: 'string' } },
  },
  required: ['summary', 'files', 'api_changes', 'requests', 'tests', 'unverified', 'concerns'],
}

const COMMON = `You are implementing part of GloorpFlorp: a private two-person screen sharing + voice chat app for two Windows PCs with NVIDIA GPUs (an RTX 5080 and an RTX 3070 Ti) connected over Tailscale. It is a Rust cargo workspace; the current directory is the repository root. The owner is NOT a programmer and plays games like Escape from Tarkov with a friend.

FIRST read docs/HANDOFF.md (project status and what the other agents already built and reported), then docs/ARCHITECTURE.md, then the lib.rs (and other src files) of your crate and of every crate you depend on. The doc comments are the contract: implement exactly that behaviour.

Environment: most likely the owner's own Windows 11 PC (Ryzen 7 9800X3D, RTX 5080, real audio devices and display) running Claude Code locally — check with 'rustc -vV' and 'ver'/'uname'. On native Windows (MSVC toolchain) you can compile and run Windows-only code for real, but this is the owner's personal computer: do not change system settings, do not install anything system-wide without it being strictly needed, never play loud sounds, and keep any windows you open short-lived. If you are on Linux instead, the Windows code can only be checked with 'cargo check --target x86_64-pc-windows-gnu'. Dependency sources are in the cargo registry (~/.cargo/registry/src/*/ or %USERPROFILE%\.cargo\registry\src\*\) — read them to get exact APIs (windows 0.62, wasapi 0.25, cpal 0.18, sonora 0.2, opus 0.4, eframe/egui 0.36, rubato 5, rtrb 0.3).

Another agent may be editing ANOTHER crate of this workspace at the same time and shares the target dir. Cargo may print "Blocking waiting for file lock" — just wait. Scope cargo commands with -p <your package>. Never run cargo clean or a global cargo update.

Rules:
- Only edit files inside your assigned directory. If you need a change in another crate (a bug or a missing API), do NOT edit it: work around it and describe it in "requests".
- You may extend your crate's public API additively. Do not rename/remove/change the signatures the skeleton defined unless truly unavoidable; report every public API change in "api_changes".
- Add dependencies to your own crate's Cargo.toml with explicit versions (don't edit the root Cargo.toml). Prefer pure-Rust crates.
- Do NOT run git add/commit/stash/checkout/reset. The coordinator commits.
- Leave no todo!()/unimplemented!() in your crate. Never panic on bad input from the network, files or child processes: return errors. Never block or allocate inside real-time audio callbacks.
- Errors that can reach the UI must be plain language and tell the user what to do.
- Style: idiomatic Rust, concise comments only where non-obvious, match the doc-comment density of the existing code. Use the log crate (info for lifecycle, warn for recoverable problems, debug for detail; never per-frame logging above trace).
- Tests must pass on Linux AND on a GitHub Actions windows-latest runner (no GPU, no audio devices, no display, no mpv, no ffmpeg on PATH unless the test provides it). Tests that need ffmpeg/mpv/NVENC/audio devices must detect absence and return early with a printed note instead of failing.
- Before finishing, these must pass for your package P: cargo build -p P ; cargo test -p P ; cargo clippy -p P --all-targets -- -D warnings. List in "unverified" what you could not confirm on real hardware.

Return: summary (what you built, key design choices), files touched, api_changes, requests (for other crates), tests (what you ran + results), unverified, concerns (anything risky or that you could not get working — be honest; never claim something works that you did not verify).`

const AUDIO = `${COMMON}

YOUR CRATE: crates/gf-audio (package gf-audio). Implement everything in lib.rs as documented. This is the most delicate crate: real-time safety and correctness matter. Split it into modules (devices, mic pipeline, playout/mixer, game encoder, opus wrappers, resampling).

Architecture to implement:
- OUTPUT: one cpal output stream (chosen device or default; prefer f32, device default rate/channels; handle mono/stereo/multichannel). The callback only pops interleaved device-rate samples from an rtrb ring (zeros on underrun, count underruns). A playout thread keeps that ring filled to a target of ~30 ms: each step it takes one 10 ms frame per stream from the stream's JitterBuffer (gf_proto::jitter, behind a Mutex shared with receive()), decodes it (Pop::Frame => decode; Pop::Lost{next:Some} => FEC decode from next; Pop::Lost{None} => PLC (empty input); Pop::Empty => silence without touching the decoder), mixes Voice (mono to both channels) + Game (stereo) with volumes (and deafen), sends a mono downmix of the mix as the echo-canceller reference to the mic thread, resamples 48 kHz -> device rate (rubato) and pushes into the ring. Consumption is paced by the device clock, so drift is absorbed by the jitter buffer.
- INPUT/VOICE: cpal input stream (chosen/default) whose callback only converts to mono f32 and pushes into an rtrb ring. Mic thread: resample to 48 kHz, cut 480-sample frames; feed all pending reference frames to sonora's render side, then process the capture frame (echo canceller / noise suppression / gain controller 2 / high-pass per settings — read the sonora 0.2 source for the exact Config and method signatures); compute level; gate (PushToTalk via gf_sys::keys::is_down(ptt_key) with ~150 ms release tail; VoiceActivity via sonora's VAD output if the API exposes one, else an energy gate on the processed signal mapping vad_sensitivity 0..1 to about -30..-55 dBFS with ~400 ms hangover; AlwaysOn); mute overrides; encode with Opus mono (Application::Voip, bitrate setting, in-band FEC on, expected loss ~5%, 10 ms frames) and call sink(Voice, seq, ts, packet). seq increments per sent packet, ts by 480 per frame.
- GAME: start_game_audio starts gf_sys::LoopbackCapture whose callback pushes into a ring/channel; a game encoder thread cuts 480-frame stereo chunks, encodes Opus stereo (Application::Audio, game bitrate, FEC on, 10 ms) and calls sink(Game, ...). Track level.
- receive(): push into the right JitterBuffer (non-blocking apart from a short mutex).
- set_settings: volumes/mute/deafen/mode/ptt key take effect immediately (atomics or a settings mutex read per frame); device changes rebuild only the affected cpal stream; processing toggles rebuild the sonora instance.
- No devices (this container, CI runners): start() must still succeed; record a plain-language error in status ("No microphone found — your friend can't hear you, but you can still hear them." / "No speakers or headphones found.") and keep everything else working.
- Test mode: with env GLOORP_FAKE_AUDIO=1 the mic is replaced by a synthetic 300 Hz tone generator at real-time pace and the output by a null sink that consumes at real-time pace (still updating levels), so the app's end-to-end test can verify voice packets flow without devices. Document this.
- Status: levels via atomics, jitter stats, error string, transmitting/peer_talking flags.
- cpal 0.18: check whether device names come from name() or a newer description()/id() API and use the non-deprecated one; list functions return names usable for selection.
- Make the core logic testable without devices (e.g. a PlayoutCore with receive/render, a VoiceGate, a Mixer, Opus wrappers). Tests: Opus round trip; FEC/PLC paths; gate behaviour for each mode (PTT via an injectable key function); mixer volumes/deafen; resampler chunking 44.1k/48k/96k; an in-process loop: encode a 440 Hz tone, push packets with shuffling/loss into PlayoutCore, render, check the output tone frequency via zero crossings; fake-audio AudioEngine sends Voice packets to a test sink at ~100/s.
- sonora/opus/rubato in debug builds are slow; the workspace already sets opt-level 2 for dependencies in dev.`


const APP = `${COMMON}

YOUR CRATE: app/ (package gloorpflorp, binary GloorpFlorp). Implement the engine, settings, logging and the egui UI. gf-net and gf-video are finished; gf-audio is being implemented concurrently by another agent — code against its documented API; do not wait for it. You may restructure app/ freely (recommended: a library target src/lib.rs holding engine + settings so integration tests can use them, and main.rs + ui.rs for the binary).

ENGINE (own thread; UI never blocks):
- Command enum, e.g.: Host, Join(code), JoinAddr{addr, secret} (for tests), Leave, StartSharing, StopSharing, OpenViewer, StartGameAudio, StopGameAudio, ApplySettings(Settings), RefreshLists, Shutdown.
- Snapshot (Arc<Mutex<_>> read by the UI each frame): phase (Idle / Hosting{code, waiting} / Joining / Connected{peer} / Reconnecting), room code, listen addr, tailscale ip, my/peer encoders, my share (codec, size, fps) and frames sent/fps, peer share and frames received/fps, viewer open, peer watching, game audio on (mine/peer's), NetStats, AudioStatus, monitors, input/output devices, audio apps, last error (plain language, dismissable) and info message.
- EngineOptions for tests: bind ip override (allow 127.0.0.1 and port 0), so tests can bypass Tailscale.
- Startup: locate tools (gf_video::Tools::locate — if missing, set a clear error but keep voice working), probe encoders on a background thread, list monitors/devices/apps, start gf_audio::AudioEngine with a sink that forwards packets to the current NetTx (if connected).
- Host: gf_sys::tailscale_ipv4() (None => "Tailscale isn't running or isn't signed in on this PC. Open Tailscale, sign in, then click Host again."), RoomCode::new_random, gf_net::start(Role::Host{bind_ip}). Join: RoomCode::parse (show its plain error), Role::Join.
- Follow the session lifecycle in docs/ARCHITECTURE.md exactly: Watching on/off, skip frames until a keyframe after the peer starts watching, auto-open viewer (setting), Open viewer button, closing the viewer window sends Watching{on:false}, codec choice (Auto = best of my encoders; explicit choice must be in my encoders else error), bitrate default per codec (AV1 20, HEVC 25, H.264 35 Mbps) unless overridden, keyframe interval = fps * keyframe_secs, ShareStarted/ShareStopped, GameAudio{active}, disconnect cleanup (stop encoder, game audio and viewer; AudioEngine::reset_peer), Leave, Shutdown on window close (fast; never hang the UI).
- Game audio source from settings: Off / AllExceptThisApp / App{exe} (resolve exe -> pid via gf_sys::list_audio_apps at start time; plain error if not running).
SETTINGS: serde JSON at directories::ProjectDirs config dir (Windows: %APPDATA%\\GloorpFlorp\\settings.json): name (default gf_sys::computer_name()), port (47800), monitor, max_height (Some(1440)), fps (60), codec choice, bitrate override, keyframe_secs (2), game audio mode, auto_open_viewer (true), last joined code, and all gf_audio::AudioSettings fields (mirror struct with serde; AudioSettings itself has no serde). #[serde(default)] everywhere; corrupt file => defaults + warning. Save (debounced) on change and on exit.
LOGGING: simplelog to <data dir>/log.txt (rotate previous to log.old.txt) + terminal in debug builds; panic hook that logs the panic.
UI (eframe/egui; one window ~460x760, dark theme, scrollable, plain language, hover tooltips explaining every option):
 - Status line (coloured): Not connected / Waiting for your friend… / Connected to NAME (ping N ms) / Reconnecting….
 - Room: [Host a room] → big monospace code + [Copy] + "Send this code to your friend"; Join: code text field + [Join]; [Leave].
 - Your screen: monitor combo, resolution cap (Native/1440p/1080p/720p), FPS (30/60/120/144), codec (Auto (best available)/AV1/HEVC/H.264 — unavailable ones disabled with tooltip), bitrate slider 5–80 Mbps with Auto checkbox, [Start sharing]/[Stop sharing], status (e.g. "Sharing AV1 2560×1440 @ 60 fps, friend is watching").
 - Game audio: Off / Everything except GloorpFlorp / Only one program [combo + refresh]; [Start]/[Stop] (or tie it to sharing with a checkbox "Send game audio while sharing" — pick whichever is simplest to understand and document it).
 - Friend's screen: "NAME is sharing (codec, size, fps)" + [Open viewer] when closed; auto-open checkbox; game audio volume slider 0–200%.
 - Voice: mic combo, speaker combo (System default + devices, refresh), mode (Push-to-talk [Set key: shows key name; while binding poll gf_sys::keys::first_pressed() every frame, Esc cancels] / Voice activation [sensitivity slider] / Always on), Mute mic, Deafen, friend voice volume 0–200%, mic level meter + "Talking" indicator, friend talking indicator, echo cancellation / noise suppression / auto gain checkboxes, your display name field.
 - Collapsible "Connection details": ping, video/audio kbps both ways, video fps in/out, queue/drops, audio loss, jitter buffer delay.
 - Error banner (red, with ✕) and info line. Request repaint about every 100 ms while connected or binding a key.
TESTS: settings round trip + corrupt file; an end-to-end test in app/tests/ that sets GLOORP_FAKE_AUDIO=1, starts two engines on 127.0.0.1 (host port 0, joiner via JoinAddr), checks: both connected; host StartSharing → joiner sees peer share and receives frames (on Linux the viewer falls back to an ffmpeg null sink; skip the video part with a note if ffmpeg is unavailable); voice packets flow both ways with AlwaysOn mode (e.g. audio recv kbps > 0 or peer_voice_level > 0); game audio start sets peer_game_audio on the other side; Leave makes the other side see the disconnect. The other crates may not be finished when you run it — if it fails only because of todo!() elsewhere, say so in your result; the integration step will run it again.`


phase('Build')
const [audio, app] = await parallel([
  () => agent(AUDIO, { label: 'gf-audio', phase: 'Build', schema: RESULT, effort: 'high' }),
  () => agent(APP, { label: 'app', phase: 'Build', schema: RESULT, effort: 'high' }),
])

phase('Integrate')
const all = { audio, app, earlierReports: 'see docs/HANDOFF.md' }
const integ = await agent(`${COMMON}

YOUR ROLE NOW: INTEGRATOR. All crates have been implemented by separate agents in parallel. You may now edit ANY file in the workspace (the per-crate restriction no longer applies, and nobody else is editing). Make the whole thing work together.

1. Resolve every item in the agents' "requests" and "api_changes" lists below and in docs/HANDOFF.md (cross-crate mismatches, missing APIs).
2. grep for todo!/unimplemented! — none may remain.
3. Make these pass: cargo build --workspace; cargo test --workspace (including the app end-to-end test with GLOORP_FAKE_AUDIO=1 — fix real bugs, don't weaken assertions to make them pass); cargo clippy --workspace --all-targets -- -D warnings; cargo build --release -p gloorpflorp (must produce GloorpFlorp.exe; on Linux use --target x86_64-pc-windows-gnu and also run clippy for that target).
4. Read the engine wiring end to end against docs/ARCHITECTURE.md's session lifecycle and fix logic bugs you find (e.g. frames sent before Watching, viewer not reopened, state not reset on disconnect, UI showing stale state).
5. Check .github/workflows/build.yml matches the real package/binary names and test behaviour (tests must pass on windows-latest without GPU/audio/display/mpv/ffmpeg), and that README.md's descriptions of buttons/labels match the real UI text.
6. Update docs/ARCHITECTURE.md if the implementation legitimately differs from it.

Agents' reports:
${JSON.stringify(all, null, 1)}

In your result, 'tests' must list the exact commands run and their outcomes.`, { label: 'integrator', phase: 'Integrate', schema: RESULT, effort: 'high' })

return { audio, app, integ }

