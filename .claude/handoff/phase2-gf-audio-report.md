# gf-audio agent report (phase 2, cloud)

Built in a Linux cloud container. Nothing has run on real sound hardware or Windows.

## Summary
gf-audio implements the lib.rs contract; no todo!() remains. Modules: codec, resample, level, gate,
mixer, playout, mic, game, devices, fake, prio, shared, tests.

- Output: one cpal stream (device default rate/channels; f32 preferred, all sample formats handled).
  The callback only pops whole frames from an rtrb ring (zeros + underrun count on underrun; no locks,
  no allocation). A playout thread keeps the ring at max(30 ms, largest device request + 10 ms):
  per 10 ms it pops each stream's JitterBuffer, decodes (Frame / FEC / PLC / Empty=silence), mixes voice
  (mono to both ears) + game (stereo) with volumes/deafen, pushes a mono downmix to the echo-reference
  ring, resamples 48 kHz -> device rate (rubato Fft), maps channels. No device or fake mode: same thread
  runs on its own 10 ms clock as a null sink.
- Mic: callback only downmixes to mono and pushes to a ring. Mic thread resamples to 48 kHz, feeds pending
  reference frames (max 25) to sonora's render side, runs sonora (high-pass always, AEC3, NS High,
  AGC2 adaptive digital; rebuilt on toggle), level, gate, Opus mono VOIP with FEC, 5% loss, 10 ms.
  sonora 0.2 has no VAD output, so voice activation is an energy gate: sensitivity 0..1 -> -30..-55 dBFS,
  400 ms hangover. PTT uses an injectable key fn (default gf_sys::keys::is_down) with 150 ms release
  tail. Mute always wins. seq counts sent packets; ts advances 480 per processed frame, also while gated.
- Game: LoopbackCapture callback only pushes stereo frames into a ring; encoder thread cuts 960-sample
  frames, Opus stereo AUDIO + FEC. On silence a partial frame is padded and sent after 30 ms; level
  drops to 0 after 150 ms. seq/ts continue across capture restarts.
- Devices: a device thread owns the cpal streams (WASAPI stream need not be Send). start() waits up to
  8 s for the first open. Error callbacks only set an AtomicBool; failed streams are reopened; missing
  devices retried every 3 s. A missing chosen device falls back to default with a plain message and
  switches back when it returns. Names from description().name() (cpal 0.18 has no name()); enumeration
  wrapped in catch_unwind.
- Robustness: engine threads catch panics -> plain error in status. NaN/inf samples sanitized. On Windows
  playout/mic/game threads register with MMCSS "Pro Audio" (fallback THREAD_PRIORITY_HIGHEST).
- Fake mode (GLOORP_FAKE_AUDIO=1): no devices; mic = 300 Hz tone at 0.2 amplitude, real-time paced
  (~100 voice packets/s); the tone bypasses sonora (noise suppression removes a steady tone and would
  close the gate). Output = clock-paced null sink.
- status().error joins internal, mic, speaker and game-capture messages. game_capturing = capture exists
  and reports no error.

## API changes
No public signatures changed or added. Doc updates only (crate docs, PacketSink thread + seq/ts, list_*,
blocking notes on start/start_game_audio/stop_game_audio). receive() ignores ts; ordering from seq.

## Requests
- gf-sys (optional): move gf-audio/src/prio.rs (MMCSS boost) into gf-sys.
- app: AudioEngine::start can block ~8 s, start_game_audio ~10 s, stop_game_audio and drop ~2 s; keep
  them off the UI thread. status().error may contain several sentences, including a non-fatal fallback
  note ("The microphone “X” isn't connected, so the default microphone is used instead.") — the UI may
  want to show that as a notice rather than an error.

## Tests (Linux container)
- cargo build -p gf-audio: ok
- cargo test -p gf-audio: 42 passed (Opus round trips, FEC/PLC, bad packets; resampler 44.1/48/96/22.05k;
  gate modes; mixer; PlayoutCore 440 Hz tone with 5% loss + reordering; sonora echo reduction ~30 dB,
  NS keeps speech; game gap flush; fake-mode engine pace, loopback, reset_peer, PTT, mute, bitrate,
  game audio; no-device start with plain messages). Stable 6x normal and 5x under CPU load.
- cargo clippy -p gf-audio --all-targets -- -D warnings: clean
- cargo check / clippy -p gf-audio --target x86_64-pc-windows-gnu: clean
- cargo test -p gf-audio --target x86_64-pc-windows-gnu --no-run: links (not executed)
- cargo fmt -p gf-audio -- --check: clean
- cargo test -p gloorpflorp --test end_to_end: 3 passed

## Unverified
All real sound devices (WASAPI streams, non-f32/multichannel, unplug/replug, default-device switch,
fallback, privacy/busy errors), real-room echo cancellation and delay, how NS/AGC sound on speech,
whether -30..-55 dBFS feels right, PTT while a game has focus, MMCSS, underruns under heavy game load,
real process-loopback game audio, Windows test binary never executed.

## Concerns
- No explicit clock-drift compensation: a faster sender clock causes an occasional dropped 10 ms frame
  (~1 per 100 s at 100 ppm), possibly a faint click.
- Estimated mouth-to-ear latency ~90–120 ms.
- Toggling processing rebuilds sonora; AEC re-adapts for a few seconds.
- NetTx::send_audio runs on the boosted mic thread (stalls absorbed by the 500 ms mic ring).
- Opus in-band FEC only helps SILK/hybrid; game audio at 160 kbps (CELT) gets concealment only.
- Fake mode skips sonora, so the end-to-end test never exercises it (unit tests do).
- On Linux CI cpal prints harmless "ALSA lib … cannot find card" every 3 s.
- Duplicate device names are merged; selection picks the first match. list_* still enumerates real
  devices in fake mode.
- Two simultaneous start_game_audio calls could briefly duplicate game seq numbers.
