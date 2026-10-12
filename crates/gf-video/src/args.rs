//! Pure command-line construction for FFmpeg and mpv, plus turning FFmpeg's
//! error output into advice. Everything here is unit tested.

use gf_proto::VideoCodec;

use crate::EncoderSettings;

/// One way of running the capture+encode pipeline. Windows tries the
/// hardware steps in order; dev/test mode uses the test pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// ddagrab's D3D11 frames straight into NVENC; `scale_d3d11` if scaling
    /// (it can't turn). With FFmpeg 8 on NVIDIA cards `scale_d3d11` fails to
    /// create its output frames (error 80070057).
    GpuDirect,
    /// `hwdownload`, then back to the GPU with `hwupload_cuda`; `scale_cuda`
    /// and `transpose_cuda` there, NVENC converts to YUV. Costs a copy each
    /// way, but not the CPU scaling and colour conversion. (D3D11 frames
    /// can't be mapped to CUDA directly: `hwmap` fails with error -40.)
    GpuCuda,
    /// `hwdownload` to system memory, CPU scale/turn/convert, NVENC on
    /// [`NVENC_D3D11_DEVICE`].
    Cpu,
    /// `testsrc2` + software encoder (non-Windows, or `GLOORP_FAKE_VIDEO=1`).
    TestSource,
}

impl Step {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Step::GpuDirect => "GPU (D3D11 -> NVENC)",
            Step::GpuCuda => "GPU (CUDA scaling)",
            Step::Cpu => "CPU fallback (hwdownload)",
            Step::TestSource => "test pattern (software encoder)",
        }
    }
}

/// Whether to scale/turn with `GpuCuda` (instead of `scale_d3d11`, which
/// fails on NVIDIA with FFmpeg 8, or the CPU). Checked on a live desktop on
/// the RTX 5080 test PC: 4K -> 1440p and the turned monitor (scaled and
/// native) come out right with ~0.1-0.3 CPU cores instead of 0.6-2.1 on the
/// CPU way, and a scaled share starts ~0.6 s sooner (no failing
/// `scale_d3d11` step). If CUDA fails, the ladder falls back to the CPU.
pub(crate) const CUDA_TRANSFORM: bool = true;

/// The GPU step that can scale/turn the picture, if any. `turn`: the
/// monitor is turned (only CUDA can do that on the GPU).
pub(crate) fn gpu_transform_step(turn: bool, cuda: bool) -> Option<Step> {
    if cuda {
        Some(Step::GpuCuda)
    } else if turn {
        None
    } else {
        Some(Step::GpuDirect)
    }
}

/// The start-up ladder for one session. `transform`: the picture needs
/// scaling or turning, with `gpu_transform` as the GPU way to try first
/// (`None`: none, or it already failed in this run of the app).
pub(crate) fn ladder(test_source: bool, transform: bool, gpu_transform: Option<Step>) -> Vec<Step> {
    if test_source {
        vec![Step::TestSource]
    } else if !transform {
        vec![Step::GpuDirect, Step::Cpu]
    } else {
        gpu_transform.into_iter().chain([Step::Cpu]).collect()
    }
}

/// The monitor as the encoder sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Screen {
    /// Desktop (upright) size.
    pub width: u32,
    pub height: u32,
    /// `gf_sys::Monitor::rotation`: degrees clockwise.
    pub rotation: u32,
}

impl Screen {
    pub(crate) fn size(self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn sideways(self) -> bool {
        self.rotation % 180 == 90
    }

    pub(crate) fn turned(self) -> bool {
        !self.rotation.is_multiple_of(360)
    }

    /// Whether the captured picture must be scaled or turned to `max_height`.
    pub(crate) fn needs_transform(self, max_height: Option<u32>) -> bool {
        self.turned() || output_size(self.width, self.height, max_height) != self.size()
    }
}

/// Filters that turn ddagrab's (unturned) frames upright, on the CPU or
/// with CUDA; `None` for an upright monitor.
fn turn_filter(rotation: u32, cuda: bool) -> Option<&'static str> {
    Some(match (rotation % 360, cuda) {
        (90, false) => "transpose=clock",
        (180, false) => "hflip,vflip",
        (270, false) => "transpose=cclock",
        (90, true) => "transpose_cuda=dir=clock",
        (180, true) => "transpose_cuda=dir=reversal",
        (270, true) => "transpose_cuda=dir=cclock",
        _ => return None,
    })
}

/// `-vf` for a hardware step: scale to the output size (in capture
/// orientation, so the turn handles the smaller picture), then turn.
/// CPU conversion uses swscale's default BT.601 matrix, which is tagged so
/// players don't guess BT.709 (NVENC's own RGB conversion is tagged alike).
fn capture_filters(step: Step, screen: Screen, out: (u32, u32)) -> Option<String> {
    let (w, h) = if screen.sideways() {
        (out.1, out.0)
    } else {
        out
    };
    let scale = out != screen.size();
    let mut f: Vec<String> = Vec::new();
    match step {
        // scale_d3d11 (FFmpeg 8) only outputs nv12/p010 and needs it spelled out.
        Step::GpuDirect if scale => {
            f.push(format!("scale_d3d11=width={w}:height={h}:format=nv12"));
        }
        Step::GpuCuda => {
            f.push("hwdownload,format=bgra,hwupload_cuda".into());
            if scale {
                f.push(format!("scale_cuda=w={w}:h={h}:interp_algo=bicubic"));
            }
            f.extend(turn_filter(screen.rotation, true).map(String::from));
        }
        Step::Cpu => {
            f.push("hwdownload,format=bgra".into());
            if scale {
                f.push(format!("scale={w}:{h}:flags=bicubic"));
            }
            f.push("format=nv12".into());
            f.extend(turn_filter(screen.rotation, false).map(String::from));
            f.push("setparams=colorspace=bt470bg".into());
        }
        Step::GpuDirect | Step::TestSource => {}
    }
    (!f.is_empty()).then(|| f.join(","))
}

pub(crate) fn nvenc_encoder(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Av1 => "av1_nvenc",
        VideoCodec::Hevc => "hevc_nvenc",
        VideoCodec::H264 => "h264_nvenc",
    }
}

pub(crate) fn software_encoder(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Av1 => "libsvtav1",
        VideoCodec::Hevc => "libx265",
        VideoCodec::H264 => "libx264",
    }
}

/// lavf demuxer name for the raw stream written to the viewer.
pub(crate) fn raw_format(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Av1 => "obu",
        VideoCodec::Hevc => "hevc",
        VideoCodec::H264 => "h264",
    }
}

/// Output size for a monitor: if `max_height` is set and the monitor is
/// taller, scale to that height keeping the aspect ratio (width rounded to an
/// even number); otherwise the native size.
pub fn output_size(monitor_w: u32, monitor_h: u32, max_height: Option<u32>) -> (u32, u32) {
    match max_height {
        Some(max) if max >= 2 && monitor_h > max && monitor_w > 0 => {
            let h = max & !1;
            // Nearest even width to the exact aspect-preserving width, at least 2.
            let num = u64::from(monitor_w) * u64::from(h);
            let den = 2 * u64::from(monitor_h);
            let w = (2 * ((num + den / 2) / den)).clamp(2, u64::from(u32::MAX - 1));
            (w as u32, h)
        }
        _ => (monitor_w, monitor_h),
    }
}

/// VBV buffer size in kbit. Smaller means less burst delay on a link that
/// is barely faster than the bitrate, but starves keyframes and scene cuts
/// (visible quality pulses). 1/8 s lets a keyframe be ~7× an average frame
/// at 60 fps while adding at most ~125 ms of burst delay; never below two
/// frames' worth.
pub(crate) fn bufsize_kbps(bitrate_kbps: u32, fps: u32) -> u32 {
    let b = bitrate_kbps.max(1);
    (b / 8).max(b.saturating_mul(2) / fps.max(1)).max(1)
}

/// The rate ddagrab is asked for. ddagrab waits for its next frame in whole
/// milliseconds on the Windows timer, which only ticks every ~15.6 ms, so
/// asking it for the share's own rate gave uneven frames: at 60 fps a ~31 ms
/// gap every few frames, and 120/144 fps only reached ~72–95 fps. 1001 is the
/// first rate whose frame time (0.999 ms) rounds down to a 0 ms wait, so
/// ddagrab never sleeps on that timer and checks for a new frame about every
/// millisecond instead (it repeats the last frame when the screen is still).
/// That checking costs about one CPU core; [`pace_filter`] then picks the
/// share's own rate out of these frames.
pub(crate) const CAPTURE_POLL_FPS: u32 = 1001;

/// A kept frame must come at least `1 / (fps * PACE_RESYNC_DIVISOR)` s
/// after the one before. Without it, a frame held up until the end of its
/// slot would be followed at once by the first frame of the next slot (two
/// pictures ~1 ms apart). Half a frame time, not a whole one, so the ~1 ms
/// unevenness of ddagrab's frames never pushes a frame out of its slot and
/// lowers the rate.
pub(crate) const PACE_RESYNC_DIVISOR: u32 = 2;

/// Filters that keep one of ddagrab's ~1000 frames a second per 1/`fps` s
/// slot and time-stamp it in slots:
///
/// - `select` keeps the first frame in each new slot (`floor(t*fps)`, `t`
///   is the frame's time in seconds) that also comes at least
///   [`PACE_RESYNC_DIVISOR`] of a frame time after the last kept one;
///   `ld(0)` holds the next slot that may be filled, `ld(1)` the earliest
///   time for the next frame. Everything else is dropped.
/// - `setpts` puts each kept frame at the start of its slot and drops the
///   1001 fps frame rate (`strip_fps`); `settb` then makes the time base
///   1/`fps`, so each frame's time is its slot number. With no frame rate
///   set, FFmpeg gives NVENC the time base's rate, `fps`, and its CBR rate
///   control spreads the bitrate over `fps` frames a second.
///
/// The output stays variable-rate (`-fps_mode vfr`, no `-r`): after a
/// hold-up the stream simply has a gap and carries on with the next new
/// picture. (`-r fps` would make FFmpeg fill the gap with copies of the
/// last frame, sent in a burst before the new one, and drop new frames
/// that land too close to its grid; FFmpeg refuses `-r` with `vfr`.)
///
/// The numbers are whole numbers written by Rust (never locale-dependent);
/// FFmpeg does the division itself, so `1/144` is as exact as a double.
/// The single quotes keep `select`'s commas from FFmpeg's filtergraph parser
/// (the argument goes to FFmpeg as is, there is no shell); that expression
/// has no `:` or `=`, so `select` takes all of it as its `expr` option, and
/// `setpts`'s expression has no `,` `:` or `=`.
pub(crate) fn pace_filter(fps: u32) -> String {
    let fps = fps.max(1);
    let resync = u64::from(fps) * u64::from(PACE_RESYNC_DIVISOR);
    format!(
        "select='if(gte(floor(t*{fps}),ld(0))*gte(t,ld(1)),\
         st(0,floor(t*{fps})+1)+st(1,t+1/{resync}),0)',\
         setpts=floor(T*{fps})/{fps}/TB:strip_fps=1,settb=1/{fps}"
    )
}

/// A D3D11 device on the NVIDIA card for NVENC when its frames come from
/// system memory (the CPU step, the probe). Without one NVENC makes its own
/// CUDA context: ~180-250 MB more video memory per process on an RTX 5080,
/// and it fails where CUDA doesn't work. `vendor_id` picks the NVIDIA card
/// even when it isn't adapter 0 (FFmpeg opens a device on each adapter to
/// check, ~0.1 s more than naming adapter 0).
const NVENC_D3D11_DEVICE: [&str; 2] = ["-init_hw_device", "d3d11va=gf:,vendor_id=0x10de"];

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Full FFmpeg argument list for one ladder step. Every step that captures
/// with ddagrab asks it for [`CAPTURE_POLL_FPS`] and paces the frames with
/// [`pace_filter`] (variable rate, no `-r`); the test pattern already runs
/// at `fps`.
pub(crate) fn encoder_args(step: Step, settings: &EncoderSettings, screen: Screen) -> Vec<String> {
    let fps = settings.fps.max(1);
    let gop = settings.keyframe_interval.max(1);
    let (w, h) = output_size(screen.width, screen.height, settings.max_height);
    let rate = format!("{}k", settings.bitrate_kbps.max(1));
    let bufsize = format!("{}k", bufsize_kbps(settings.bitrate_kbps, fps));

    let mut a = s(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "warning",
        "-nostats",
    ]);
    if step == Step::TestSource {
        a.extend(s(&["-re", "-f", "lavfi", "-i"]));
        a.push(format!("testsrc2=size={w}x{h}:rate={fps}"));
        a.extend(s(&[
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            software_encoder(settings.codec),
        ]));
        match settings.codec {
            VideoCodec::H264 => {
                a.extend(s(&[
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                    "-bf",
                    "0",
                ]));
                a.extend(s(&["-sc_threshold", "0", "-g"]));
                a.push(gop.to_string());
                a.extend(s(&["-b:v", &rate, "-maxrate", &rate, "-bufsize", &bufsize]));
            }
            VideoCodec::Hevc => {
                a.extend(s(&["-preset", "ultrafast", "-tune", "zerolatency", "-g"]));
                a.push(gop.to_string());
                a.extend(s(&["-b:v", &rate, "-maxrate", &rate, "-bufsize", &bufsize]));
                a.push("-x265-params".into());
                a.push(format!(
                    "keyint={gop}:min-keyint={gop}:bframes=0:scenecut=0:log-level=error"
                ));
            }
            VideoCodec::Av1 => {
                // SVT-AV1 low-delay prediction structure, CBR (it rejects -maxrate).
                a.extend(s(&["-preset", "12", "-g"]));
                a.push(gop.to_string());
                a.extend(s(&["-b:v", &rate, "-svtav1-params", "pred-struct=1:rc=2"]));
            }
        }
    } else {
        if step == Step::Cpu {
            a.extend(s(&NVENC_D3D11_DEVICE));
        }
        a.extend(s(&["-f", "lavfi", "-i"]));
        a.push(format!(
            "ddagrab=output_idx={}:framerate={CAPTURE_POLL_FPS}:draw_mouse=1",
            settings.monitor
        ));
        // Pace first, so only the kept frames are copied, scaled or turned.
        let pace = pace_filter(fps);
        a.push("-vf".into());
        a.push(match capture_filters(step, screen, (w, h)) {
            Some(vf) => format!("{pace},{vf}"),
            None => pace,
        });
        // Keep the paced frames as they are: no copies after a hold-up (the
        // FLV default; spelled out so a `-r` added later fails loudly).
        a.extend(s(&["-fps_mode", "vfr"]));
        a.extend(s(&[
            "-c:v",
            nvenc_encoder(settings.codec),
            "-preset",
            "p4",
            "-tune",
            "ull",
        ]));
        a.extend(s(&[
            "-rc", "cbr", "-b:v", &rate, "-maxrate", &rate, "-bufsize", &bufsize,
        ]));
        a.push("-g".into());
        a.push(gop.to_string());
        // -bf is a generic codec option, so it is safe for av1_nvenc too.
        a.extend(s(&["-zerolatency", "1", "-delay", "0", "-bf", "0"]));
        if settings.codec != VideoCodec::Av1 {
            a.extend(s(&["-forced-idr", "1"]));
        }
    }
    a.extend(s(&[
        "-an",
        "-f",
        "flv",
        "-flvflags",
        "no_duration_filesize",
    ]));
    a.extend(s(&["-flush_packets", "1", "pipe:1"]));
    a
}

/// Tiny test encode used by `probe_encoders`, on [`NVENC_D3D11_DEVICE`] (not
/// a CUDA context per probe).
pub(crate) fn probe_args(encoder: &str) -> Vec<String> {
    s(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        NVENC_D3D11_DEVICE[0],
        NVENC_D3D11_DEVICE[1],
        "-f",
        "lavfi",
        "-i",
        "color=black:s=640x360:r=30",
        "-frames:v",
        "3",
        "-pix_fmt",
        "yuv420p",
        "-c:v",
        encoder,
        "-f",
        "null",
        "-",
    ])
}

/// Software encoders (best first) listed in `ffmpeg -encoders` output.
pub(crate) fn parse_software_encoders(listing: &str) -> Vec<VideoCodec> {
    VideoCodec::ALL
        .into_iter()
        .filter(|&c| {
            let name = software_encoder(c);
            listing
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name))
        })
        .collect()
}

/// mpv arguments for the viewer window (reads the raw stream from stdin).
pub(crate) fn mpv_args(codec: VideoCodec, title: &str) -> Vec<String> {
    vec![
        "--no-config".into(),
        // mpv expands `${property}` in the title; `$$` is a plain `$`.
        format!("--title={}", title.replace('$', "$$")),
        "--force-window=immediate".into(),
        "--profile=low-latency".into(),
        "--untimed".into(),
        "--no-cache".into(),
        // Straight to lavf: mpv's own format probes would first wait for
        // 8 KB of input, which a small first keyframe (a dark screen) isn't.
        "--demuxer=lavf".into(),
        format!("--demuxer-lavf-format={}", raw_format(codec)),
        "--demuxer-lavf-probesize=32".into(),
        "--demuxer-lavf-analyzeduration=0".into(),
        // The raw demuxers create their stream in read_header, so skipping
        // stream probing loses nothing (and with nobuffer, probed packets
        // would otherwise be thrown away).
        "--demuxer-lavf-probe-info=nostreams".into(),
        "--demuxer-lavf-o=fflags=+nobuffer".into(),
        "--hwdec=auto-safe".into(),
        // The low-latency profile only makes software decoding single
        // threaded; hardware H.264/HEVC decoding would otherwise hold back
        // 3 frames (4 threads by default).
        "--hwdec-threads=1".into(),
        "--audio=no".into(),
        "--keep-open=no".into(),
        "--idle=no".into(),
        "--autofit=70%x70%".into(),
        "--cursor-autohide=1000".into(),
        "--stop-screensaver=yes".into(),
        "--msg-level=all=warn".into(),
        "-".into(),
    ]
}

/// Dev/test sink used when mpv isn't available (non-Windows only).
pub(crate) fn null_sink_args(codec: VideoCodec) -> Vec<String> {
    s(&[
        "-hide_banner",
        "-v",
        "error",
        "-f",
        raw_format(codec),
        "-i",
        "pipe:0",
        "-f",
        "null",
        "-",
    ])
}

/// Window titles can't contain line breaks; keep them short.
pub(crate) fn clean_title(title: &str) -> String {
    let t: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(120)
        .collect();
    let t = t.trim();
    if t.is_empty() {
        "GloorpFlorp".to_string()
    } else {
        t.to_string()
    }
}

/// Advice for an FFmpeg that refuses an option GloorpFlorp passes.
const TOO_OLD: &str = "The FFmpeg GloorpFlorp found is too old for screen sharing (it \
    needs FFmpeg 8.0 or newer). Extract the whole GloorpFlorp zip again (it comes with \
    the right FFmpeg), then restart GloorpFlorp.";

/// Plain-language advice for FFmpeg's error output.
pub(crate) fn advice(stderr: &str, codec: VideoCodec) -> String {
    let low = stderr.to_lowercase();
    let has = |pats: &[&str]| pats.iter().any(|p| low.contains(&p.to_lowercase()));
    if has(&["output parameters changed"]) {
        "The shared screen changed resolution (this often happens when a game switches \
         to fullscreen), so sharing stopped. Click Start sharing again. Running the game \
         in borderless windowed mode avoids this."
            .to_string()
    } else if has(&[
        // FFmpeg before 8.0 doesn't know setpts's strip_fps (`pace_filter`):
        // "Error applying option 'strip_fps' to filter 'setpts': Option not found".
        "strip_fps",
        // FFmpeg before 5.1 doesn't know -fps_mode.
        "Unrecognized option 'fps_mode'",
    ]) {
        TOO_OLD.to_string()
    } else if has(&[
        "No NVENC capable devices",
        "OpenEncodeSessionEx failed",
        "Driver does not support the required nvenc API version",
        "minimum required Nvidia driver",
        "Cannot load nvEncodeAPI",
        "Cannot load nvcuda",
        "cuInit",
        "CUDA_ERROR",
        "No capable devices found",
        "InitializeEncoder failed",
    ]) {
        format!(
            "The NVIDIA video encoder couldn't start for {}. Update your NVIDIA graphics \
             driver (NVIDIA App or nvidia.com), restart the PC, then try again. Closing \
             other programs that record the screen (OBS, Instant Replay) can also help. \
             If it still fails, pick another Codec under “Your screen” (H.264 works on \
             every NVIDIA card).",
            codec.label()
        )
    } else if has(&[
        "ddagrab",
        "dxgi",
        "duplicat",
        "AcquireNextFrame",
        "Selected output not supported",
    ]) {
        "Windows refused to let GloorpFlorp capture the screen. Check that the right \
         monitor is selected, close any admin (UAC) prompt or protected video player, \
         switch the game to borderless windowed mode, then click Start sharing again."
            .to_string()
    } else if has(&["Option not found", "Unrecognized option"]) {
        // Some other option this FFmpeg doesn't know: most likely an old one
        // too. Checked after the NVENC and capture cases, so when the ladder
        // joins several steps' output, a step's real driver or capture error
        // isn't hidden behind another step's refused option.
        TOO_OLD.to_string()
    } else if has(&["Unknown encoder", "Encoder not found", "No such filter"]) {
        "The FFmpeg next to GloorpFlorp.exe is missing a needed feature. Extract the \
         whole GloorpFlorp zip again, then restart GloorpFlorp."
            .to_string()
    } else {
        "Screen sharing stopped unexpectedly. Click Start sharing to try again; if it \
         keeps happening, update your NVIDIA driver and restart the PC."
            .to_string()
    }
}

/// Advice followed by FFmpeg's last lines.
pub(crate) fn error_message(stderr_lines: &[String], codec: VideoCodec) -> String {
    let details = stderr_lines.join("\n");
    let advice = advice(&details, codec);
    if details.trim().is_empty() {
        advice
    } else {
        format!("{advice}\n\nDetails from FFmpeg:\n{details}")
    }
}

/// Reads `width` / `height` from the `onMetaData` script tag at the start of
/// an FLV stream (FFmpeg writes it before any video). `None` if not found.
pub(crate) fn flv_metadata_size(flv: &[u8]) -> Option<(u32, u32)> {
    // 9-byte header + 4-byte PreviousTagSize0, then the first tag.
    let tag = flv.get(13..)?;
    if *tag.first()? & 0x1f != 18 {
        return None;
    }
    let len = u32::from_be_bytes([0, *tag.get(1)?, *tag.get(2)?, *tag.get(3)?]) as usize;
    let body = tag.get(11..11 + len)?;
    let number = |key: &str| -> Option<u32> {
        let mut pat = Vec::with_capacity(key.len() + 3);
        pat.extend_from_slice(&(key.len() as u16).to_be_bytes());
        pat.extend_from_slice(key.as_bytes());
        pat.push(0); // AMF0 number marker
        let at = body.windows(pat.len()).position(|w| w == pat.as_slice())? + pat.len();
        let v = f64::from_be_bytes(body.get(at..at + 8)?.try_into().ok()?);
        (1.0..=16384.0).contains(&v).then_some(v as u32)
    };
    Some((number("width")?, number("height")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(codec: VideoCodec, max_height: Option<u32>) -> EncoderSettings {
        EncoderSettings {
            codec,
            monitor: 1,
            fps: 60,
            max_height,
            bitrate_kbps: 20_000,
            keyframe_interval: 120,
        }
    }

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    #[test]
    fn output_sizes() {
        assert_eq!(output_size(2560, 1440, None), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(1440)), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(2160)), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(1080)), (1920, 1080));
        assert_eq!(output_size(3440, 1440, Some(1080)), (2580, 1080));
        assert_eq!(output_size(1920, 1080, Some(240)), (426, 240));
        assert_eq!(output_size(1920, 1080, Some(720)), (1280, 720));
        // Odd max height is made even; width always even.
        assert_eq!(output_size(1920, 1080, Some(361)), (640, 360));
        let (w, h) = output_size(1366, 768, Some(500));
        assert_eq!((w % 2, h), (0, 500));
        assert_eq!(output_size(1920, 1080, Some(0)), (1920, 1080));
        assert_eq!(output_size(0, 0, Some(720)), (0, 0));
    }

    #[test]
    fn bufsize() {
        assert_eq!(bufsize_kbps(20_000, 60), 2_500);
        // Low fps: at least two frames' worth.
        assert_eq!(bufsize_kbps(20_000, 10), 4_000);
        assert_eq!(bufsize_kbps(0, 0), 2);
    }

    fn upright(width: u32, height: u32) -> Screen {
        Screen {
            width,
            height,
            rotation: 0,
        }
    }

    #[test]
    fn ladders() {
        assert_eq!(ladder(true, true, None), vec![Step::TestSource]);
        let direct = Some(Step::GpuDirect);
        assert_eq!(ladder(false, false, None), vec![Step::GpuDirect, Step::Cpu]);
        assert_eq!(
            ladder(false, true, direct),
            vec![Step::GpuDirect, Step::Cpu]
        );
        let cuda = Some(Step::GpuCuda);
        assert_eq!(ladder(false, true, cuda), vec![Step::GpuCuda, Step::Cpu]);
        // No GPU way (left), or it failed before: straight to the CPU.
        assert_eq!(ladder(false, true, None), vec![Step::Cpu]);

        // scale_d3d11 can scale but not turn; CUDA does both.
        assert_eq!(gpu_transform_step(false, false), direct);
        assert_eq!(gpu_transform_step(true, false), None);
        assert_eq!(gpu_transform_step(false, true), cuda);
        assert_eq!(gpu_transform_step(true, true), cuda);
    }

    /// What ships: a scaled or turned share tries CUDA, then the CPU, and
    /// never `scale_d3d11` (it fails on NVIDIA with FFmpeg 8); a native
    /// upright share stays GPU-direct.
    #[test]
    fn shipped_ladders() {
        let shipped = |screen: Screen, max_height| {
            let transform = screen.needs_transform(max_height);
            let gpu = gpu_transform_step(screen.turned(), CUDA_TRANSFORM);
            ladder(false, transform, gpu)
        };
        let portrait = Screen {
            width: 2160,
            height: 3840,
            rotation: 90,
        };
        let cuda_then_cpu = vec![Step::GpuCuda, Step::Cpu];
        assert_eq!(shipped(upright(3840, 2160), Some(1440)), cuda_then_cpu);
        assert_eq!(shipped(portrait, Some(1440)), cuda_then_cpu);
        assert_eq!(shipped(portrait, None), cuda_then_cpu);
        assert_eq!(
            shipped(upright(3840, 2160), None),
            vec![Step::GpuDirect, Step::Cpu]
        );
        for (screen, max_height) in [
            (upright(3840, 2160), Some(1440)),
            (upright(2560, 1440), Some(1080)),
            (portrait, Some(1440)),
            (portrait, None),
        ] {
            let st = settings(VideoCodec::Av1, max_height);
            for step in shipped(screen, max_height) {
                let j = joined(&encoder_args(step, &st, screen));
                assert!(!j.contains("scale_d3d11"), "{step:?}: {j}");
            }
        }
    }

    #[test]
    fn gpu_direct_scaled() {
        let a = encoder_args(
            Step::GpuDirect,
            &settings(VideoCodec::Hevc, Some(1080)),
            upright(2560, 1440),
        );
        let j = joined(&a);
        assert!(
            j.contains("settb=1/60,scale_d3d11=width=1920:height=1080:format=nv12 -fps_mode vfr -c:v hevc_nvenc")
        );
        assert!(j.contains("-bf 0 -forced-idr 1 -an"));
    }

    #[test]
    fn transform_needed() {
        assert!(!upright(2560, 1440).needs_transform(None));
        assert!(!upright(2560, 1440).needs_transform(Some(1440)));
        assert!(upright(3840, 2160).needs_transform(Some(1440)));
        for rotation in [90, 180, 270] {
            let s = Screen {
                width: 2160,
                height: 3840,
                rotation,
            };
            assert!(s.needs_transform(None), "{rotation}");
        }
    }

    #[test]
    fn gpu_direct_native() {
        let a = encoder_args(
            Step::GpuDirect,
            &settings(VideoCodec::Av1, None),
            upright(2560, 1440),
        );
        let expected = "-hide_banner -nostdin -loglevel warning -nostats \
            -f lavfi -i ddagrab=output_idx=1:framerate=1001:draw_mouse=1 \
            -vf select='if(gte(floor(t*60),ld(0))*gte(t,ld(1)),st(0,floor(t*60)+1)+st(1,t+1/120),0)',\
            setpts=floor(T*60)/60/TB:strip_fps=1,settb=1/60 -fps_mode vfr \
            -c:v av1_nvenc -preset p4 -tune ull -rc cbr -b:v 20000k -maxrate 20000k \
            -bufsize 2500k -g 120 -zerolatency 1 -delay 0 -bf 0 \
            -an -f flv -flvflags no_duration_filesize -flush_packets 1 pipe:1";
        assert_eq!(joined(&a), expected);
    }

    const CAPTURE_STEPS: [Step; 3] = [Step::GpuDirect, Step::GpuCuda, Step::Cpu];

    #[test]
    fn pace_filter_text() {
        assert_eq!(
            pace_filter(60),
            "select='if(gte(floor(t*60),ld(0))*gte(t,ld(1)),\
             st(0,floor(t*60)+1)+st(1,t+1/120),0)',\
             setpts=floor(T*60)/60/TB:strip_fps=1,settb=1/60"
        );
        assert_eq!(
            pace_filter(144),
            "select='if(gte(floor(t*144),ld(0))*gte(t,ld(1)),\
             st(0,floor(t*144)+1)+st(1,t+1/288),0)',\
             setpts=floor(T*144)/144/TB:strip_fps=1,settb=1/144"
        );
        // fps 0 can't divide by zero.
        assert!(pace_filter(0)
            .ends_with("st(1,t+1/2),0)',setpts=floor(T*1)/1/TB:strip_fps=1,settb=1/1"));
    }

    /// Brackets open and close in order.
    fn balanced(expr: &str) -> bool {
        let mut depth = 0i32;
        for c in expr.chars() {
            depth += match c {
                '(' => 1,
                ')' => -1,
                _ => 0,
            };
            if depth < 0 {
                return false;
            }
        }
        depth == 0
    }

    /// What FFmpeg's parsers need: the filtergraph parser splits filters at
    /// `,` (and `;` `[` `]`) outside single quotes and removes one level of
    /// quotes; then each filter splits its options at `:` and `=`. So the
    /// argument is exactly three filters, `select`'s expression is one
    /// quoted token with none of those inside, `setpts` gets its expression
    /// and `strip_fps`, and there are no spaces or double quotes for
    /// Windows' command line.
    #[test]
    fn pace_filter_survives_ffmpeg_parsing() {
        for fps in [1, 24, 30, 60, 120, 144, 165, 240, 1000] {
            let f = pace_filter(fps);
            for bad in [' ', '"', ';', '[', ']', '\\'] {
                assert!(!f.contains(bad), "{bad:?} in {f}");
            }
            // Split at commas outside quotes, as the filtergraph parser does.
            let mut filters = vec![String::new()];
            let mut quoted = false;
            for c in f.chars() {
                if c == ',' && !quoted {
                    filters.push(String::new());
                    continue;
                }
                if c == '\'' {
                    quoted = !quoted;
                }
                filters.last_mut().unwrap().push(c);
            }
            assert!(!quoted, "{f}");
            assert_eq!(filters.len(), 3, "{f}");
            let expr = filters[0]
                .strip_prefix("select='")
                .and_then(|r| r.strip_suffix('\''))
                .unwrap_or_else(|| panic!("{f}"));
            for bad in ['\'', ':', '='] {
                assert!(!expr.contains(bad), "{bad:?} in {f}");
            }
            assert!(balanced(expr), "{f}");
            assert!(expr.contains(&format!("floor(t*{fps})")), "{f}");
            assert!(expr.contains(&format!("t+1/{}", 2 * fps)), "{f}");
            let setpts: Vec<&str> = filters[1]
                .strip_prefix("setpts=")
                .unwrap_or_else(|| panic!("{f}"))
                .split(':')
                .collect();
            let want = format!("floor(T*{fps})/{fps}/TB");
            assert_eq!(setpts, [want.as_str(), "strip_fps=1"], "{f}");
            assert!(balanced(setpts[0]) && !setpts[0].contains('='), "{f}");
            assert_eq!(filters[2], format!("settb=1/{fps}"), "{f}");
            // Only whole numbers are formatted into it: no locale, no float text.
            assert!(!f.contains('.'), "{f}");
        }
    }

    /// Every ddagrab step asks for 1001 fps, paces to `fps` before any
    /// other filter and keeps the output variable-rate (no `-r`, which
    /// would make FFmpeg copy frames into hold-ups), for all codecs, rates
    /// and monitor shapes.
    #[test]
    fn every_capture_step_is_paced() {
        let portrait = Screen {
            width: 2160,
            height: 3840,
            rotation: 90,
        };
        let screens = [
            (upright(2560, 1440), None),
            (upright(3840, 2160), Some(1440)),
            (portrait, None),
            (portrait, Some(1440)),
        ];
        for step in CAPTURE_STEPS {
            for fps in [30, 60, 120, 144] {
                for codec in VideoCodec::ALL {
                    for (screen, max_height) in screens {
                        let mut st = settings(codec, max_height);
                        st.fps = fps;
                        let a = encoder_args(step, &st, screen);
                        let ctx = format!("{step:?} {fps} {codec:?} {screen:?} {max_height:?}");
                        let at = |x: &str| a.iter().position(|v| v == x);
                        let input = at("-i").map(|i| a[i + 1].as_str());
                        assert_eq!(
                            input,
                            Some("ddagrab=output_idx=1:framerate=1001:draw_mouse=1"),
                            "{ctx}"
                        );
                        let vf = at("-vf").expect("-vf");
                        let filters = &a[vf + 1];
                        let rest = filters
                            .strip_prefix(&pace_filter(fps))
                            .unwrap_or_else(|| panic!("{ctx}: {filters}"));
                        assert!(rest.is_empty() || rest.starts_with(','), "{ctx}: {rest}");
                        assert!(at("-r").is_none() && at("-fpsmax").is_none(), "{ctx}");
                        let mode = at("-fps_mode").expect("-fps_mode");
                        assert_eq!(a[mode + 1], "vfr", "{ctx}");
                        let cv = at("-c:v").expect("-c:v");
                        assert!(vf < mode && mode < cv, "{ctx}");
                    }
                }
            }
        }
    }

    #[test]
    fn test_pattern_is_not_paced() {
        for fps in [30, 60, 144] {
            let mut st = settings(VideoCodec::H264, Some(240));
            st.fps = fps;
            let a = encoder_args(Step::TestSource, &st, upright(1920, 1080));
            let j = joined(&a);
            assert!(
                j.contains(&format!("testsrc2=size=426x240:rate={fps}")),
                "{j}"
            );
            assert!(
                !j.contains("select") && !j.contains(" -r ") && !j.contains("-fps_mode"),
                "{j}"
            );
        }
    }

    /// The pacing rule of [`pace_filter`] in Rust, computed the way FFmpeg
    /// evaluates the expression (doubles, same operations). Returns the
    /// kept frames' times and slots (= their timestamps in 1/`fps`).
    fn paced(times: &[f64], fps: u32) -> Vec<(f64, i64)> {
        let f = f64::from(fps);
        let resync = 1.0 / f64::from(fps * PACE_RESYNC_DIVISOR);
        let (mut next_slot, mut earliest) = (0.0f64, 0.0f64); // ld(0), ld(1)
        let mut kept = Vec::new();
        for &t in times {
            let slot = (t * f).floor();
            if slot >= next_slot && t >= earliest {
                next_slot = slot + 1.0;
                earliest = t + resync;
                kept.push((t, slot as i64));
            }
        }
        kept
    }

    /// ddagrab at 1001 fps hands over a frame about every millisecond (a
    /// little uneven). Pacing fills every slot with one frame, about one
    /// frame time apart; after a 100 ms hold-up the stream has a gap, then
    /// carries on with no burst.
    #[test]
    fn pacing_rule_holds_the_rate() {
        let mut seed = 1u32;
        let mut jitter = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            f64::from(seed >> 8) / f64::from(1u32 << 24) * 0.0006 - 0.0003
        };
        let poll = 1.0 / f64::from(CAPTURE_POLL_FPS);
        let times: Vec<f64> = (0..10_010)
            .map(|i| f64::from(i) * poll + jitter())
            .collect();
        for fps in [30, 60, 120, 144] {
            let (f, period) = (f64::from(fps), 1.0 / f64::from(fps));
            let kept = paced(&times, fps);
            let n = kept.len() as i64;
            let want = i64::from(fps) * 10;
            assert!((n - want).abs() <= 1, "{fps}: {n} frames in 10 s");
            for w in kept.windows(2) {
                assert_eq!(w[1].1, w[0].1 + 1, "{fps}: a slot without a frame");
                let g = w[1].0 - w[0].0;
                assert!((g - period).abs() < 0.0017, "{fps}: gap {g}");
            }

            let stalled: Vec<f64> = times
                .iter()
                .copied()
                .filter(|t| !(5.0..5.1).contains(t))
                .collect();
            let kept = paced(&stalled, fps);
            let after = kept.iter().position(|k| k.0 >= 5.1).unwrap();
            assert!(
                kept[after].0 < 5.1 + 0.0017,
                "{fps}: first frame after the hold-up"
            );
            // The hold-up is a gap of ~0.1 s worth of slots: nothing fills it.
            let gap = (kept[after].1 - kept[after - 1].1) as f64;
            assert!((gap - 0.1 * f).abs() <= 1.5, "{fps}: gap of {gap} slots");
            // No burst: frames at least half a frame time apart, and every
            // slot after the hold-up gets exactly one frame again.
            for w in kept[after..].windows(2) {
                let g = w[1].0 - w[0].0;
                assert!(
                    g >= period / 2.0 - 1e-9,
                    "{fps}: burst after the hold-up ({g})"
                );
                assert_eq!(w[1].1, w[0].1 + 1, "{fps}: slots after the hold-up");
            }
        }
    }

    /// The real FFmpeg with the filters as built here (one argument, no
    /// shell) and the encoder's `-fps_mode vfr`: a 1001 fps source with a
    /// 100 ms hold-up comes out as exactly the frames the Rust model keeps,
    /// time-stamped in 1/fps slots. So the hold-up stays a gap (no copies
    /// of the last frame fill it, no burst after it) and no new frame is
    /// dropped. Skipped without FFmpeg, or with one older than 8.0.
    #[test]
    fn ffmpeg_paces_a_fast_source() {
        let Ok(tools) = crate::tools::locate() else {
            println!("note: skipping, ffmpeg is not available");
            return;
        };
        match crate::tools::can_pace(&tools.ffmpeg) {
            Some(true) => {}
            Some(false) => {
                println!(
                    "note: skipping, {} is older than FFmpeg 8.0 (its setpts has no strip_fps)",
                    tools.ffmpeg.display()
                );
                return;
            }
            None => panic!(
                "couldn't read `{} -h filter=setpts`",
                tools.ffmpeg.display()
            ),
        }
        // testsrc2's frame k is at k/1001 s; FFmpeg's `t` is pts * (1/1001).
        let source: Vec<f64> = (0..3 * CAPTURE_POLL_FPS)
            .map(|k| f64::from(k) * (1.0 / f64::from(CAPTURE_POLL_FPS)))
            .filter(|&t| !(1.0..=1.1).contains(&t))
            .collect();
        for fps in [30, 60, 120, 144] {
            let mut a = s(&["-hide_banner", "-nostdin", "-loglevel", "error"]);
            a.extend(s(&["-f", "lavfi", "-i"]));
            a.push(format!(
                "testsrc2=size=64x36:rate={CAPTURE_POLL_FPS}:duration=3,\
                 select='not(between(t,1,1.1))'"
            ));
            a.extend(["-vf".into(), pace_filter(fps)]);
            a.extend(s(&["-fps_mode", "vfr", "-f", "framecrc", "-"]));
            let run =
                crate::tools::run_short(&tools.ffmpeg, &a, std::time::Duration::from_secs(60));
            let Some((ok, out, err)) = run else {
                panic!("ffmpeg didn't finish: {a:?}");
            };
            assert!(ok, "{fps}: {err}");
            assert!(out.contains(&format!("#tb 0: 1/{fps}")), "{fps}: {out}");
            let pts: Vec<i64> = out
                .lines()
                .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
                .map(|l| l.split(',').nth(2).unwrap().trim().parse().unwrap())
                .collect();
            let want: Vec<i64> = paced(&source, fps).iter().map(|k| k.1).collect();
            assert_eq!(pts, want, "{fps}");
            // One frame per slot except one gap of ~0.1 s for the hold-up.
            let gaps: Vec<i64> = pts
                .windows(2)
                .map(|w| w[1] - w[0])
                .filter(|&d| d != 1)
                .collect();
            assert_eq!(gaps.len(), 1, "{fps}: {gaps:?}");
            let want_gap = 0.1 * f64::from(fps);
            assert!((gaps[0] as f64 - want_gap).abs() <= 1.5, "{fps}: {gaps:?}");
            let want_n = 2.9 * f64::from(fps);
            assert!(
                (pts.len() as f64 - want_n).abs() <= 2.0,
                "{fps}: {}",
                pts.len()
            );
        }
    }

    /// A real FFmpeg refusing a filter option it doesn't know, as one older
    /// than 8.0 refuses `strip_fps`: the advice says it is too old.
    /// Skipped without FFmpeg.
    #[test]
    fn ffmpeg_refusing_a_filter_option_is_too_old() {
        let Ok(tools) = crate::tools::locate() else {
            println!("note: skipping, ffmpeg is not available");
            return;
        };
        let mut a = s(&["-hide_banner", "-nostdin", "-loglevel", "warning"]);
        a.extend(s(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x36:rate=30:duration=1",
        ]));
        a.extend(s(&[
            "-vf",
            "setpts=PTS:gloorpflorp_no_such_option=1,settb=1/30",
        ]));
        a.extend(s(&["-f", "null", "-"]));
        let run = crate::tools::run_short(&tools.ffmpeg, &a, std::time::Duration::from_secs(60));
        let Some((ok, _, err)) = run else {
            panic!("ffmpeg didn't finish: {a:?}");
        };
        assert!(!ok, "{err}");
        let lines: Vec<String> = err.lines().map(String::from).collect();
        let m = error_message(&lines, VideoCodec::H264);
        assert!(
            m.starts_with("The FFmpeg GloorpFlorp found is too old"),
            "{m}"
        );
    }

    #[test]
    fn cuda_and_cpu_steps() {
        let st = settings(VideoCodec::Hevc, Some(1080));
        let j = joined(&encoder_args(Step::GpuCuda, &st, upright(2560, 1440)));
        assert!(j.contains(
            "settb=1/60,hwdownload,format=bgra,hwupload_cuda,scale_cuda=w=1920:h=1080:interp_algo=bicubic \
             -fps_mode vfr -c:v hevc_nvenc"
        ));
        assert!(j.contains("-bf 0 -forced-idr 1 -an"));
        let st = settings(VideoCodec::H264, Some(1080));
        let j = joined(&encoder_args(Step::Cpu, &st, upright(2560, 1440)));
        assert!(j.contains(
            "settb=1/60,hwdownload,format=bgra,scale=1920:1080:flags=bicubic,format=nv12,\
             setparams=colorspace=bt470bg -fps_mode vfr -c:v h264_nvenc"
        ));
        let j = joined(&encoder_args(
            Step::Cpu,
            &settings(VideoCodec::H264, None),
            upright(2560, 1440),
        ));
        assert!(j.contains(
            "settb=1/60,hwdownload,format=bgra,format=nv12,setparams=colorspace=bt470bg -fps_mode vfr -c:v"
        ));
        // Only the CPU step hands NVENC frames in system memory, so only it
        // brings a D3D11 device (else NVENC makes its own CUDA context).
        assert!(
            j.contains("-nostats -init_hw_device d3d11va=gf:,vendor_id=0x10de -f lavfi -i ddagrab")
        );
        for step in [Step::GpuDirect, Step::GpuCuda] {
            let j = joined(&encoder_args(step, &st, upright(3840, 2160)));
            assert!(!j.contains("-init_hw_device"), "{step:?}: {j}");
        }
    }

    /// The portrait monitor of the PC this was tested on: Windows turns it
    /// 90° and ddagrab hands over 3840×2160 frames that must be turned
    /// clockwise. The size is still worked out from the upright 2160×3840.
    #[test]
    fn turned_monitors() {
        let portrait = |rotation| Screen {
            width: 2160,
            height: 3840,
            rotation,
        };
        let st = settings(VideoCodec::H264, Some(1440));
        assert_eq!(output_size(2160, 3840, Some(1440)), (810, 1440));
        let j = joined(&encoder_args(Step::Cpu, &st, portrait(90)));
        assert!(j.contains(
            "settb=1/60,hwdownload,format=bgra,scale=1440:810:flags=bicubic,format=nv12,\
             transpose=clock,setparams=colorspace=bt470bg -fps_mode vfr -c:v"
        ));
        let j = joined(&encoder_args(Step::GpuCuda, &st, portrait(90)));
        assert!(j.contains(
            "settb=1/60,hwdownload,format=bgra,hwupload_cuda,\
             scale_cuda=w=1440:h=810:interp_algo=bicubic,transpose_cuda=dir=clock -fps_mode vfr -c:v"
        ));
        let j = joined(&encoder_args(Step::GpuCuda, &st, portrait(270)));
        assert!(j.contains(
            "scale_cuda=w=1440:h=810:interp_algo=bicubic,transpose_cuda=dir=cclock -fps_mode vfr -c:v"
        ));
        let j = joined(&encoder_args(Step::Cpu, &st, portrait(270)));
        assert!(j.contains("format=nv12,transpose=cclock,setparams"));

        // Native size: only the turn.
        let st = settings(VideoCodec::Av1, None);
        let j = joined(&encoder_args(Step::Cpu, &st, portrait(90)));
        assert!(
            j.contains("settb=1/60,hwdownload,format=bgra,format=nv12,transpose=clock,setparams")
        );
        let j = joined(&encoder_args(Step::GpuCuda, &st, portrait(90)));
        assert!(j.contains(
            "settb=1/60,hwdownload,format=bgra,hwupload_cuda,transpose_cuda=dir=clock -fps_mode vfr -c:v"
        ));

        // Upside down: same size, flipped both ways.
        let upside_down = Screen {
            width: 2560,
            height: 1440,
            rotation: 180,
        };
        let st = settings(VideoCodec::Av1, Some(1080));
        let j = joined(&encoder_args(Step::Cpu, &st, upside_down));
        assert!(j.contains("scale=1920:1080:flags=bicubic,format=nv12,hflip,vflip,setparams"));
        let j = joined(&encoder_args(Step::GpuCuda, &st, upside_down));
        assert!(j.contains(
            "scale_cuda=w=1920:h=1080:interp_algo=bicubic,transpose_cuda=dir=reversal -fps_mode vfr -c:v"
        ));
    }

    #[test]
    fn every_step_ends_with_flv_on_stdout() {
        for codec in VideoCodec::ALL {
            for step in [Step::GpuDirect, Step::GpuCuda, Step::Cpu, Step::TestSource] {
                let a = encoder_args(step, &settings(codec, Some(1080)), upright(2560, 1440));
                assert!(joined(&a).ends_with(
                    "-an -f flv -flvflags no_duration_filesize -flush_packets 1 pipe:1"
                ));
                assert_eq!(
                    &a[..5],
                    [
                        "-hide_banner",
                        "-nostdin",
                        "-loglevel",
                        "warning",
                        "-nostats"
                    ]
                );
            }
        }
    }

    #[test]
    fn test_source_args() {
        let mut st = settings(VideoCodec::H264, Some(240));
        st.fps = 30;
        st.keyframe_interval = 15;
        let j = joined(&encoder_args(Step::TestSource, &st, upright(1920, 1080)));
        assert!(j.contains("-re -f lavfi -i testsrc2=size=426x240:rate=30 -pix_fmt yuv420p"));
        assert!(j.contains("-c:v libx264 -preset ultrafast -tune zerolatency -bf 0"));
        assert!(j.contains("-g 15"));
        st.codec = VideoCodec::Hevc;
        let j = joined(&encoder_args(Step::TestSource, &st, upright(1920, 1080)));
        assert!(j.contains("-c:v libx265"));
        assert!(j.contains("keyint=15:min-keyint=15:bframes=0"));
        st.codec = VideoCodec::Av1;
        let j = joined(&encoder_args(Step::TestSource, &st, upright(1920, 1080)));
        assert!(j.contains("-c:v libsvtav1 -preset 12 -g 15"));
        assert!(j.contains("pred-struct=1"));
        assert!(!j.contains("ddagrab"));
    }

    #[test]
    fn mpv_and_sink_args() {
        let a = mpv_args(VideoCodec::Av1, "Alex's screen");
        assert!(a.contains(&"--title=Alex's screen".to_string()));
        assert!(a.contains(&"--demuxer-lavf-format=obu".to_string()));
        assert_eq!(a.last().map(String::as_str), Some("-"));
        assert!(mpv_args(VideoCodec::Hevc, "x").contains(&"--demuxer-lavf-format=hevc".to_string()));
        // Straight to the raw demuxer, and no frames held by decoder threads.
        let lavf = a.iter().position(|x| x == "--demuxer=lavf").unwrap();
        assert!(a[lavf + 1].starts_with("--demuxer-lavf-format="));
        assert!(a.contains(&"--hwdec-threads=1".to_string()));
        // `${...}` in a friend's name is not expanded by mpv.
        let a = mpv_args(VideoCodec::Av1, "${path} costs $5");
        assert!(a.contains(&"--title=$${path} costs $$5".to_string()));
        assert_eq!(
            joined(&null_sink_args(VideoCodec::H264)),
            "-hide_banner -v error -f h264 -i pipe:0 -f null -"
        );
        assert_eq!(clean_title("a\nb\tc"), "a b c");
        assert_eq!(clean_title("  "), "GloorpFlorp");
    }

    #[test]
    fn encoder_listing() {
        let listing = " V....D libx264              libx264 H.264\n \
                       V..... libsvtav1            SVT-AV1\n \
                       V....D h264_nvenc           NVIDIA\n";
        assert_eq!(
            parse_software_encoders(listing),
            vec![VideoCodec::Av1, VideoCodec::H264]
        );
        assert!(parse_software_encoders("").is_empty());
        let probe = joined(&probe_args("av1_nvenc"));
        assert!(probe.contains("-c:v av1_nvenc -f null -"));
        assert!(probe.contains("-init_hw_device d3d11va=gf:,vendor_id=0x10de -f lavfi"));
    }

    #[test]
    fn advice_for_known_errors() {
        let nv = advice(
            "[h264_nvenc @ 0x1] OpenEncodeSessionEx failed: unsupported device (2): (no details)",
            VideoCodec::H264,
        );
        assert!(nv.contains("Update your NVIDIA graphics driver"));
        assert!(nv.contains("pick another Codec under “Your screen”") && !nv.contains('\n'));
        let nv = advice(
            "Driver does not support the required nvenc API version. Required: 13.0 Found: 12.1\n\
             The minimum required Nvidia driver for nvenc is 570.0 or newer",
            VideoCodec::Av1,
        );
        assert!(nv.contains("NVIDIA") && nv.contains("AV1"));
        let nv = advice(
            "[AVHWDeviceContext @ 0x3] cuInit(0) failed -> CUDA_ERROR_NO_DEVICE: no CUDA-capable device is detected",
            VideoCodec::Hevc,
        );
        assert!(nv.contains("Update your NVIDIA graphics driver"));
        let dd = advice(
            "[Parsed_ddagrab_0 @ 0x2] Desktop duplication access denied",
            VideoCodec::H264,
        );
        assert!(dd.contains("Windows refused"));
        let dd = advice("AcquireNextFrame failed: 887a0026", VideoCodec::H264);
        assert!(dd.contains("borderless windowed"));
        let rc = advice(
            "[Parsed_ddagrab_0 @ 0x2] Output parameters changed!",
            VideoCodec::H264,
        );
        assert!(rc.contains("changed resolution"));
        assert!(advice("No such filter: 'scale_d3d11'", VideoCodec::H264).contains("zip again"));
        // What FFmpeg 6.1 to 7.1 print for the pacing filter (fftools'
        // filter_opt_apply at n6.1.1), and an FFmpeg older than -fps_mode.
        for old in [
            "[fc#-1 @ 0x1] Error applying option 'strip_fps' to filter 'setpts': Option not found\n\
             Error opening output file -.\n\
             Error opening output files: Option not found",
            "Unrecognized option 'fps_mode'.\nError splitting the argument list: Option not found",
        ] {
            let a = advice(old, VideoCodec::Av1);
            assert!(a.contains("needs FFmpeg 8.0 or newer"), "{a}");
            assert!(a.contains("Extract the whole GloorpFlorp zip again"), "{a}");
        }
        assert!(advice("something odd", VideoCodec::H264).contains("stopped unexpectedly"));
        let m = error_message(&["line 1".into(), "line 2".into()], VideoCodec::H264);
        assert!(m.ends_with("Details from FFmpeg:\nline 1\nline 2"));
        assert!(!error_message(&[], VideoCodec::H264).contains("Details"));
    }

    #[test]
    fn metadata_size() {
        let mut body = vec![0x02, 0x00, 0x0a];
        body.extend_from_slice(b"onMetaData");
        body.extend_from_slice(&[0x08, 0, 0, 0, 2]);
        for (k, v) in [("width", 1920.0f64), ("height", 1080.0)] {
            body.extend_from_slice(&(k.len() as u16).to_be_bytes());
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(&v.to_be_bytes());
        }
        body.extend_from_slice(&[0, 0, 9]);
        let mut flv = b"FLV\x01\x01\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        let len = body.len() as u32;
        flv.push(18);
        flv.extend_from_slice(&len.to_be_bytes()[1..]);
        flv.extend_from_slice(&[0; 7]);
        flv.extend_from_slice(&body);
        assert_eq!(flv_metadata_size(&flv), Some((1920, 1080)));
        assert_eq!(flv_metadata_size(&flv[..flv.len() - 5]), None);
        assert_eq!(flv_metadata_size(b"FLV"), None);
        let mut video_first = flv.clone();
        video_first[13] = 9;
        assert_eq!(flv_metadata_size(&video_first), None);
    }
}
