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
| `gf-audio`: mic, echo cancellation, Opus, jitter, mixing, playback, game audio | ✅ written (cloud, phase 2) | 42 pass; clippy clean on Linux + windows-gnu |
| `app`: engine, settings, logging, egui UI, end-to-end test | ✅ written (cloud, phase 2) | 5 unit + 3 end-to-end pass; clippy clean on Linux + windows-gnu; UI driven under Xvfb |
| Integration (everything together, release build) | 🟡 partly: `cargo test --workspace` passes on Linux; the integration pass (cross-crate requests, workspace clippy, release exe, README wording) has not run | |

Nothing has run on real Windows/NVIDIA hardware yet.

## Next steps

1. **Integrate.** `gf-audio` and `app` are written (phase 2, built in a Linux
   cloud container). Do NOT re-run the Build phase of `finish-v1`; run only its
   integrator step (the last `agent(...)` in `.claude/workflows/finish-v1.js`),
   giving it the two phase-2 reports above. Open items it must handle are in
   the reports' "Requests" sections (game-audio start/stop off the engine
   thread, README label wording, `game_capturing` doc, etc.).
2. **Review.** Run an adversarial review across the workspace: Windows API
   correctness, real-time audio safety, protocol edge cases, UI wording.
   Verify each finding before fixing it.
3. **Test on real hardware** (this PC). Get `ffmpeg.exe` (BtbN
   `ffmpeg-master-latest-win64-gpl.zip`) and `mpv` (shinchiro
   `mpv-x86_64-*.7z`) next to the exe, or use `packaging/package-windows.ps1`.
   Then check:
   - [ ] `probe_encoders` finds AV1, HEVC and H.264 NVENC on the 5080
   - [ ] A short real share: ddagrab → av1_nvenc → viewer window shows it, delay feels low
   - [ ] Scaling path if the monitor is above 1440p (the ladder falls through as expected)
   - [ ] Monitor list matches the real monitors and ddagrab indices
   - [ ] Game-audio capture: "everything except GloorpFlorp" and "only one program" (e.g. a YouTube tab in a browser)
   - [ ] Mic → echo cancellation → voice; push-to-talk key works while a game window has focus
   - [ ] Two instances on one PC can't share port 47800, so test against the friend's PC, or add a dev-only port override
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
  `reset()` on `reset_peer`.
- `NetEvent::Audio` arrives on gf-net's UDP thread, one `Vec` per packet.
  `NetTx::send_audio` takes a short mutex and does a UDP send, so call it
  from encoder threads, never from a cpal/WASAPI callback.
- Game audio from `LoopbackCapture` arrives in Windows-sized chunks (~10 ms),
  and **stops entirely while the program is silent** (process loopback sends
  nothing then). The encoder must tolerate gaps. The capture buffer is 20 ms,
  so the callback must hand samples off immediately (no Opus encoding inside it).

## Known risks / unverified

- All Windows-only code (DXGI monitor enumeration, WASAPI process loopback,
  GetAsyncKeyState, job objects, CREATE_NO_WINDOW) is compile-checked only.
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
