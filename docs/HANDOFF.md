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

| Part | State | Tests (owner's PC, FFmpeg + mpv on PATH) |
|---|---|---|
| `gf-proto`: room codes, wire format, FLV demuxer + viewer writer, jitter buffer | ✅ done | 58 unit + 7 FFmpeg stream tests pass |
| `gf-sys`: monitors, game-audio capture (process loopback), push-to-talk keys, Tailscale IP, child-process job object | ✅ done | 23 pass |
| `gf-net`: TCP+UDP session, handshake, reconnect, video queue, stats | ✅ done | 35 pass |
| `gf-video`: FFmpeg encoder (ddagrab → NVENC → FLV), mpv viewer | ✅ done | 28 unit + 3 integration pass |
| `.github/workflows/build.yml`, `packaging/`, `README.md` | ✅ done | actionlint clean; packaging script run end to end with the real FFmpeg zip |
| `gf-audio`: mic, echo cancellation, Opus, jitter, mixing, playback, game audio | ✅ done | 48 pass |
| `app`: engine, settings, logging, egui UI, end-to-end test | ✅ done | 7 + 2 (UI) unit + 4 end-to-end pass |
| Integration (everything together, release build) | ✅ done on the owner's PC (Windows 11, MSVC): `cargo build --workspace`, `cargo test --workspace` (with and without FFmpeg/mpv), `cargo clippy --workspace --all-targets -D warnings`, `cargo fmt --all -- --check`, `cargo build --release -p gloorpflorp` → `target\release\GloorpFlorp.exe` | all pass |

Most of the app has now run on the owner's PC, including a real screen
share between two copies of the app: see "Hardware test results", "Live-desktop
checks" and "Live app checks" below. Not yet: push-to-talk and viewer focus
with a real game in front, the mouse pointer on the portrait monitor, the
Firewall prompt, and a session between the two PCs. `dist\GloorpFlorp` holds
a packaged copy (BtbN FFmpeg N-127271, shinchiro mpv 0.41) with the latest
`GloorpFlorp.exe` (CUDA scaling on).

## Next steps

1. ~~Integrate.~~ Done (see the status table and "Integration" below).
2. **Review.** Run an adversarial review across the workspace: Windows API
   correctness, real-time audio safety, protocol edge cases, UI wording.
   Verify each finding before fixing it.
3. **Test on real hardware** (this PC). Done on 2026-10-11 (overnight, then
   with the owner at the PC) except where unticked; results and fixes below.
   The unticked items need the owner (a real game, the Firewall prompt):
   - [x] `probe_encoders` finds AV1, HEVC and H.264 NVENC on the 5080 (0.82–0.84 s)
   - [x] A real share in the app (two packaged copies on 127.0.0.1): both monitors, codec changes, viewer close/reopen, 120 fps, game audio, Leave and closing (see "Live app checks")
   - [x] Scaling path: both GPU steps failed every time and the CPU step won (~2.3 s to start, ~1.9 cores). Now the CUDA step scales and turns on the GPU (checked on a live desktop: ~0.95 s to start, ~0.2 cores; see "Live-desktop checks")
   - [x] The portrait monitor (2160×3840, index 1) shares upright at 1440p and native (live capture compared with `gdigrab`). Not checked: where the mouse pointer is drawn on it (leave the pointer on that monitor and run `scratchpad/hwlive/capture/pointer.ps1`)
   - [x] Monitor list matches the real monitors, and `Monitor::index` = ddagrab `output_idx`
   - [x] Game-audio capture, both modes, from the window too
   - [x] Mic → echo cancellation → voice on the real Yeti / ECHO-A (runs cleanly; how well echo is cancelled needs a real call, as no sound was played)
   - [x] Push-to-talk with F24 bound through the UI (synthesized key): first packet 3–6 ms after key-down, 15 tail packets ending ~145 ms after key-up, nothing sent while released
   - [ ] Push-to-talk while a real game has focus, and the "runs as administrator" hint with an admin game (or Task Manager) in front
   - [x] Two copies on one PC (`GLOORP_DEV_IP=127.0.0.1`, `GLOORP_FAKE_AUDIO=1`)
   - [ ] Windows Firewall prompt appears on first Host (tests only used 127.0.0.1)
   - [x] The auto-opened viewer doesn't take focus: with another window in front (3 runs: first share, no key presses, codec-change reopen) mpv opened behind it and the foreground never changed. It does come to the front after clicking Open viewer. Not checked with a real fullscreen game
   - [x] Next-session checks prepared by the fixers (portrait direction, `CUDA_TRANSFORM = true`, pacing, H.264 bitrate): done, see "Live-desktop checks". Still open: the mouse pointer on the portrait monitor, and the owner's choice on the pacing experiment (smooth, but ~1 more CPU core). Scripts: `scratchpad/hwlive/capture/` (harness, `run.ps1`, `pointer.ps1`, `pace-experiment.diff`) and `scratchpad/hwlive/app/` (two-copy app checks). (Scratchpads are under `%TEMP%\claude\…`; they may be gone.)
4. **Push without `[skip ci]`** so GitHub builds the zip. Check both jobs go green.
5. **First session with the friend.** The friend's 3070 Ti PC needs an up-to-date NVIDIA driver (FFmpeg master needs a recent NVENC API). Judge delay and echo by ear (nothing audible was played in the tests), and keep both PCs' log files. Use 60 fps: "120/144 fps (experimental)" only reach ~72–95 fps and get less of the bitrate.

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
  above the noise level the gain may lift, so a fan can't hold it open. It
  also ignores a newly opened microphone for 1 s (`gate::MIC_SETTLE_FRAMES`):
  the Yeti makes ~0.3 s of settling noise that auto gain lifted over the
  threshold and sent as ~0.45 s of noise. This applies on start, on a device
  switch, on a Windows default-device change and on recovery; push-to-talk
  and Always on are not delayed.
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

## Hardware test results (RTX 5080 PC, 2026-10-11)

Windows 11, Ryzen 7 9800X3D, RTX 5080 (driver 617.42), monitors 3840×2160
(primary) and 2160×3840 (portrait), Yeti mic, ECHO-A headphones. Five test
runs, every defect double-checked before fixing. Key numbers:

- **Tests:** with FFmpeg and mpv on PATH every test now really runs; the
  only failure (an mpv test) was a real viewer delay, fixed below.
- **Encoders:** AV1, HEVC and H.264 NVENC found in 0.82–0.84 s. Native 4K60
  AV1 straight from the GPU: started in 0.77 s, 60.05 fps, almost no CPU.
  Every keyframe decodes on its own, one every 2 s. Stop takes 50–60 ms and
  leaves no FFmpeg behind.
- **Scaling (4K → 1440p):** both GPU steps failed every time, so the CPU step
  ran: ~2.3–2.4 s to start, ~1.9 CPU cores. (Fixed; see below.)
- **Capture pacing:** at 60 fps about every 10th frame gap is ~31 ms instead
  of ~16 ms; 120 and 144 fps only reach 74–89 fps. The limit is the capture
  (`ddagrab`), not NVENC (8–39% busy). Not fixed yet.
- **Portrait monitor:** index mapping right, but the picture came out
  sideways. (Fixed; checked live, see "Live-desktop checks".)
- **Viewer (mpv 0.41):** hardware decoding of all three codecs, `push`
  never blocks (max 0.26 ms), closing the window is noticed in ~0.18 s. Before
  the fixes: H.264/HEVC shown ~47 ms late, a new window could stay black ~2 s,
  and starting mpv froze the engine for ~0.37 s.
- **Audio:** device lists match Windows; Always on sends exactly 100 voice
  packets/s (48.8 kbps), no lost mic samples, 0 output underruns over ~4.5
  minutes and under a 16-thread CPU load; all audio threads at MMCSS priority
  24. Game audio starts in 0.4–20 ms, stops in ~6 ms, and GloorpFlorp's own
  sound is really left out of "Everything except GloorpFlorp".
- **The app window (two copies on 127.0.0.1):** Host, Join, voice, Mute,
  Deafen, game audio, leave/rejoin, error messages and closing all worked
  with plain messages; closing takes 150–260 ms and leaves no FFmpeg/mpv.

## Hardware test fixes

Crates (first round):
- **Viewer:** mpv gets `--demuxer=lavf` and `--hwdec-threads=1` (H.264/HEVC
  delay ~47 ms → 0.4 ms; a new window shows the first picture after ~0.63 s,
  mpv's start-up, instead of up to ~2 s). While mpv starts, frames from the
  latest keyframe on wait for it (up to 300 frames / 16 MB). mpv's messages
  reach the log; `$` in the friend's name no longer gets expanded.
- **Portrait monitor:** `gf_sys::Monitor::rotation`; the picture is turned
  upright, also at native size (with CUDA since the live-desktop checks; on
  the CPU in the fallback).
- **Scaling:** the impossible CUDA mapping step is gone, and once a GPU step
  failed and the CPU step then worked, later shares in the same app run go
  straight to the CPU. After the live-desktop checks the CUDA path is on
  (`args::CUDA_TRANSFORM`), so `scale_d3d11` is no longer tried.
- **H.264 filler:** NVENC padded 72–78% of the bytes; it is dropped before
  sending (a quiet 3 s 4K NVENC stream from test frames: 13.1 MB → 1.07 MB;
  live numbers below).
- **Colours:** the CPU path tags its colour matrix like NVENC does.
- **Game audio:** a program Windows won't let us watch (e.g. access denied)
  is captured anyway instead of "has closed"; new plain message if capture
  itself is refused; the "has closed" message names Start sending game audio.
- **Voice activation:** a newly opened mic is ignored for 1 s (Yeti start-up
  noise was sent in 3 of 7 opens; now 0 of 16).

App and docs (second round):
- **Viewer start off the engine thread:** mpv is started on a helper thread
  (stale starts discarded); video that arrives meanwhile is kept from the
  latest keyframe on and handed over. Measured: engine stalls at viewer open
  372–385 ms → 3–5 ms (same as normal), voice packets dropped per open 32–34
  → 0; on a codec change the new viewer still gets the opening keyframe.
- **Settings saved with a byte-order mark** (Notepad, PowerShell 5.1) load
  instead of being reset to defaults.
- **Screen readers / UI Automation:** every text box, dropdown, slider and
  number box is named after its label; the banner's × is "Hide this message";
  text boxes accept UI Automation "set value".
- **120 and 144 fps** are labelled "(experimental)", with the measured rates
  in the tooltip and README.
- **README:** log.old.txt in the privacy list, "Wrong room code", Codec under
  "Your screen", "a few seconds" of black, mpv 0.41 needed.
- **Packaging** fails if the downloaded mpv is older than 0.41.

## Live-desktop checks (owner at the PC, 2026-10-11)

Through the real `gf_video::Encoder` (ddagrab → NVENC → FLV → `FlvDemuxer`)
with the packaged FFmpeg N-127271, 6 s per run:

- **Portrait monitor:** upright in every run, 1440p and native, H.264 and
  AV1, on the CPU and the CUDA path. Compared with a `gdigrab` capture of the
  monitor: 42–48 dB as-is against 15–21 dB turned either way (before the fix:
  13 dB as-is). The size known before `Started` matched (810×1440,
  2160×3840), decoded as SAR 1:1, DAR 9:16.
- **Scaling and turning with CUDA** (now on, `args::CUDA_TRANSFORM`).
  Monitor 0, 4K → 1440p AV1, 13 alternating rounds with the old ladder:
  started in 0.93–0.98 s against 1.59–1.67 s (the failing `scale_d3d11` step
  is gone); 0.12–0.26 CPU cores against 0.44–1.58; GPU 13–16% against 10–12%;
  NVENC unchanged; ~97 MB more memory. Portrait monitor: 0.21–0.35 cores
  against 1.2–2.1 (1.83 for native H.264); it starts ~40–110 ms later than
  the CPU turn there (920–950 ms), as no step fails on that monitor. Same
  picture as the CPU way (43.4 dB monitor 0, 55.6 dB monitor 1, same frames);
  all streams decode without errors.
- **CUDA failing** (simulated with `CUDA_VISIBLE_DEVICES=-1`): the CUDA step
  exits ~0.8 s after start (`cuInit … CUDA_ERROR_NO_DEVICE`) and the CPU
  step is tried at once. That one then fails the same way, because NVENC
  needs CUDA for frames from system memory too: without CUDA only native
  upright shares work, with or without the CUDA path. (The CUDA filters
  also load their own GPU code, so they could fail where the CPU step still
  works; then the CPU step takes over.) After 1.8 s the user sees the
  "update your NVIDIA graphics driver" message (`cuInit` / `CUDA_ERROR`
  are among its patterns).
- **Capture pacing** (not fixed): at 60 fps, frames come either evenly
  (frame-gap sd ~3.2–5.5 ms) or with a ~31 ms gap and a ~3 ms gap every few
  frames (sd ~7–9 ms, p95 ~30–34 ms). Native GPU-direct shares were in the
  uneven state 5 of 5 times, CUDA-scaled ones 10 of 13 and CPU-scaled ones 3
  of 13, so with CUDA a scaled share paces like a native one (median p95
  30.6 ms against 25.2 ms on the CPU; steady 58.3 against 58.7 fps). 120 and
  144 fps reach 72–95 fps. A tested fix (ddagrab `framerate=1001` + a
  `select` to the target rate + `-r`) holds exactly 60/120/144 fps, also on
  a still screen, but costs ~0.8–1.0 more CPU core (ddagrab then polls);
  `scratchpad/hwlive/capture/pace-experiment.diff`. Not shipped: the
  owner's choice. (`dup_frames=0` + `fps` stops sending on a still screen.)
- **H.264 on the wire** (filler dropped): 7.0 Mbps at 1440p and 5.1 Mbps
  native against a 35 Mbps target, no filler left (FFmpeg's own output was
  35 Mbps, 85% filler). HEVC 0.2× and AV1 0.83–0.90× of target, no padding.
- **Native 4K60 AV1** still GPU-direct: started in 0.73–0.98 s, ≤0.05
  cores, keyframe every 120 frames, stop in 53–67 ms, no FFmpeg left.

## Live app checks (owner at the PC, 2026-10-11)

Two packaged copies of the current `GloorpFlorp.exe` on 127.0.0.1
(`GLOORP_FAKE_AUDIO=1`), driven through UI Automation:

- **Share from monitor 0** (Auto → AV1 1440p60 via CUDA scaling): host
  "Sharing" after ~1.0 s, viewer window ~1.6 s, first picture ~3.3 s (the
  viewer waits for the next keyframe, one every 2 s). 17–19 Mbps, 59–61
  fps, 0 dropped. Glass-to-glass on one PC (no network, a clock window
  read from the screen and from the viewer in the same capture): median
  21 ms (16–22 over 25 grabs).
- **Portrait monitor:** "Sharing AV1 810×1440", viewer 9:16 and upright
  (50 dB as-is against 21 dB turned).
- **Codec change while watching** (AV1 → HEVC → H.264 → AV1): new picture
  2.5–2.65 s after the click (1.5 s of it is the settings wait); the old
  picture freezes ~0.45 s, then ~0.1 s no window and ~0.15 s black.
- **Viewer closed / Open viewer:** the host shows "friend's viewer is
  closed" in 0.21–0.28 s and sends nothing; picture back 1.4–2.2 s after
  Open viewer.
- **120 fps (experimental):** 71–79 fps sent and only 10–13 of 20 Mbps
  (CBR budgets each frame at 1/120 of the bitrate), 0 dropped.
- **Push-to-talk (F24, bound with the key button):** see the checklist;
  with the key up only background traffic (8–9 datagrams/s) goes out.
- **Viewer focus:** an auto-opened viewer stayed behind the window in
  front (foreground logged every 10 ms, 3 runs).
- **Game audio while sharing, Leave, closing the window:** game audio shows
  on the other side in ~0.1 s; everything stops in 0.14–0.31 s and no
  FFmpeg/mpv is left. Logs: no ERROR, only the dev-override WARN.
- Small UI nit: with Voice activation, "Your mic: Talking" can show while
  not connected (nothing is sent then).

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
  false (window closed), send `Watching{on:false}`. `Viewer::start` can
  block ~0.4 s (starting mpv.exe), so the engine runs it on a helper thread.
- `gf_sys::LoopbackCapture::start` can block up to 10 s and dropping it up to
  2 s, so do both off the UI thread. Poll `LoopbackCapture::error()` to show
  "the program you picked closed".
- Test env vars: `GLOORP_FAKE_AUDIO=1` (fake mic/output/game tone),
  `GLOORP_FAKE_VIDEO=1` (test pattern + software encoders; the viewer is
  mpv when it is found, else an FFmpeg null sink, also on Windows). With
  mpv on PATH, `cargo test -p gloorpflorp` opens a real, focus-taking mpv
  window for a few seconds. The end-to-end test on the windows-latest
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
  CREATE_NO_WINDOW, MMCSS, the egui window, ddagrab/NVENC, mpv, a live
  share in the app, GetAsyncKeyState push-to-talk (synthesized F24). Not
  yet exercised for real: push-to-talk with a real (or admin) game in
  front, the mouse pointer on the portrait monitor, the Windows Firewall
  prompt, a session over Tailscale.
- **The screen saver blocks capture.** While it runs (or the PC is locked),
  Windows refuses Desktop Duplication ("Desktop duplication access denied";
  the app shows its plain "Windows refused to let GloorpFlorp capture the
  screen…" message) and synthesized key presses. Test sharing with someone
  at the PC.
- **Capture pacing:** ddagrab delivers frames in ~15.6 ms steps (the
  Windows timer), so 60 fps often has a ~31 ms gap every few frames (p95
  ~30 ms; worse on a still screen, p95 up to ~54 ms) and 120/144 fps reach
  only ~72–95 fps (in the app at 120: 71–79 fps with ~11.5 of 20 Mbps).
  Since the CUDA path is on, scaled shares pace like native ones; the CPU
  way delivered evenly more often (see "Live-desktop checks"). The tested
  fix (`pace-experiment.diff`) costs ~1 CPU core; the owner decides.
- **CUDA scaling/turning** (`args::CUDA_TRANSFORM`) has only run on the RTX
  5080. Each frame goes GPU → system memory → GPU (a few % GPU, ~97 MB).
  `scale_d3d11` (fails on NVIDIA with FFmpeg 8, error 80070057) is no longer
  tried. If CUDA doesn't work on the friend's 3070 Ti, the CUDA step exits
  in ~0.8 s and the CPU step runs, but that also needs CUDA (NVENC with
  system-memory frames), so only native upright shares would work there;
  the message tells the user to update the NVIDIA driver. If
  only the CUDA filters fail, the CPU step costs 0.4–2.1 cores.
- **Viewer focus:** in the app an auto-opened viewer stayed behind the
  window in front and didn't take focus (3 runs; earlier tests where mpv
  took the foreground had the test program itself in front). Not yet tried
  with a real fullscreen game (e.g. Tarkov).
- **A CUDA step that failed once** (e.g. no picture within 5 s for a
  passing reason) while the CPU step then worked makes every later scaled
  or turned share in that run of the app use the CPU (0.4–2.1 cores) until
  GloorpFlorp is restarted. By design, but now that the CUDA step normally
  works, a restart is the fix if CPU use looks high.
- Starting mpv.exe is slow when no mpv is running (~0.37 s, probably
  antivirus scanning the 120 MB unsigned file); the engine no longer waits
  for it.
- A turned (portrait) monitor is always transformed, also at native size
  (2160×3840): with CUDA ~0.25 cores; on the CPU fallback 1.8 (H.264) to
  2.1 (AV1) cores.
- mpv's messages now reach the log: each viewer open adds ~10 harmless
  warnings ("no timestamps", "No video PTS", decoder errors at the end of a
  stream). Don't read them as the cause of a problem.
- `wasapi`'s `new_application_loopback_client` waits with no timeout. If
  Windows never activates capture, `start()` gives up after 10 s and leaves
  a stuck thread (logged).
- `list_monitors` only lists adapter 0's outputs (what ddagrab uses).
- CI on a private repo: Windows minutes count double, and each zip is
  ~110–130 MB against a 500 MB free artifact quota. The workflow cancels
  older runs on the same branch and keeps artifacts 14 days. Consider
  building the package only on main/tags/manual runs.
- FFmpeg/mpv downloads take "latest", so builds aren't reproducible.
  VERSIONS.txt in each zip records exactly what went in.
- Licensing: the workspace says MIT, but there is no LICENSE file. The owner
  should confirm. FFmpeg and mpv are GPL; the notices cover private use
  between two people.
