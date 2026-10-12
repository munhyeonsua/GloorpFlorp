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
| `gf-net`: TCP+UDP session, handshake, reconnect, video queue, stats | ✅ done | 36 pass |
| `gf-video`: FFmpeg encoder (ddagrab or one window's gfxcapture → NVENC → FLV), mpv viewer | ✅ done | 40 unit + 3 integration pass |
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
`GloorpFlorp.exe` (CUDA scaling on, capture pacing fix in: see "Capture
pacing fix").

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
   - [x] Next-session checks prepared by the fixers (portrait direction, `CUDA_TRANSFORM = true`, pacing, H.264 bitrate): done, see "Live-desktop checks". Still open: the mouse pointer on the portrait monitor. The pacing experiment is now shipped (the owner's choice: smooth, ~1 more CPU core; see "Capture pacing fix"). Scripts: `scratchpad/hwlive/capture/` (harness, `run.ps1`, `pointer.ps1`, `pace-experiment.diff`) and `scratchpad/hwlive/app/` (two-copy app checks). (Scratchpads are under `%TEMP%\claude\…`; they may be gone.)
4. **Push without `[skip ci]`** so GitHub builds the zip. Check both jobs go green.
5. **Release v1.0.0.** Prepared: MIT `LICENSE`, version 1.0.0 in
   `Cargo.toml`, exact FFmpeg/mpv source links in the package and source
   archives attached by the release job (see "How to release" below). Done
   by tagging `v1.0.0` once CI is green on the release commit; the friend
   then downloads `GloorpFlorp-windows-x64.zip` from the Releases page (no
   GitHub account needed). Check the published release lists the zip and
   the source archives: four, or five when the FFmpeg inside mpv is a
   different commit from ffmpeg.exe's (`licenses/SOURCES.txt` says which).
6. **First session with the friend.** The friend's 3070 Ti PC needs an up-to-date NVIDIA driver (FFmpeg master needs a recent NVENC API). Judge delay and echo by ear (nothing audible was played in the tests), and keep both PCs' log files. 60 fps is the default; 120/144 now send exactly that rate, but spread the same bitrate over more frames, and the pacing fix's CPU cost (~1 core) is only measured on the 9800X3D, so check Task Manager on the 3070 Ti PC.

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
  (`ddagrab`), not NVENC (8–39% busy). (Fixed; see "Capture pacing fix".)
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
- **120 and 144 fps** were labelled "(experimental)", with the measured rates
  in the tooltip and README. Since the pacing fix they reach their rate, so
  the label is gone; the tooltip and README now say that the same bitrate
  is shared by more pictures.
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
- **Capture pacing** (before the fix; see "Capture pacing fix"): at 60 fps, frames come either evenly
  (frame-gap sd ~3.2–5.5 ms) or with a ~31 ms gap and a ~3 ms gap every few
  frames (sd ~7–9 ms, p95 ~30–34 ms). Native GPU-direct shares were in the
  uneven state 5 of 5 times, CUDA-scaled ones 10 of 13 and CPU-scaled ones 3
  of 13, so with CUDA a scaled share paces like a native one (median p95
  30.6 ms against 25.2 ms on the CPU; steady 58.3 against 58.7 fps). 120 and
  144 fps reach 72–95 fps. A tested fix (ddagrab `framerate=1001` + a
  `select` to the target rate + `-r`) holds exactly 60/120/144 fps, also on
  a still screen, but costs ~0.8–1.0 more CPU core (ddagrab then polls);
  `scratchpad/hwlive/capture/pace-experiment.diff`. The owner chose to ship
  it (see "Capture pacing fix"). (`dup_frames=0` + `fps` stops sending on a
  still screen, so that variant is not used.)
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
- **120 fps (experimental, before the pacing fix):** 71–79 fps sent and
  only 10–13 of 20 Mbps (CBR budgets each frame at 1/120 of the bitrate),
  0 dropped. Not re-run in the app since the fix; through the real encoder
  120 fps now gives 120.0 fps and 0.92–0.97× of the bitrate.
- **Push-to-talk (F24, bound with the key button):** see the checklist;
  with the key up only background traffic (8–9 datagrams/s) goes out.
- **Viewer focus:** an auto-opened viewer stayed behind the window in
  front (foreground logged every 10 ms, 3 runs).
- **Game audio while sharing, Leave, closing the window:** game audio shows
  on the other side in ~0.1 s; everything stops in 0.14–0.31 s and no
  FFmpeg/mpv is left. Logs: no ERROR, only the dev-override WARN.
- Small UI nit: with Voice activation, "Your mic: Talking" can show while
  not connected (nothing is sent then).

## Capture pacing fix (owner at the PC, 2026-10-11)

Shipped by the owner's choice (smooth video for ~1 more CPU core).
`args::encoder_args`: every ddagrab step (GPU-direct, CUDA scale/turn, CPU
fallback) asks ddagrab for `framerate=1001` (`CAPTURE_POLL_FPS`: a frame
time under 1 ms, so ddagrab never sleeps on the 15.6 ms timer) and puts
`pace_filter` before any other filter:
`select='if(gte(floor(t*fps),ld(0))*gte(t,ld(1)),st(0,floor(t*fps)+1)+st(1,t+1/(2·fps)),0)'`
keeps the first frame of each 1/fps slot (at least half a frame time after
the last kept one), then `setpts=floor(T*fps)/fps/TB:strip_fps=1,settb=1/fps`
time-stamps it in slots and removes ddagrab's 1001 fps frame rate, so FFmpeg
hands NVENC the time base's rate (`fps`) for its bitrate budget. The output
is `-fps_mode vfr` (FLV's default, spelled out), with no `-r`. The test
pattern is unchanged. The numbers in the filter are whole numbers (FFmpeg
divides), so no float formatting or locale is involved. Tests: the filter
text and its quoting for FFmpeg's parsers, every step × 30/60/120/144 fps ×
codec × monitor shape (no `-r`, `-fps_mode vfr`), the pacing rule on
simulated 1 ms frames with a 100 ms hold-up (every slot filled, a gap for
the hold-up, no burst), and the real FFmpeg on a 1001 fps `testsrc2` with a
100 ms hold-up giving exactly the frames and slot timestamps of the Rust
model at 30/60/120/144 fps (skipped without FFmpeg or with one older than
8.0, which has no `strip_fps`; with `-r` it gives 13 extra frames at 144
and fails). `strip_fps` makes FFmpeg 8.0 the minimum for sharing: see
"Known risks".

**First version used `-r fps` (review follow-up).** `-r` makes FFmpeg
constant-rate: it fills any hold-up with copies of the last frame, sent in a
burst just before the new one, and drops new frames that land too close to
its grid (FFmpeg refuses `-r` with `-fps_mode vfr`). So the half-frame rule
never reached the stream, and "sent N fps" counted the copies. Live (6 s,
output to NUL): monitor 0 native 144 fps dup=26 drop=9; CUDA 1440p 144 fps
dup=51, mostly a startup burst (ddagrab's first frame comes ~0.3 s after
its clock starts and FFmpeg filled that time with copies); the same runs
with the variable-rate filters: dup=0 drop=0. Bitrate is unchanged
(lavfi noise, 1280×720, AV1/HEVC/H.264 at 144 fps: 1.01–1.02× of target
both ways; without either fix 0.15×), and the H.264/HEVC/AV1 headers are
byte-identical.

Re-measured live with the shipped (variable-rate) arguments against the
`-r` version, alternating, same 7 paths × 30/60/120/144 fps
(`scratchpad/hwpace/fix/results-vfr.txt`): steady rate the same (29.99–30.00,
60.00–60.03, 119.97–120.01, 143.96–144.02); whole-run rate now matches it
(144: 144.1–144.6 fps against 146.9–149.1 with `-r`; 120: 120.2–120.5
against 122.4–124.3; 60: 60.2–60.5 against 60.3–62.1), because the startup
copies are gone (CUDA paths at 144 fps: 2–6 frame gaps under 4 ms in the
whole run against 37–53). Gap sd 0.4–1.5 ms at 60–144 fps (with `-r`
0.8–2.9), p95 within 1.8 ms of the frame time, max ≤ 12.2 ms at 144 apart
from one 6 s run per CPU-fallback case at 16–18 ms (repeated 3× each: 0–1
gaps over 1.5×, max 7.9–12.2 ms, same as `-r`). Bitrate per second, start
time (730–940 ms GPU, 815–1550 ms CPU fallback), CPU cores (0.6–1.5 GPU
paths, 1.6–5.0 CPU fallback, 3.3–5.0 of it at 144 fps over the repeats; `-r` 3.2–4.9) and keyframes every `2·fps` frames are the same
as with `-r`; every stream decoded in full (AV1 0 errors; H.264 only the
known prologue message). Real mpv viewer (AV1 1440p60, 8 s, 2 runs each,
alternating): first picture 30.7–34.8 ms after the keyframe (`-r`:
30.2–36.4, so the earlier 36–40 was run-to-run spread), display gap max
19.3–21.1 ms, 0 over 25 ms, 481 of 481 frames shown, push-to-display
median 0.28–0.29 ms, 0 mpv errors. The table below is from the `-r`
version; its steady numbers hold for the shipped one.

Live, through the real `gf_video::Encoder` (packaged FFmpeg N-127271), HEAD
and the fix alternating in one session, 6 s per run, 7 paths: monitor 0
native AV1 (GPU-direct), monitor 0 1440p AV1 and H.264 (CUDA), monitor 1
(portrait, a still screen) 1440p AV1 and native AV1 (CUDA turn), and the
CPU fallback (monitor 0 1440p AV1, monitor 1 1440p H.264; forced with a
scratch copy of gf-video built with `CUDA_TRANSFORM = false`, so the
shipped `Step::Cpu` arguments ran). Per frame rate, ranges over the 7 paths
(steady fps after the first second; gap = time between frames):

| fps | build | steady fps | gap sd ms | gap p95 ms | gap max ms | gaps > 1.5× (sum) |
|---|---|---|---|---|---|---|
| 30 | HEAD | 29.9–30.1 | 7.1–21.4 | 43.8–71.1 | 53–123 | 80 |
| 30 | fix | 30.00 | 2.3–5.8 | 33.9–34.9 | 34.1–35.9 | 0 |
| 60 | HEAD | 56.8–59.0 | 5.2–16.1 | 29.8–52.8 | 38–71 | 475 |
| 60 | fix | 59.99–60.01 | 1.0–2.9 | 17.0–17.9 | 17.3–18.5 | 0 |
| 120 | HEAD | 66–107 | 4.7–9.6 | 18.8–26.4 | 32–68 | 1758 |
| 120 | fix | 119.98–120.01 | 0.6–1.5 | 9.0–9.6 | 9.5–10.2 | 0 |
| 144 | HEAD | 84–120 | 6.4–8.5 | 19.3–23.2 | 32–62 | 1844 |
| 144 | fix | 143.96–144.01 | 0.8–1.2 | 7.4–7.9 | 7.9–11.6 | 1 |

- **Still screen** (portrait monitor, ~0.3–2.6 KB per frame): the fix
  keeps the exact rate (HEAD there was the worst: p95 up to 71 ms at 30 fps,
  53 ms at 60), keyframes every 2 s, streams decode.
- **Keyframes:** every `2·fps` frames = every 2.0 s in every run of the fix
  (at 120/144 HEAD's 240/288-frame spacing took 2.2–3.6 s); every keyframe
  decodable on its own (0 bad).
- **Bitrate** with a moving picture (a non-focused mpv window playing a
  1920×1080 test pattern on monitor 0), 1440p, target 20 Mbps AV1 / 35 Mbps
  H.264: 60 fps HEAD 0.91/0.90× → fix 0.99/0.98×; 120 fps 0.59/0.58× →
  0.92/0.97×; 144 fps 0.55/0.57× → 0.95/0.95×. On the mostly still desktop
  bitrate follows the picture (AV1 0.04–1.0×, H.264 filler dropped), the
  same in both builds.
- **CPU (FFmpeg, cores, steady):** native GPU-direct 0.01–0.08 → 0.63–0.97;
  CUDA 0.08–0.53 → 1.08–1.52; CPU fallback 0.43–3.45 → 1.65–4.27. GPU and
  NVENC load rise with the frames encoded (monitor 0 1440p AV1 at 144 fps:
  GPU 20% → 32%, NVENC 10% → 18%).
- **Start-up** (`Started`): native 743–823 → 740–760 ms, CUDA 902–988 →
  902–966 ms, CPU fallback 824–1670 → 815–1585 ms: not slower. Stop 53–69
  ms, no FFmpeg left behind.
- **Decoding:** every stream decoded with FFmpeg, all frames (with
  `-fps_mode passthrough`; raw H.264 at 120/144 needs it, else FFmpeg's
  null output drops frames by timestamp), 0 errors for AV1. H.264 shows the
  one pre-existing message for the 6-byte opening access unit of the viewer
  prologue ("missing picture in access unit"), in both builds.
- **With motion at 120/144** (GPU also drawing the test window): 1–17 gaps
  over 1.5× the frame time per 6 s run, max 14–18 ms (HEAD: ~300–340).
- **Real mpv viewer** (Encoder monitor 0 → `Viewer` with mpv 0.41, AV1
  1440p60, 8 s after the first keyframe, 2 runs each): frames shown on
  screen arrive evenly — gap max 18.9–20.6 ms, 0 over 25 ms (HEAD: max
  34–40 ms, 33–34 gaps over 25 ms); push-to-display median 0.29 ms (HEAD
  0.31–0.32), so no added delay; every pushed frame shown (481 of 481), no
  skips, 0 mpv errors. First picture after the keyframe 36–40 ms (HEAD
  31–33 ms; mpv start-up/decoder, n=2; re-run later with 2 more runs per
  build: 30–36 ms either way, so not a regression).

Scripts and evidence: `scratchpad/hwpace/` (`run1.ps1`, `matrix.ps1`,
`motion.ps1`, `results.txt`, `motion.txt`, `ev/`); raw streams were
deleted after decoding.

## Window sharing (one program; RTX 5080, 2026-10-12)

Owner's ask #4. **Share** in "Your screen" lists the monitors, then "Only
one program: exe - title" (the game-audio program list,
`gf_sys::list_audio_apps`, which now also returns each program's window as
`AudioApp::hwnd`). The setting is `share_app` (exe name; missing or `null`
= the monitor, so old settings files load). Starting a share looks the exe
up again on the encoder's start thread and captures its front-most window
with FFmpeg's `gfxcapture` (Windows Graphics Capture):

```
-f lavfi -i gfxcapture=hwnd=0x…:max_framerate=1000:capture_cursor=1:display_border=0:width=W:height=H:resize_mode=scale_aspect:scale_mode=bicubic
-vf select='if(gte(floor(t*F),ld(0)),st(0,floor(t*F)+1),0)',setpts=floor(T*F)/F/TB:strip_fps=1,settb=1/F   (+ hwdownload,… on the CPU step)
-fps_mode vfr -c:v *_nvenc … -g G … -force_key_frames expr:isnan(prev_forced_t)+gte(t,prev_forced_t+G/F) -forced-idr 1
```

W×H is `args::window_output_size`: the window's inside at start, scaled to
the Resolution cap, even, at least 256×144 (AV1 NVENC refused 146×50; HEVC
and H.264 128×72). Monitor shares' arguments are unchanged (their tests
pass as before). Ladder: GPU-direct, then CPU; no CUDA step (gfxcapture
scales by itself; the research measured 624 MB with CUDA against 222 MB).

Changed from the research's recommendation (`scratchpad/gfxcap`, measured
under the screen saver), each measured here with the desktop visible:
- **`max_framerate=1000` + slot timing instead of `max_framerate=fps` + a
  cap `select`.** This desktop stamps window frames on a 1/240 s grid
  (monitor at 144 Hz), and Windows' own limit drops frames that come one
  step early. Through NVENC (AV1 20 Mbps, 7 s, 2 runs each), research way
  / shipped: 144 fps window at 60 fps 59.1–59.3 / 60.04–60.07; 60 fps
  window at 60 58.7–59.4 / 59.94–60.00; at 120 109.7–111.0 / 116.5–119.4;
  at 144 115.9–119.0 / 114.4–141.5 (the window itself delivered ~140 fps).
  Bitrate shipped 0.96–1.00× (research way 0.81–0.95×). Cost: 0.03–0.14 CPU
  cores (0.02–0.07), GPU 3D ~2% (~1%), same memory.
- **Keyframes by `prev_forced_t`, not `n_forced*N`:** real FFmpeg (libx264,
  30 fps, 5 s pause) gave keyframes at frames 0, 30 against 0, 30, 31, 32
  (a burst after every pause). `-forced-idr 1` for AV1 too (av1_nvenc
  ignores forced keyframes without it).
- **Still windows:** a still window sends nothing, so a new viewer would
  wait forever for a keyframe. Instead of restarting the encoder (which
  fails while the window is minimized), the engine keeps its share's latest
  keyframe group (`PendingFrames`, ≤300 frames / 16 MB) and sends it to a
  viewer that started watching once the encoder has sent nothing for 1 s
  (`STILL_SHARE`), queued in one go with `NetTx::send_video_group`: sent
  frame by frame, a group of more than 30 frames (gf-net's
  `MAX_VIDEO_QUEUE`, ~0.5 s of motion at 60 fps) was thrown away by the
  queue, keyframe and all. Never triggers on a monitor share (ddagrab
  always delivers frames). A larger group isn't kept, so that viewer waits
  until the window changes (a `shortcut:` in the engine).
- **Settings changed while the window is minimized:** restarting the
  encoder would fail (start refuses a minimized window) and end the share,
  so the restart waits until the window is restored.

Live, through the real `gf_video::Encoder` and two real engines (packaged
FFmpeg N-127271; test window = a renamed copy of mpv playing `testsrc2`;
harness and scripts in `scratchpad/winshare/`):
- **Start:** `Started` 0.23–0.27 s after `Encoder::start` (once 0.74 s);
  FFmpeg 0.02–0.11 cores, 79–92 MB at 1280×720, 238 MB for a near-4K window
  shared at 1440p.
- **Rates** (shipped graph): 60 → 60.00 fps (gap sd 0.4–0.5 ms on a smooth
  window), 30 → 30.07, 120 → 119.97 at 1.00× bitrate, 144 → 115–141
  (limited by what the window delivers). Alternating all three codecs on
  one window: 59.5–60.05 fps each. In the runs that opened a fresh 60 fps
  window per codec, HEVC and H.264 came out at 52–54 fps twice: that was the
  test window's own cadence (60 fps mpv on a 144 Hz screen), as the
  research also saw.
- **Window events** (AV1 60 fps, 720p): resized to 1600×900, 800×450,
  1000×800: 60.00 fps throughout and the stream stays 1280×720 (767 frames
  decoded, 0 errors; a differently shaped window sits top-left with black
  on the right or bottom). Minimized 3 s: 0 frames, FFmpeg keeps running;
  restored: a keyframe at once, then 60 fps. Closed (WM_CLOSE): `Stopped`
  0.11 s later with "<exe>'s window was closed, so sharing stopped. …".
  Start is refused with plain messages when the program isn't running or
  its window is minimized.
- **Still window:** a paused window sent 1 frame in 8 s. Moving the pointer
  over it sends frames, and the pointer is drawn (`capture_cursor=1`). No
  yellow border around the shared window (checked on the desktop).
- **Two engines** on 127.0.0.1 with a paused window: the friend's viewer
  got the picture 1.5 s after it started watching (17 frames replayed; it
  would stay black without the fix). Closing the window: the host's share
  stopped in 0.08 s with that message as the banner, and the friend's
  viewer closed. A monitor share right after: 60 fps.
- **Group replay** (fix after review; a 60 fps window minimized 0.25, 1.4,
  2.6 or 3.5 s after the share went live, the friend joins ~1.5 s later):
  18, 84–89, 39 and 93 frames replayed, all received, 0 dropped; the
  replay went out 0.03 s after the friend started watching. Before the fix
  (reviewers' runs): 88 sent / 87 dropped / 1 received at 1.4 s (an old
  picture), 41/41/0 at 2.6 s (black). Harness: `scratchpad/winshare-fix/e2e`.
- **Bitrate changed while minimized** (same harness): the share stays
  live; restored, the encoder restarted 0.7 s later at the new bitrate.
  Before the fix the share ended with the "minimized" message.
- The CPU and CUDA forms of the graph also run (180 frames at 60 fps,
  keyframes every 2 s).
- **App:** the Share box (driven through UI Automation) lists the monitors
  and programs; picking one saves `"share_app": "<exe>"`, and it is still
  picked after a restart; the owner's old settings.json (no `share_app`)
  loaded. `%APPDATA%\GloorpFlorp` was backed up first and restored byte for
  byte.

Not verified: a real game (exclusive fullscreen should give no picture,
then "No picture came from <exe>'s window…" after both ladder steps' 5 s;
borderless should work), an elevated (admin) window, the friend's 3070 Ti
and its Windows version (older Windows only warns when it can't hide the
border or set the pointer option), a monitor with display scaling other
than the test PC's.

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
  **Stop sharing**, **Open viewer**, **Share** (monitor or "Only one
  program" for video), **Refresh**, "Everything except GloorpFlorp",
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
- **Capture pacing: fixed, at a CPU cost.** ddagrab is asked for 1001 fps
  and paced to the share's rate with `select` (see "Capture pacing fix"):
  exactly 30/60/120/144 fps, also on a still screen. The cost is ddagrab
  polling: +0.6–1.0 CPU core for a native GPU-direct share, +0.9–1.3 for a
  CUDA-scaled/turned one, +0.8–2.9 on the CPU fallback (up to +3.8 in one 144 fps repeat; part of that is
  the CPU scaling the frames that used to be missing) — measured on the
  9800X3D only. If the friend's PC struggles, 60 fps is cheaper than
  120/144. Untested: a heavily loaded CPU (a game using every core) could
  delay the polling thread and bring back uneven gaps, and with the GPU busy
  drawing another window, 120/144 fps showed a few gaps up to ~18 ms.
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
- **Window sharing** has only run on the RTX 5080 with test windows (see
  "Window sharing"): not with a real game, an admin window or on the 3070
  Ti. A program with several windows shares the one in front when sharing
  starts; a window resized to another shape is drawn top-left, not centred
  (gfxcapture's `scale_aspect`). At 144 fps it sends what the window
  delivers (~115–140 fps here). When a burst of changes ends with two
  frames in one 1/fps slot, the pacing drops the final picture and a still
  window sends nothing to replace it: the friend sees a picture a frame or
  two old until the window or the pointer over it moves (a 144 fps test
  window that stops at 2.993: the last frame sent showed 2.965–2.979 at a
  60 fps share). Windows' own limit (`max_framerate=fps`, no `select`)
  dropped it too (3 of 3 runs at 30 and 60 fps,
  `scratchpad/winshare-fix/lastframe-win.sh`); a fix needs pacing with a
  timer, which FFmpeg's filters don't have.
- **FFmpeg 8.0 or newer is required** for sharing the screen: the pacing
  filter uses `setpts=…:strip_fps=1`, an option FFmpeg 8.0 added (absent at
  n6.1.1, n7.0, n7.1). The zip ships BtbN's master build, so the app is
  fine; but `Tools::locate` falls back to an `ffmpeg` on PATH when none is
  next to the exe, and an older one makes every ladder step fail at once
  with "Error applying option 'strip_fps' to filter 'setpts': Option not
  found". `args::advice` turns that (and "Unrecognized option") into "The
  FFmpeg GloorpFlorp found is too old … needs FFmpeg 8.0 or newer"; the log
  line "FFmpeg: … (from PATH; …)" shows when PATH was used. The test
  pattern (Linux, `GLOORP_FAKE_VIDEO=1`) isn't paced and works with older
  FFmpeg. The real-FFmpeg pacing test (`args::tests::ffmpeg_paces_a_fast_source`)
  skips with a note when `ffmpeg -h filter=setpts` doesn't list
  `strip_fps` (`tools::can_pace`).
- **Linux CI uses BtbN's FFmpeg, not Ubuntu's.** The first CI run with
  Linux tests (run 38160494259, 64f2c23) failed because ubuntu-latest's apt
  FFmpeg is 6.1. The Linux job now downloads BtbN's static
  `ffmpeg-N-…-linux64-gpl.tar.xz` from the newest dated `autobuild-…`
  release (same lookup as the Windows job; `latest` as a fallback), checks
  that its `setpts` has `strip_fps`, and puts it on PATH; apt FFmpeg is no
  longer installed (nothing else needed it). If BtbN can't be reached the
  job fails with a clear error rather than falling back to Ubuntu's FFmpeg
  (the Windows job can't package without BtbN anyway, and the other
  real-FFmpeg tests have only been run against FFmpeg 8). ubuntu-latest
  moves to Ubuntu 26 from 2026-10-19; only `libasound2-dev`, `cmake` and
  `pkg-config` come from apt now. Tests run with `--no-fail-fast` (every
  failure is reported, not just the first test program's) and clippy runs
  even when tests fail. Linux clippy had never run before; a Windows
  emulation (the workspace copied with `cfg(windows)` renamed, built
  without it) found no warnings in the non-Windows code.
- CI: the repository is **public**, so GitHub Actions minutes on standard
  runners are free and artifacts don't count against a private-repo quota.
  The workflow still cancels older runs on the same branch and keeps
  artifacts 14 days (each zip is ~110–130 MB).
- FFmpeg/mpv downloads take the newest build, so builds aren't
  reproducible. VERSIONS.txt and `licenses/SOURCES.txt` in each zip record
  exactly what went in. FFmpeg now comes from BtbN's newest dated
  `autobuild-…` release (not the moving `latest`), so its build scripts are
  known exactly; if the GitHub API lookup fails, the step falls back to
  `latest` with a warning and the release job then refuses to publish
  (re-run all jobs).
- Licensing: resolved. GloorpFlorp's own code is MIT (`LICENSE`, copyright
  munhyeonsua; copied into the zip as `licenses\GloorpFlorp-LICENSE.txt`).
  FFmpeg (BtbN "gpl" build: `--enable-gpl --enable-version3`) is GPL v3 or
  later; mpv is GPL v2 or later, but shinchiro's build includes an FFmpeg
  made with `--enable-gpl --enable-version3`, so that build as a whole is
  GPL v3. mpv.exe has that FFmpeg built in, usually at a different commit
  from ffmpeg.exe; only `mpv --version` ("FFmpeg version: N-…-g<hash>")
  records it, and the packaging reads it from there (`mpv_ffmpeg_commit` in
  `licenses/SOURCES.txt`). The mpv folder also holds Microsoft's
  `d3dcompiler_43.dll` (not GPL), which the notices name. The zip ships both
  GPL texts, and THIRD-PARTY-NOTICES.txt / VERSIONS.txt link the exact
  FFmpeg, mpv and mpv-built-in FFmpeg commits and build-script releases.
  Each GitHub Release also attaches source archives of those commits and
  build scripts. Not attached: the source of every library
  inside the two builds (x264, x265, libass, …). BtbN's scripts at the
  attached tag pin each library's exact version; shinchiro's scripts mostly
  fetch each library's newest code at build time, so for mpv the exact
  library versions are only in that build's logs.

## How to release

1. Set `version` in `Cargo.toml` (`[workspace.package]`; every crate
   inherits it) and build once so `Cargo.lock` follows. The app logs it at
   start and `VERSIONS.txt` shows it.
2. Push the commit and wait for both jobs to go green.
3. Tag that commit with the matching version and push the tag:
   `git tag v1.0.0 && git push origin v1.0.0`. The `release` job refuses
   to publish if the tag doesn't match the first line of `VERSIONS.txt`.
4. The tag build runs the tests and packaging again, then the `release`
   job zips the package as `GloorpFlorp-windows-x64.zip`, downloads the
   source archives named in `licenses/SOURCES.txt` (FFmpeg and mpv at the
   exact commits, the FFmpeg built into mpv.exe at its own commit when it
   differs from ffmpeg.exe's, and BtbN's and shinchiro's build scripts at
   the exact releases), and publishes the zip and those four or five
   archives as a GitHub Release with
   install and license notes plus generated release notes. Anyone can
   download it from the Releases page without a GitHub account.
