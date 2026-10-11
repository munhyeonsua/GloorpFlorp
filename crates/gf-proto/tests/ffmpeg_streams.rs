//! Demuxes real FLV streams from the local FFmpeg (software encoders) and
//! checks that the viewer byte stream decodes cleanly, also when started from
//! a later keyframe, and that a frame is decoded without waiting for the next.
//!
//! Every test returns early with a note when FFmpeg or the encoder is missing.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gf_proto::bitstream::{is_decodable_start, viewer_prologue, write_viewer_frame, DemuxedFrame, FlvDemuxer};
use gf_proto::VideoCodec;
use rand::{rngs::StdRng, Rng, SeedableRng};

const FRAMES: usize = 40;
const GOP: usize = 10;

fn raw_format(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "obu",
    }
}

/// FLV bytes from FFmpeg, or `None` (with a printed note) if unavailable.
fn encode_flv(encoder: &str) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30"])
        .args(["-frames:v", &FRAMES.to_string(), "-c:v", encoder, "-g", &GOP.to_string(), "-bf", "0"])
        .args(["-f", "flv", "-flvflags", "no_duration_filesize", "pipe:1"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => Some(o.stdout),
        Ok(o) => {
            println!("note: skipping, ffmpeg can't encode with {encoder}: {}", String::from_utf8_lossy(&o.stderr));
            None
        }
        Err(e) => {
            println!("note: skipping, ffmpeg is not available ({e})");
            None
        }
    }
}

fn demux_chunked(flv: &[u8], seed: Option<u64>) -> Vec<DemuxedFrame> {
    let mut d = FlvDemuxer::new();
    let mut frames = Vec::new();
    match seed {
        None => frames.extend(d.push(flv).expect("demux")),
        Some(seed) => {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut rest = flv;
            while !rest.is_empty() {
                let n = rng.random_range(1..=4096).min(rest.len());
                frames.extend(d.push(&rest[..n]).expect("demux"));
                rest = &rest[n..];
            }
        }
    }
    frames
}

fn viewer_stream(codec: VideoCodec, frames: &[DemuxedFrame]) -> Vec<u8> {
    let mut out = viewer_prologue(codec);
    for f in frames {
        write_viewer_frame(codec, &f.data, &mut out);
    }
    out
}

/// Decodes `stream` with FFmpeg; returns (decoded frame count, stderr).
fn decode(codec: VideoCodec, stream: Vec<u8>) -> (usize, String) {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-f", raw_format(codec), "-i", "pipe:0", "-f", "framemd5", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&stream);
    });
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    writer.join().unwrap();
    let err = err_reader.join().unwrap();
    child.wait().unwrap();
    let count = out.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).count();
    (count, err)
}

/// Checks everything about one codec's real stream; returns the frames.
fn check_stream(encoder: &str, codec: VideoCodec) -> Option<Vec<DemuxedFrame>> {
    let flv = encode_flv(encoder)?;
    let frames = demux_chunked(&flv, None);
    for seed in 0..5 {
        assert_eq!(demux_chunked(&flv, Some(seed)), frames, "chunked demux differs (seed {seed})");
    }
    assert_eq!(frames.len(), FRAMES, "{encoder}: frame count");
    let keys: Vec<usize> = frames.iter().enumerate().filter(|(_, f)| f.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keys, (0..FRAMES).step_by(GOP).collect::<Vec<_>>(), "{encoder}: keyframe positions");
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.codec, codec);
        assert_eq!(is_decodable_start(codec, &f.data), f.keyframe, "{encoder}: is_decodable_start on frame {i}");
    }

    let (count, err) = decode(codec, viewer_stream(codec, &frames));
    assert!(err.is_empty(), "{encoder}: decoder errors: {err}");
    assert_eq!(count, FRAMES, "{encoder}: decoded frame count");

    let (count, err) = decode(codec, viewer_stream(codec, &frames[keys[1]..]));
    assert!(err.is_empty(), "{encoder}: decoder errors starting at 2nd keyframe: {err}");
    assert_eq!(count, FRAMES - keys[1], "{encoder}: decoded frames starting at 2nd keyframe");
    Some(frames)
}

/// Writes the prologue and the first keyframe (plus its trailing delimiter)
/// to a low-delay FFmpeg without closing stdin, and returns how long it took
/// for the decoded frame to appear, or `None` if it didn't within `timeout`.
fn first_frame_latency(codec: VideoCodec, frames: &[DemuxedFrame], timeout: Duration) -> Option<Duration> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-probesize", "32", "-analyzeduration", "0"])
        .args(["-fflags", "nobuffer", "-flags", "low_delay", "-f", raw_format(codec), "-i", "pipe:0"])
        .args(["-f", "framemd5", "-flush_packets", "1", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn ffmpeg");
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if !line.starts_with('#') && tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stdin = child.stdin.take().unwrap();
    let mut bytes = viewer_prologue(codec);
    write_viewer_frame(codec, &frames[0].data, &mut bytes);
    let start = Instant::now();
    stdin.write_all(&bytes).unwrap();
    stdin.flush().unwrap();
    let result = rx.recv_timeout(timeout).ok().map(|_| start.elapsed());
    let _ = child.kill();
    let _ = child.wait();
    drop(stdin);
    result
}

fn check_latency(encoder: &str, codec: VideoCodec) {
    let Some(flv) = encode_flv(encoder) else { return };
    let frames = demux_chunked(&flv, None);
    match first_frame_latency(codec, &frames, Duration::from_secs(2)) {
        Some(d) => println!("{encoder}: first frame decoded after {d:?}"),
        None => panic!("{encoder}: the first frame was not decoded within 2 s without more input"),
    }
}

#[test]
fn h264_stream() {
    check_stream("libx264", VideoCodec::H264);
}

#[test]
fn hevc_stream() {
    // x265 only puts VPS/SPS/PPS in the first keyframe (repeat-headers=0),
    // so later keyframes rely on parameter set injection.
    check_stream("libx265", VideoCodec::Hevc);
}

#[test]
fn av1_stream() {
    check_stream("libsvtav1", VideoCodec::Av1);
}

#[test]
fn h264_low_latency() {
    check_latency("libx264", VideoCodec::H264);
}

#[test]
fn hevc_low_latency() {
    check_latency("libx265", VideoCodec::Hevc);
}

#[test]
fn av1_low_latency() {
    check_latency("libsvtav1", VideoCodec::Av1);
}

#[test]
#[ignore]
fn dump_streams() {
    let dir = std::env::var("DUMP_DIR").unwrap();
    for (enc, codec) in [("libx264", VideoCodec::H264), ("libx265", VideoCodec::Hevc), ("libsvtav1", VideoCodec::Av1)] {
        let flv = encode_flv(enc).unwrap();
        let frames = demux_chunked(&flv, None);
        let mut first = viewer_prologue(codec);
        write_viewer_frame(codec, &frames[0].data, &mut first);
        std::fs::write(format!("{dir}/{enc}.first"), first).unwrap();
        std::fs::write(format!("{dir}/{enc}.all"), viewer_stream(codec, &frames)).unwrap();
    }
}
