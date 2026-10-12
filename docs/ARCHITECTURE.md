# GloorpFlorp architecture

Private screen sharing + voice chat for exactly two Windows PCs with NVIDIA
GPUs, connected over Tailscale. No accounts, database or analytics, and no
servers in a room: the two PCs always connect directly. The only server is an
optional lobby the users run themselves (see [Lobby](#lobby-optional)); the
app works the same without it.

## Goals (v1)

- Share a whole monitor at up to 1440p, 60 fps (or more), NVENC encoded
  (AV1 when the sender's GPU can encode it, else HEVC, else H.264), low delay.
- Either person can share; both can share at the same time.
- Game audio: only the game's sound (or "everything except this app"), stereo,
  high quality. The sharer still hears their own game normally.
- Voice: two-way, push-to-talk (global key, works in-game) / voice activity /
  always on, noise suppression, echo cancellation, auto gain, mute, deafen,
  separate volumes for friend's voice and friend's game audio.
- Viewer: friend's screen opens in its own resizable window (fullscreen with
  `f` or double-click).
- Rooms: the host clicks "Host", gets a code like `K7QM-2XPA-9TD`, sends it
  to the friend, who pastes it and clicks "Join". With the optional lobby,
  "Invite" does the same without copying the code.
- Connection details panel (ping, bitrates, drops, audio loss) and a
  plain-language error line.

Non-goals: more than two people, remote control, recording, non-Windows or
non-NVIDIA support, internet use without Tailscale.

## Big decisions (and why)

| Decision | Why |
|---|---|
| Rust app, std threads + channels, no async runtime | Small, predictable, easy to reason about. |
| Video capture+encode by a **bundled FFmpeg** child process (`ddagrab` → `*_nvenc`) | FFmpeg's NVENC and Desktop Duplication code is used by millions; writing our own NVENC/D3D11 FFI blind is the riskiest possible thing. Frames stay on the GPU. |
| Viewer window is a **bundled mpv** (0.41 or newer) child process fed through stdin | GPU decoding, scaling, fullscreen, window handling for free, battle tested low-latency mode. |
| Video over **TCP**, audio over **UDP** | Tailscale paths are direct and low loss; TCP gives a never-corrupted picture with very little code. A frame-aware send queue drops to the next keyframe if the network falls behind. Audio must never wait behind video, so it gets its own UDP socket. Moonlight-style UDP+FEC video is a possible v2. |
| No TLS | Tailscale (WireGuard) already encrypts and authenticates both machines. |
| FFmpeg outputs FLV, mpv gets a raw stream with a trailing delimiter after each frame | Raw H.264/HEVC/AV1 streams have no frame lengths, so a parser only knows a frame is finished when the next one starts — a full frame of extra delay at each end. FLV has explicit lengths; on the viewer side we write the next frame's delimiter immediately so mpv's parser completes the frame at once. See `gf_proto::bitstream`. |
| Keyframe every 2 s | The FFmpeg CLI can't be asked for a keyframe on demand; periodic keyframes let a viewer start and let the queue recover. |
| Opus for both audio streams, 10 ms frames | Lowest delay; in-band FEC for loss. |
| `sonora` (pure Rust WebRTC audio processing) | Echo cancellation (AEC3), noise suppression, AGC with no C++ build. |
| WASAPI process loopback (`wasapi` crate) | Capture only the game, or everything except our own playback, so the friend never hears themselves echoed. |
| `GetAsyncKeyState` polling for push-to-talk | Works while the game has focus, no hooks or injection (anti-cheat friendly). |
| egui (eframe) UI | Pure Rust, one small window. |
| Settings as one JSON file | "No database" — just remembered preferences, stored locally. |
| GitHub Actions builds a ready-to-run zip | The user never installs a compiler; FFmpeg and mpv are bundled in the zip. |

## Crates

```
crates/gf-proto   pure: wire format, room codes, FLV demuxer + viewer stream writer, jitter buffer
crates/gf-sys     OS-specific: monitors, process loopback audio, key polling,
                  Tailscale IP, child-process job object,
                  audio thread priority (MMCSS)  (stubs on non-Windows)
crates/gf-net     the TCP+UDP session between the two PCs
crates/gf-video   FFmpeg encoder process, mpv viewer process
crates/gf-audio   mic → processing → Opus, game audio → Opus, jitter, mixing, playback
crates/gf-lobby   optional lobby: protocol, the app's client, the server binary
app               engine (wires everything) + egui UI + settings + logging
```

Dependency direction: `app → {gf-net, gf-video, gf-audio, gf-sys} → gf-proto`
(`gf-video` and `gf-audio` also use `gf-sys`). `app → gf-lobby`, which uses no
other gf crate (so its server builds alone in Docker). Crates never depend
sideways otherwise. The public API of each crate is in its `lib.rs`; doc comments
there are the contract.

## Data flow

```
 PC A (sharing)                                         PC B (watching)
 ffmpeg ddagrab→nvenc→FLV ─stdout─► Encoder ─VideoFrame─► NetTx ══TCP══► Net ─► Viewer ─stdin─► mpv window
 game process audio ─► LoopbackCapture ─► Opus ─► NetTx ══UDP══► Net ─► jitter ─► Opus dec ─┐
 mic ─► sonora ─► gate ─► Opus ─────────────► NetTx ══UDP══► Net ─► jitter ─► Opus dec ─┴► mixer ─► speakers
```

Everything is symmetric: both PCs run the same pipelines in both directions.

## Session lifecycle

1. Host: engine finds the Tailscale IPv4 (`gf_sys::tailscale_ipv4`), makes a
   `RoomCode`, starts `gf_net` as `Role::Host` bound to that IP only, shows
   the code. If Tailscale isn't running, the UI says so in plain words.
2. Joiner: pastes the code; engine parses it and starts `Role::Join`.
3. `Connected` → both sides start/keep audio running and send voice.
4. Share: user clicks "Start sharing" → `Encoder::start` → on `Started` send
   `ShareStarted{codec,w,h,fps}`; frames are sent only while the peer has
   said `Watching{on:true}`. After the peer starts watching, frames are
   skipped until the next keyframe.
5. Watch: on `ShareStarted` the viewer side opens mpv automatically
   (setting, default on) and sends `Watching{on:true}`. Closing the mpv
   window sends `Watching{on:false}`; an "Open viewer" button reopens it.
   mpv is started on a helper thread (launching mpv.exe took ~0.37 s on the
   RTX 5080 PC), so incoming audio keeps flowing; video that arrives
   meanwhile is kept from the latest keyframe on and handed to the new
   viewer, which keeps it until mpv reads (up to 300 frames / 16 MB).
6. Game audio: toggled independently of video (always off at launch);
   `GameAudio{active}` tells the peer (for the UI) once the capture runs.
   Starting/stopping the capture can block for seconds, so the engine hands
   it to its audio thread (which also starts the `AudioEngine`) and keeps
   handling network events meanwhile.
7. Disconnect: stop sending frames, close viewer, `AudioEngine::reset_peer`,
   forget the peer's state. Host keeps listening, and its own sharing and
   game audio keep running (re-announced when the friend rejoins). Joiner
   retries for 30 s, keeping its own sharing and game audio; if it gives up
   (or the host closed the room) they stop. **Leave** always stops your own
   sharing and game audio.

## Codec choice

Each side probes which NVENC encoders work (`gf_video::probe_encoders`) and
announces them in `Hello`/`Welcome`. When sharing, the sender uses the user's
codec setting ("Auto" = best it can encode). Both target GPUs (RTX 30 and
newer) decode AV1, HEVC and H.264 in hardware, so the receiver's decode
ability is assumed; mpv falls back to software decoding if needed.

Defaults: AV1 20 Mbps, HEVC 25 Mbps, H.264 35 Mbps at 1440p60; adjustable
5–80 Mbps.

## Capture and viewer details (from the hardware tests)

- **Turned monitors.** Desktop Duplication delivers a portrait monitor's
  picture unturned. `gf_sys::Monitor::rotation` (from DXGI) tells gf-video
  to turn it upright (`transpose_cuda=dir=clock` for 90°, or
  `transpose=clock` on the CPU fallback), so the stream size is still
  `output_size(monitor_size(i))` (`monitor_size` is the upright desktop
  size).
- **Scaling.** ddagrab → `scale_d3d11` fails on NVIDIA with FFmpeg 8 (error
  80070057, reproduced without capture), and mapping ddagrab's frames to CUDA
  is impossible (`hwmap` error -40). So a scaled or turned share uses the
  CUDA path (`args::CUDA_TRANSFORM`): `hwdownload` → `hwupload_cuda` →
  `scale_cuda`/`transpose_cuda` → NVENC. On a live desktop (RTX 5080) it
  starts in ~0.95 s with ~0.1–0.35 CPU cores, against ~1.6 s and 0.4–2.1
  cores for CPU scaling/turning, which stays as the fallback. A GPU step
  that failed is skipped for the rest of the app run.
- **Frame pacing.** Asked for the share's own rate, ddagrab waits on the
  ~15.6 ms Windows timer, so at 60 fps there was often a ~31 ms gap every
  few frames (p95 ~30 ms, up to ~70 ms on a still screen) and 120/144 fps
  only reached ~66–120 fps with less bitrate per second. So every ddagrab
  step asks for 1001 fps (`args::CAPTURE_POLL_FPS`: a frame time under
  1 ms, so ddagrab never sleeps on that timer and polls instead), and
  `args::pace_filter` keeps the first frame of each 1/fps slot before any
  copy or scaling (`select`; a kept frame comes at least half a frame time
  after the one before) and time-stamps it in slots (`setpts`/`settb`:
  time base 1/fps and no frame rate, so FFmpeg gives NVENC the real rate
  for its bitrate budget). The output stays variable-rate (`-fps_mode vfr`,
  no `-r`): a capture hold-up is a gap in the stream, not a burst of copies
  of the last frame (which is what `-r` did: CFR, ~50 copies at the start
  of a CUDA share at 144 fps, more after each hold-up, and new frames
  dropped). Live on the RTX 5080: exactly 30/60/120/144 fps on every step and
  on a still screen, frame-gap p95 at most ~1.6 ms over the frame time,
  CBR bitrate at 0.92–0.99× of the target at 60/120/144 with a moving
  picture. Cost: ~0.6–1.3 more CPU cores while sharing on the GPU paths
  (polling); see HANDOFF.md for the numbers.
- **Filler.** NVENC's CBR mode pads H.264 (72–78% of the bytes in a quiet
  picture). The FLV demuxer drops H.264 (NAL 12) and HEVC (NAL 38) filler
  and AV1 padding OBUs, so the wire carries only the picture.
- **mpv options** that matter for delay: `--demuxer=lavf` (else mpv's probe
  waits for 8 KB of input), `--hwdec-threads=1` (else H.264/HEVC hardware
  decoding holds 3 frames, ~47 ms). `$` in the window title is written as
  `$$` (mpv expands `${...}`). mpv's stdout and stderr both go to the log.

## Lobby (optional)

An always-on server on a home server in the tailnet (`packaging/lobby`:
Docker, with the official `tailscale/tailscale` container as its network)
that carries text chat, who is online, and invites. Rooms stay 1:1 and
peer-to-peer: an invite carries only a room code, and the invited app joins
exactly as if the code had been pasted. The engine and gf-net don't know the
lobby exists.

- **Protocol** (`gf_lobby`): newline-delimited JSON over TCP 47900, tagged
  by `"t"`, with a protocol version in `Hello`/`Welcome`. The server reads
  client lines with a 16 KB cap (longer ends the connection), ignores lines
  that don't parse, cleans names (40 chars) and messages (1000 chars), rate
  limits chat and invites (bursts of 5, then 1/s per connection), and takes
  at most 64 connections. `Ping` every 20 s; 60 s of silence drops a
  connection.
- **Identity:** the connection's Tailscale address (Tailscale already
  authenticates devices; no accounts or passwords) plus the name the app
  sends. Only Tailscale addresses (100.64.0.0/10, fd7a:115c:a1e0::/48) and
  loopback are accepted; loopback connections (development) are told apart
  by name. This needs Tailscale's kernel networking in the container
  (`TS_USERSPACE=false`): in userspace mode every connection would come from
  127.0.0.1.
- **Storage:** `members.json` (everyone who ever joined, written via a
  temporary file; a damaged one stops the server rather than being
  overwritten) and `chat.jsonl` (appended per message) in the data volume.
  The last 200 messages are sent to each newcomer.
- **Presence:** every 5 s the server asks tailscaled's LocalAPI over its Unix
  socket (shared from the tailscale container through a volume):
  `GET /localapi/v0/status` (`Host: local-tailscaled.sock`), mapping each
  `Peer[*].TailscaleIPs` to `Peer[*].Online` (`Self` is the server). States:
  *In the lobby* (app connected), *Online* (Tailscale online, app not in the
  lobby), *Offline*. Without the socket (e.g. on Windows) only *In the
  lobby* / *Offline*.
- **App:** the window owns a `gf_lobby::client::Client` (its own threads,
  reconnects after 1 s doubling to 30 s, repaints the window on events) while
  a lobby address is set. Invites are built on the existing commands: A's
  window sends `Command::Host`, and once the room is open (`Hosting` with a
  code) sends `Invite{to, code}`. B's window shows Accept/Deny and asks for
  attention (taskbar flash); Accept sends `Command::Join(code)`. Deny, or no
  answer within 60 s, makes A's window send `Command::Leave` if the room is
  still waiting. An invite that arrives while B isn't idle is declined
  automatically, so a call is never interrupted.

## Ports and firewall

Host uses TCP and UDP port 47800 on its Tailscale address. The first time
the host app listens, Windows Firewall asks for permission; the user must
click Allow. The README explains this. The optional lobby server listens on
TCP 47900 (in the Docker setup it is reachable only through Tailscale); the
app only connects out to it.

## Testing strategy

- `gf-proto`: unit tests for everything, including demuxing real H.264 /
  HEVC / AV1 FLV streams produced by the local FFmpeg (software encoders) and
  checking the viewer byte stream decodes cleanly, also when started from a
  later keyframe.
- `gf-net`: loopback tests on 127.0.0.1 (host + joiner in one process).
- `gf-lobby`: a real server and clients on 127.0.0.1 (chat, persistence
  across a restart, invite routing, presence, malformed and oversize lines,
  rate limit), plus parsing a canned tailscaled status. CI's Linux job builds
  the Docker image and starts the server in it.
- `gf-video`: non-Windows dev mode uses `testsrc2` + software encoders, so the
  encoder process + splitter can be tested on Linux.
- `gf-audio`: unit tests for the pure parts (resampling glue, gating, Opus
  round trip, mixer math); device code is exercised manually.
- `app`: an end-to-end test runs two engines (host + joiner) on 127.0.0.1
  with fake capture (`GLOORP_FAKE_AUDIO=1`, test video source) and checks
  that video frames and audio packets arrive both ways.
- Windows: the workspace builds and its tests run natively on
  `x86_64-pc-windows-msvc` (the owner's PC and GitHub Actions); on Linux it
  can be cross-checked for `x86_64-pc-windows-gnu`. Real NVENC / mpv
  behaviour can only be confirmed on the two PCs (see "Hardware test
  results" in [HANDOFF.md](HANDOFF.md)).
