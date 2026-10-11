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
    /// `hwdownload` to system memory, CPU scale/turn/convert, NVENC.
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

/// Whether to scale/turn with `GpuCuda`. It works on the RTX 5080 test PC
/// with D3D11 test frames (same picture as the CPU way, ~1/15 of its CPU
/// time), but hasn't run on a live desktop yet, so it stays off until then.
pub(crate) const CUDA_TRANSFORM: bool = false;

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

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Full FFmpeg argument list for one ladder step.
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
        a.extend(s(&["-f", "lavfi", "-i"]));
        a.push(format!(
            "ddagrab=output_idx={}:framerate={fps}:draw_mouse=1",
            settings.monitor
        ));
        if let Some(vf) = capture_filters(step, screen, (w, h)) {
            a.push("-vf".into());
            a.push(vf);
        }
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

/// Tiny test encode used by `probe_encoders`.
pub(crate) fn probe_args(encoder: &str) -> Vec<String> {
    s(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
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
        "No NVENC capable devices",
        "OpenEncodeSessionEx failed",
        "Driver does not support the required nvenc API version",
        "minimum required Nvidia driver",
        "Cannot load nvEncodeAPI",
        "Cannot load nvcuda",
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

    #[test]
    fn gpu_direct_scaled() {
        let a = encoder_args(
            Step::GpuDirect,
            &settings(VideoCodec::Hevc, Some(1080)),
            upright(2560, 1440),
        );
        let j = joined(&a);
        assert!(j.contains("-vf scale_d3d11=width=1920:height=1080:format=nv12 -c:v hevc_nvenc"));
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
            -f lavfi -i ddagrab=output_idx=1:framerate=60:draw_mouse=1 \
            -c:v av1_nvenc -preset p4 -tune ull -rc cbr -b:v 20000k -maxrate 20000k \
            -bufsize 2500k -g 120 -zerolatency 1 -delay 0 -bf 0 \
            -an -f flv -flvflags no_duration_filesize -flush_packets 1 pipe:1";
        assert_eq!(joined(&a), expected);
    }

    #[test]
    fn cuda_and_cpu_steps() {
        let st = settings(VideoCodec::Hevc, Some(1080));
        let j = joined(&encoder_args(Step::GpuCuda, &st, upright(2560, 1440)));
        assert!(j.contains(
            "-vf hwdownload,format=bgra,hwupload_cuda,scale_cuda=w=1920:h=1080:interp_algo=bicubic \
             -c:v hevc_nvenc"
        ));
        assert!(j.contains("-bf 0 -forced-idr 1 -an"));
        let st = settings(VideoCodec::H264, Some(1080));
        let j = joined(&encoder_args(Step::Cpu, &st, upright(2560, 1440)));
        assert!(j.contains(
            "-vf hwdownload,format=bgra,scale=1920:1080:flags=bicubic,format=nv12,\
             setparams=colorspace=bt470bg -c:v h264_nvenc"
        ));
        let j = joined(&encoder_args(
            Step::Cpu,
            &settings(VideoCodec::H264, None),
            upright(2560, 1440),
        ));
        assert!(
            j.contains("-vf hwdownload,format=bgra,format=nv12,setparams=colorspace=bt470bg -c:v")
        );
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
            "-vf hwdownload,format=bgra,scale=1440:810:flags=bicubic,format=nv12,\
             transpose=clock,setparams=colorspace=bt470bg -c:v"
        ));
        let j = joined(&encoder_args(Step::GpuCuda, &st, portrait(90)));
        assert!(j.contains(
            "-vf hwdownload,format=bgra,hwupload_cuda,\
             scale_cuda=w=1440:h=810:interp_algo=bicubic,transpose_cuda=dir=clock -c:v"
        ));
        let j = joined(&encoder_args(Step::GpuCuda, &st, portrait(270)));
        assert!(j.contains(
            "scale_cuda=w=1440:h=810:interp_algo=bicubic,transpose_cuda=dir=cclock -c:v"
        ));
        let j = joined(&encoder_args(Step::Cpu, &st, portrait(270)));
        assert!(j.contains("format=nv12,transpose=cclock,setparams"));

        // Native size: only the turn.
        let st = settings(VideoCodec::Av1, None);
        let j = joined(&encoder_args(Step::Cpu, &st, portrait(90)));
        assert!(j.contains("-vf hwdownload,format=bgra,format=nv12,transpose=clock,setparams"));
        let j = joined(&encoder_args(Step::GpuCuda, &st, portrait(90)));
        assert!(
            j.contains("-vf hwdownload,format=bgra,hwupload_cuda,transpose_cuda=dir=clock -c:v")
        );

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
            "scale_cuda=w=1920:h=1080:interp_algo=bicubic,transpose_cuda=dir=reversal -c:v"
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
        assert!(joined(&probe_args("av1_nvenc")).contains("-c:v av1_nvenc -f null -"));
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
