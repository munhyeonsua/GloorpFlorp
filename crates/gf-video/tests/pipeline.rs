//! End-to-end checks of the dev/test path: FFmpeg test pattern + software
//! encoders -> Encoder -> Viewer (FFmpeg null sink). Skipped without FFmpeg.

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use gf_proto::bitstream::is_decodable_start;
use gf_proto::{VideoCodec, VideoFrame};
use gf_video::{probe_encoders, Encoder, EncoderEvent, EncoderSettings, Tools, Viewer};

const GOP: u32 = 10;

fn tools() -> Option<Tools> {
    // Windows uses ddagrab + NVENC unless told to use the test pattern.
    #[cfg(windows)]
    std::env::set_var("GLOORP_FAKE_VIDEO", "1");
    match Tools::locate() {
        Ok(t) => Some(Tools { mpv: None, ..t }),
        Err(_) => {
            println!("note: skipping, ffmpeg is not available");
            None
        }
    }
}

fn settings(codec: VideoCodec) -> EncoderSettings {
    EncoderSettings {
        codec,
        monitor: 0,
        fps: 30,
        max_height: Some(240),
        bitrate_kbps: 1_000,
        keyframe_interval: GOP,
    }
}

/// Waits for `Started`, then collects `n` frames.
fn collect(
    rx: &Receiver<EncoderEvent>,
    n: usize,
) -> ((VideoCodec, u32, u32, u32), Vec<VideoFrame>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut started = None;
    let mut frames = Vec::new();
    while frames.len() < n {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("encoder went quiet") {
            EncoderEvent::Started {
                codec,
                width,
                height,
                fps,
            } => {
                assert!(started.is_none(), "Started twice");
                started = Some((codec, width, height, fps));
            }
            EncoderEvent::Frame(f) => {
                assert!(started.is_some(), "frame before Started");
                frames.push(f);
            }
            EncoderEvent::Stopped { error } => panic!("encoder stopped: {error:?}"),
        }
    }
    (started.unwrap(), frames)
}

fn stop_and_check(mut enc: Encoder, rx: &Receiver<EncoderEvent>) {
    enc.stop();
    let mut last = None;
    while let Ok(ev) = rx.recv_timeout(Duration::from_secs(5)) {
        last = Some(ev);
    }
    match last {
        Some(EncoderEvent::Stopped { error: None }) => {}
        other => panic!("expected Stopped {{ error: None }} last, got {other:?}"),
    }
}

#[test]
fn every_software_encoder_streams() {
    let Some(tools) = tools() else { return };
    let codecs = probe_encoders(&tools);
    if codecs.is_empty() {
        println!("note: skipping, this ffmpeg has none of libx264 / libx265 / libsvtav1");
        return;
    }
    let runs: Vec<_> = codecs
        .into_iter()
        .map(|codec| {
            let tools = tools.clone();
            std::thread::spawn(move || {
                let (enc, rx) = Encoder::start(&tools, &settings(codec)).unwrap();
                let (started, frames) = collect(&rx, 25);
                // The fake monitor is 1920x1080; max_height 240 keeps 16:9.
                assert_eq!(started, (codec, 426, 240, 30), "{codec:?}");
                let keys: Vec<usize> = (0..frames.len()).filter(|&i| frames[i].keyframe).collect();
                assert_eq!(keys, [0, 10, 20], "{codec:?} keyframes");
                for &i in &keys {
                    assert!(
                        is_decodable_start(codec, &frames[i].data),
                        "{codec:?} frame {i}"
                    );
                }
                assert!(frames
                    .iter()
                    .all(|f| f.codec == codec && !f.data.is_empty()));
                assert!(frames.windows(2).all(|w| w[0].sent_us <= w[1].sent_us));
                // -re paces the test pattern in real time (after FFmpeg 6.1's
                // default 0.5 s initial burst), so 25 frames take > ~0.3 s.
                let span = frames[24].sent_us - frames[0].sent_us;
                assert!(span > 150_000, "{codec:?} 25 frames in {span} us");
                stop_and_check(enc, &rx);
                codec
            })
        })
        .collect();
    for r in runs {
        let codec = r.join().expect("encoder test failed");
        println!("{codec:?} ok");
    }
}

#[test]
fn viewer_sink_receives_frames() {
    let Some(tools) = tools() else { return };
    let Some(&codec) = probe_encoders(&tools).first() else {
        println!("note: skipping, no software encoder available");
        return;
    };
    let (enc, rx) = Encoder::start(&tools, &settings(codec)).unwrap();
    let mut viewer = Viewer::start(&tools, codec, "Test screen").unwrap();
    assert!(viewer.is_open());
    let (_, frames) = collect(&rx, 15);
    for f in &frames {
        assert!(viewer.push(f), "viewer closed early");
    }
    // Give the sink time to choke on bad data, if it would.
    std::thread::sleep(Duration::from_millis(300));
    assert!(viewer.is_open());
    viewer.stop();
    assert!(!viewer.is_open());
    assert!(!viewer.push(&frames[0]));
    stop_and_check(enc, &rx);
}

#[test]
fn missing_ffmpeg_is_a_plain_error() {
    let tools = Tools {
        ffmpeg: std::env::temp_dir()
            .join("gloorpflorp-missing")
            .join("ffmpeg.exe"),
        mpv: None,
    };
    let err = Encoder::start(&tools, &settings(VideoCodec::H264))
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("Couldn't start FFmpeg"), "{err}");
    assert!(
        err.contains("Extract the whole GloorpFlorp zip again"),
        "{err}"
    );
    assert!(probe_encoders(&tools).is_empty());
}

/// A stand-in "ffmpeg" that fails like a PC with an old NVIDIA driver.
#[cfg(unix)]
#[test]
fn failing_ffmpeg_gives_advice_and_details() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("gf-video-fake-ffmpeg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("ffmpeg");
    std::fs::write(
        &script,
        "#!/bin/sh\necho '[h264_nvenc @ 0x1] Driver does not support the required nvenc API version.' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tools = Tools {
        ffmpeg: script,
        mpv: None,
    };
    // Another test thread forking while the script was written can make exec
    // fail with "Text file busy" for a moment; retry.
    let mut attempt = 0;
    let (_enc, rx) = loop {
        match Encoder::start(&tools, &settings(VideoCodec::H264)) {
            Ok(r) => break r,
            Err(e) if attempt < 50 && e.to_string().contains("busy") => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("{e}"),
        }
    };
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(EncoderEvent::Stopped { error: Some(e) }) => {
            assert!(e.contains("Update your NVIDIA graphics driver"), "{e}");
            assert!(e.contains("Details from FFmpeg"), "{e}");
            assert!(e.contains("required nvenc API version"), "{e}");
        }
        other => panic!("expected a Stopped error, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
