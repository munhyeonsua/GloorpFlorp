# GloorpFlorp architecture

Private screen sharing + voice chat for exactly two Windows PCs with NVIDIA
GPUs, connected over Tailscale. No servers, accounts, database or analytics.

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
  to the friend, who pastes it and clicks "Join".
- Connection details panel (ping, bitrates, drops, audio loss) and a
  plain-language error line.

Non-goals: more than two people, remote control, recording, non-Windows or
non-NVIDIA support, internet use without Tailscale.

## Big decisions (and why)

| Decision | Why |
|---|---|
| Rust app, std threads + channels, no async runtime | Small, predictable, easy to reason about. |
| Video capture+encode by a **bundled FFmpeg** child process (`ddagrab` → `*_nvenc`) | FFmpeg's NVENC and Desktop Duplication code is used by millions; writing our own NVENC/D3D11 FFI blind is the riskiest possible thing. Frames stay on the GPU. |
| Viewer window is a **bundled mpv** child process fed through stdin | GPU decoding, scaling, fullscreen, window handling for free, battle tested low-latency mode. |
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
                  Tailscale IP, child-process job object  (stubs on non-Windows)
crates/gf-net     the TCP+UDP session between the two PCs
crates/gf-video   FFmpeg encoder process, mpv viewer process
crates/gf-audio   mic → processing → Opus, game audio → Opus, jitter, mixing, playback
app               engine (wires everything) + egui UI + settings + logging
```

Dependency direction: `app → {gf-net, gf-video, gf-audio, gf-sys} → gf-proto`
(`gf-video` and `gf-audio` also use `gf-sys`). Crates never depend sideways
otherwise. The public API of each crate is in its `lib.rs`; doc comments
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
6. Game audio: toggled independently of video; `GameAudio{active}` tells
   the peer (for the UI).
7. Disconnect: stop encoder sending, close viewer, `AudioEngine::reset_peer`.
   Host keeps listening; joiner retries for 30 s.

## Codec choice

Each side probes which NVENC encoders work (`gf_video::probe_encoders`) and
announces them in `Hello`/`Welcome`. When sharing, the sender uses the user's
codec setting ("Auto" = best it can encode). Both target GPUs (RTX 30 and
newer) decode AV1, HEVC and H.264 in hardware, so the receiver's decode
ability is assumed; mpv falls back to software decoding if needed.

Defaults: AV1 20 Mbps, HEVC 25 Mbps, H.264 35 Mbps at 1440p60; adjustable
5–80 Mbps.

## Ports and firewall

Host uses TCP and UDP port 47800 on its Tailscale address. The first time
the host app listens, Windows Firewall asks for permission; the user must
click Allow. The README explains this.

## Testing strategy

- `gf-proto`: unit tests for everything, including demuxing real H.264 /
  HEVC / AV1 FLV streams produced by the local FFmpeg (software encoders) and
  checking the viewer byte stream decodes cleanly, also when started from a
  later keyframe.
- `gf-net`: loopback tests on 127.0.0.1 (host + joiner in one process).
- `gf-video`: non-Windows dev mode uses `testsrc2` + software encoders, so the
  encoder process + splitter can be tested on Linux.
- `gf-audio`: unit tests for the pure parts (resampling glue, gating, Opus
  round trip, mixer math); device code is exercised manually.
- `app`: an end-to-end test runs two engines (host + joiner) on 127.0.0.1
  with fake capture (`GLOORP_FAKE_AUDIO=1`, test video source) and checks
  that video frames and audio packets arrive both ways.
- Windows: the whole workspace is cross-compiled for `x86_64-pc-windows-gnu`
  locally and built natively for `x86_64-pc-windows-msvc` in GitHub Actions.
  Real NVENC / WASAPI / mpv behaviour can only be confirmed on the two PCs.
