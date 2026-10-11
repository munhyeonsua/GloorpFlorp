# Handoff: where GloorpFlorp v1 stands

Written when the work moved from a 4-core cloud machine to the owner's PC
(Windows 11, Ryzen 7 9800X3D, RTX 5080). Read this first, then
[ARCHITECTURE.md](ARCHITECTURE.md). Setting up the PC:
[LOCAL-SETUP.md](LOCAL-SETUP.md).

Branch: `claude/nifty-ride-afb8qt` (earlier: `claude/lucid-fermi-dew58r`). Full reports from the
agents that built the crates: `.claude/handoff/phase1-agent-reports.json` (proto, sys, net, video),
`.claude/handoff/phase2-gf-audio-report.md` and `.claude/handoff/phase2-app-report.md`. Work-in-progress commits end in
`[skip ci]` so they don't start a Windows build.

## Status

| Part | State | Tests (Linux) |
|---|---|---|
| `gf-proto`: room codes, wire format, FLV demuxer + viewer writer, jitter buffer | ✅ done | 52 unit + 6 FFmpeg stream tests pass |
| `gf-sys`: monitors, game-audio capture (process loopback), push-to-talk keys, Tailscale IP, child-process job object | ✅ done | 19 pass (Windows parts compile-checked only) |
| `gf-net`: TCP+UDP session, handshake, reconnect, video queue, stats | ✅ done | 29 pass |
| `gf-video`: FFmpeg encoder (ddagrab → NVENC → FLV), mpv viewer | ✅ done | 22 unit + 4 integration pass (software encoders) |
| `.github/workflows/build.yml`, `packaging/`, `README.md` | ✅ done | actionlint clean; packaging script run end to end with the real FFmpeg zip |
| `gf-audio`: mic, echo cancellation, Opus, jitter, mixing, playback, game audio | ✅ done | 41 pass (Linux + native Windows) |
| `app`: engine, settings, logging, egui UI, end-to-end test | ✅ done | 5 unit + 3 end-to-end pass (Linux + native Windows) |
| Integration (everything together, release build) | ✅ done on the owner's PC (Windows 11, MSVC): `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -D warnings`, `cargo build --release -p gloorpflorp` → `target\release\GloorpFlorp.exe` | all pass |

What has run on the real PC (no FFmpeg/mpv installed there yet): the
release exe opens its window, finds Tailscale, both monitors, the real
microphone and headphones, lists programs, and closes cleanly in < 1 s;
`cargo run -p gf-sys --example sys_probe` captured real process-loopback
audio ("everything except" and a single program); every test binary runs on
Windows. **Screen sharing (FFmpeg/NVENC) and the mpv viewer have not run on
Windows yet**, and the end-to-end test skipped its video part there (no
FFmpeg).

## Next steps

1. ~~Integrate.~~ Done (see the status table and "Integration" below).
2. **Review.** Run an adversarial review across the workspace: Windows API
   correctness, real-time audio safety, protocol edge cases, UI wording.
   Verify each finding before fixing it.
3. **Test on real hardware** (this PC). Get `ffmpeg.exe` (BtbN
   `ffmpeg-master-latest-win64-gpl.zip`) and `mpv` (shinchiro
   `mpv-x86_64-*.7z`) next to the exe, or use `packaging/package-windows.ps1`.
   With FFmpeg on PATH, also re-run `cargo test --workspace` so the video
   part of the end-to-end test and the gf-video/gf-proto FFmpeg tests run on
   Windows. Then check:
   - [ ] `probe_encoders` finds AV1, HEVC and H.264 NVENC on the 5080
   - [ ] A short real share: ddagrab → av1_nvenc → viewer window shows it, delay feels low
   - [ ] Scaling path: this PC's main monitor is 3840×2160 and the default cap is 1440p, so the ladder's GPU scaling is used (check which step wins in the log)
   - [ ] The portrait monitor (2160×3840, index 1) shares correctly
   - [x] Monitor list matches the real monitors (2 found: 3840×2160 primary, 2160×3840); ddagrab indices still to confirm
   - [x] Game-audio capture works ("everything except" and one program, via `sys_probe`), and through the engine with real devices (a throwaway example: capture starts, level moves, stops; closing the picked program stops game audio within ~1.5 s with the plain message). Still to try from the window itself
   - [ ] Mic → echo cancellation → voice; push-to-talk key works while a game window has focus
   - [ ] Two copies on one PC: start both with `GLOORP_DEV_IP=127.0.0.1` (use `GLOORP_FAKE_AUDIO=1` on one of them, or headphones, to avoid feedback)
   - [ ] Windows Firewall prompt appears on first Host
4. **Push without `[skip ci]`** so GitHub builds the zip. Check both jobs go green.
5. **First session with the friend.** The friend's 3070 Ti PC needs an up-to-date NVIDIA driver (FFmpeg master needs a recent NVENC API).

## Decisions made after ARCHITECTURE.md was written

- **AV1 viewer delimiter.** FFmpeg's `obu` demuxer reads 10 bytes before
  parsing an OBU header, so a bare `12 00` temporal delimiter didn't let a
  frame finish. The delimiter is now TD + an 8-byte padding OBU:
  `12 00 7A 06 00 00 00 00 00 80`. Measured first-frame latency without
  further input: ~160 ms AV1, ~70–80 ms H.264/HEVC (that includes FFmpeg
  start-up).
- **mpv options:** `--demuxer-lavf-probe-info=nostreams` is used together
  with `fflags=+nobuffer`. Without it, nobuffer discards the probed first
  keyframe.
- **NVENC VBV:** `-bufsize` = bitrate / 8. A keyframe can add up to ~125 ms
  on a link barely faster than the bitrate. Tune on the real link.
- **Room code checksum:** the top 3 bits of SplitMix64 over the 52-bit payload.

### Integration (phase 3)

- **Audio thread in the engine.** `AudioEngine::start` (up to ~8 s) and
  `start_game_audio` / `stop_game_audio` (up to ~10 s / ~2 s) run in order on
  one `audio-control` thread, so network events are never delayed. The
  engine shows game audio as "starting" (`Snapshot::game_audio_starting`)
  and tells the friend `GameAudio{active:true}` only once the capture runs.
  The UI's game-audio button is disabled for the few seconds audio takes to
  start after launch.
- **Host keeps its media when the friend drops.** The host can't tell a blip
  from leaving, so its sharing and game audio keep running while the room
  waits and are re-announced on rejoin. A joiner's own sharing/game audio
  stop only when it gives up reconnecting or the host closes the room.
  **Leave** always stops your own sharing and game audio.
- **MMCSS "Pro Audio" thread boost** moved from gf-audio into gf-sys
  (`gf_sys::raise_audio_thread_priority`), so gf-audio no longer depends on
  the `windows` crate.
- **gf-net, port 0:** the host retries when the OS-picked TCP port is already
  taken for UDP (seen as a flaky test on Windows).
- **New clippy (Rust 1.99):** `chunks_exact(N)` with a constant N is now
  `as_chunks::<N>()` (needs Rust ≥ 1.88).
- gf-audio's non-fatal device notes (e.g. "the microphone X isn't connected,
  so the default microphone is used instead") are shown in orange under
  "Voice", not in the red error banner.

### Review fixes

- **Audio thread priority:** cpal's stream threads raise their own priority
  (MMCSS) on their first callback via `gf_sys::keep_audio_thread_priority`.
- **Jitter and playout:** a dry buffer conceals for up to 50 ms instead of
  cutting to silence; a drift drop is cross-faded (`Pop::Skip`); a stream
  that ends fades out. Packets carry their real arrival time
  (`NetEvent::Audio::arrived` → `AudioEngine::receive_at`).
- **Devices:** a stream that stops working is reopened at once, and default
  streams follow Windows' default device.
- **Voice activation:** with auto gain on, the gate threshold stays 6 dB
  above the noise level the gain may lift, so a fan can't hold it open.
- **Game-audio capture:** 100 ms buffer, encoder flush after 120 ms (no
  padding when the capture thread was held up). The capture's problem is
  `AudioStatus::game_error`, shown under "Game audio" and as the banner when
  it dies.
- **Program list:** our own windows are skipped before asking for titles
  (no message to our own UI thread); Store (UWP) apps resolve to the app's
  process, not ApplicationFrameHost; the "Only one program" lookup runs on
  the audio thread, never the engine thread.
- **Push-to-talk vs. elevated games:** `gf_sys::foreground_is_elevated`,
  polled every 500 ms on the engine tick while push-to-talk is on; the hint
  under "Voice" stays 10 s after such a window loses focus.
- **Wording:** FFmpeg errors point at "Codec under 'Your screen'"; game-audio
  messages say "Start sending game audio".
- **Frozen handshake start:** `Hello` stays variant 0 with `version` first,
  `Reject` stays variant 2 with only `reason`. The host reads the version
  with `gf_proto::wire::hello_version` before decoding the rest, so any
  version gets the "different version" answer.
- **gf-net ordering:** a connection's `Disconnected` + `Listening` are
  emitted before its slot is freed, so a quick rejoin's `Connected` always
  comes after them. Sending `ShareStarted`/`ShareStopped` drops video still
  queued from the old share. Ping resets when the friend leaves.
- **Joiner retries:** the first attempt retries "room full" for
  `dead_after` + 2 s (12 s; after a crash or sleep the host still holds the
  old connection). A reconnect that drops within 5 s doesn't restart the
  30 s window. Leave during connect/handshake stops at once (with `Bye` if
  the host already welcomed us).
- **Host listener:** `accept()` failing for 5 s (Tailscale adapter
  replaced) ends the room with `NetEvent::Error`.
- **Engine:** `Watching{on:true}` asks for a keyframe only when the friend
  starts watching, and is ignored while not sharing. Overlapping host
  start-ups ("New code" while "Opening a room…") retry the port.
- **UI:** no-encoder state shown up front; release builds show a message
  box if the window can't open; minimum width 440.

## What the finished crates need from `gf-audio` and `app`

From the agents' reports (also check the code's doc comments):

**For `app` / engine**
- `gf_net::Net` implements `Drop` (dropping it closes the session). Keep it
  alive and **clone** `net.events` / `net.tx`; never destructure `Net`.
- Host bind failures come back as `Err` from `gf_net::start` (plain
  language). A joiner's failed first connect arrives as `NetEvent::Error`.
  Show both.
- `NetConfig.port = 0` lets the OS pick a port; `Listening{addr}` has the
  real one, and is emitted again after every disconnect.
- After `Disconnected{will_retry:true}`, a joiner's next `Connected` is a
  fresh connection with a new UDP token. Re-send ShareStarted / Watching /
  GameAudio state, and call `AudioEngine::reset_peer`.
- After `close()`, no more events are emitted. Background threads may linger
  for ~2–5 s, so the port frees a little later.
- Always drain `net.events` promptly (unbounded channel; video piles up otherwise).
- Video: call `probe_encoders` (up to ~8 s) and `Encoder::start`/`stop`
  (up to ~2 s) off the UI thread. Use `gf_video::output_size(monitor_size(i),
  max_h)` to know the stream size before `Started`. On
  `Stopped{error:Some(msg)}` show only the first paragraph in the UI; the
  rest ("Details from FFmpeg: …") is for the log.
- Open a new `Viewer` when the codec changes. When `Viewer::push` returns
  false (window closed), send `Watching{on:false}`.
- `gf_sys::LoopbackCapture::start` can block up to 10 s and dropping it up to
  2 s, so do both off the UI thread. Poll `LoopbackCapture::error()` to show
  "the program you picked closed".
- Test env vars: `GLOORP_FAKE_AUDIO=1` (fake mic/output/game tone),
  `GLOORP_FAKE_VIDEO=1` (test pattern + software encoders, FFmpeg null-sink
  viewer, also on Windows). The end-to-end test on the windows-latest
  runner needs `GLOORP_FAKE_VIDEO=1` and ffmpeg on PATH, or must skip video.
- UI labels the README uses (keep them, or update README.md and
  `packaging/start-here.txt`): **Host**, **Join**, **Start sharing**,
  **Stop sharing**, **Open viewer**, "Everything except GloorpFlorp",
  "Only one program", **Push-to-talk**, **Voice activation**, **Always on**,
  **Mute**, **Deafen**, connection details (Ping, Bitrates, Dropped frames,
  Audio loss). Log file at `%APPDATA%\GloorpFlorp\log.txt`. The README says
  the room code can change when the host restarts or clicks Host again.

**For `gf-audio`**
- `JitterBuffer::pop` allocates when returning `Lost{next:Some}` (a clone),
  so call it from the playout thread, not the device callback. Call
  `reset()` on `reset_peer`. When the buffer runs dry while playing, `pop`
  conceals (`Lost{next:None}`) for up to 5 frames (50 ms) before going
  `Empty`, without using up a sequence number. On a drift drop it returns
  `Pop::Skip(packet)`: decode that packet without playing it (keeps the
  decoder in step), then `pop` again for the frame's audio.
- `NetEvent::Audio` arrives on gf-net's UDP thread, one `Vec` per packet,
  stamped with its arrival time (`arrived`); the engine passes it to
  `AudioEngine::receive_at`. `NetTx::send_audio` takes a short mutex and
  does a UDP send, so call it from encoder threads, never from a cpal/WASAPI
  callback.
- Game audio from `LoopbackCapture` arrives in Windows-sized chunks (~10 ms),
  and **stops entirely while the program is silent** (process loopback sends
  nothing then). The encoder must tolerate gaps. The capture buffer is
  100 ms (the encoder's `FLUSH_AFTER` is 120 ms, so a held-up capture isn't
  padded), and the callback must hand samples off immediately (no Opus
  encoding inside it).
- `AudioStatus::error` is voice problems only; the game-audio capture's own
  problem is `AudioStatus::game_error`.

## Known risks / unverified

- Windows-only code that has run on the real PC: DXGI monitor enumeration,
  WASAPI process loopback, cpal/WASAPI device open, job object,
  CREATE_NO_WINDOW, MMCSS, the egui window. Not yet exercised for real:
  GetAsyncKeyState push-to-talk in a game, ddagrab/NVENC, mpv.
- `wasapi`'s `new_application_loopback_client` waits with no timeout. If
  Windows never activates capture, `start()` gives up after 10 s and leaves
  a stuck thread (logged).
- `list_monitors` only lists adapter 0's outputs (what ddagrab uses).
- `scale_d3d11` options in FFmpeg 8 are unconfirmed. The ladder falls back to
  CUDA, then CPU scaling.
- CI on a private repo: Windows minutes count double, and each zip is
  ~110–130 MB against a 500 MB free artifact quota. The workflow cancels
  older runs on the same branch and keeps artifacts 14 days. Consider
  building the package only on main/tags/manual runs.
- FFmpeg/mpv downloads take "latest", so builds aren't reproducible.
  VERSIONS.txt in each zip records exactly what went in.
- Licensing: the workspace says MIT, but there is no LICENSE file. The owner
  should confirm. FFmpeg and mpv are GPL; the notices cover private use
  between two people.
