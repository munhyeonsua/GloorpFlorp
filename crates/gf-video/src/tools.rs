//! Finding the bundled programs and probing which encoders work.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gf_proto::VideoCodec;

use crate::{args, proc, Tools};

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

fn exe_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Looks in `exe_dir/<name>/<name>.exe`, `exe_dir/<name>.exe`, then PATH.
pub(crate) fn find_program(
    name: &str,
    exe_dir: Option<&Path>,
    path_var: Option<OsString>,
) -> Option<PathBuf> {
    let file = exe_name(name);
    let bundled = exe_dir
        .into_iter()
        .flat_map(|d| [d.join(name).join(&file), d.join(&file)]);
    let on_path = path_var
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(&file));
    bundled.chain(on_path).find(|p| p.is_file())
}

pub(crate) fn locate_in(
    exe_dir: Option<&Path>,
    path_var: Option<OsString>,
) -> anyhow::Result<Tools> {
    let ffmpeg = find_program("ffmpeg", exe_dir, path_var.clone()).ok_or_else(|| {
        anyhow::anyhow!(
            "Couldn't find FFmpeg next to GloorpFlorp.exe. Extract the whole zip again, \
             then restart GloorpFlorp."
        )
    })?;
    let mpv = find_program("mpv", exe_dir, path_var);
    if exe_dir.is_some_and(|d| ffmpeg.starts_with(d)) {
        log::info!("FFmpeg: {}", ffmpeg.display());
    } else {
        // Not the bundled build: one older than 8.0 makes screen sharing
        // fail (see `Tools::locate`).
        log::info!(
            "FFmpeg: {} (from PATH; sharing the screen needs FFmpeg 8.0 or newer)",
            ffmpeg.display()
        );
    }
    match &mpv {
        Some(p) => log::info!("mpv: {}", p.display()),
        None => log::warn!("mpv not found; the viewer window won't be available"),
    }
    Ok(Tools { ffmpeg, mpv })
}

pub(crate) fn locate() -> anyhow::Result<Tools> {
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(Path::parent);
    locate_in(exe_dir, std::env::var_os("PATH"))
}

/// Runs a short command with a hidden console. Returns (success, stdout,
/// stderr) or `None` if it couldn't start or didn't finish within `timeout`
/// (it is killed then).
pub(crate) fn run_short(
    program: &Path,
    args: &[String],
    timeout: Duration,
) -> Option<(bool, String, String)> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    gf_sys::hide_console_window(&mut cmd);
    let mut child = match gf_sys::spawn_bound(&mut cmd) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("couldn't run {}: {e}", program.display());
            return None;
        }
    };
    let read_all = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r {
                let _ = r.take(1 << 20).read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = read_all(
        child
            .stdout
            .take()
            .map(|r| Box::new(r) as Box<dyn Read + Send>),
    );
    let err = read_all(
        child
            .stderr
            .take()
            .map(|r| Box::new(r) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                proc::kill(&mut child);
                break None;
            }
        }
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    status.map(|s| (s.success(), out, err))
}

/// Whether to use the test pattern + software encoders instead of
/// ddagrab + NVENC: always off Windows, or with `GLOORP_FAKE_VIDEO=1`.
pub(crate) fn use_test_source() -> bool {
    !cfg!(windows) || std::env::var("GLOORP_FAKE_VIDEO").is_ok_and(|v| v == "1")
}

pub(crate) fn probe_encoders(tools: &Tools) -> Vec<VideoCodec> {
    if use_test_source() {
        let listing = run_short(
            &tools.ffmpeg,
            &["-hide_banner".into(), "-encoders".into()],
            PROBE_TIMEOUT,
        );
        let found = match listing {
            Some((true, out, _)) => args::parse_software_encoders(&out),
            _ => Vec::new(),
        };
        log::info!("software encoders available (test mode): {found:?}");
        return found;
    }
    let probes: Vec<_> = VideoCodec::ALL
        .into_iter()
        .map(|codec| {
            let ffmpeg = tools.ffmpeg.clone();
            std::thread::spawn(move || {
                let enc = args::nvenc_encoder(codec);
                let started = Instant::now();
                let ok = match run_short(&ffmpeg, &args::probe_args(enc), PROBE_TIMEOUT) {
                    Some((true, _, _)) => true,
                    Some((false, _, err)) => {
                        let last = err.lines().rev().take(3).collect::<Vec<_>>();
                        log::info!("{enc} doesn't work here: {}", last.join(" | "));
                        false
                    }
                    None => {
                        log::warn!("{enc} test encode didn't finish in time");
                        false
                    }
                };
                log::debug!("{enc} probe took {:?}", started.elapsed());
                ok.then_some(codec)
            })
        })
        .collect();
    let found: Vec<_> = probes
        .into_iter()
        .filter_map(|p| p.join().ok().flatten())
        .collect();
    log::info!("working NVENC encoders: {found:?}");
    found
}

/// Whether `ffmpeg -h filter=<name>` output (stdout and stderr together)
/// lists `option`. FFmpeg prints one option per line, its name first
/// (`   strip_fps   <boolean>  ..FV....... Unset framerate metadata`;
/// codec options get a `-` in front).
#[cfg(test)]
pub(crate) fn help_lists_option(help: &str, option: &str) -> bool {
    help.lines()
        .filter_map(|l| l.split_whitespace().next())
        .any(|name| name.trim_start_matches('-') == option)
}

/// Test support: whether `ffmpeg` can run [`args::pace_filter`]. Its
/// `setpts` option `strip_fps` came with FFmpeg 8.0; older builds (Ubuntu
/// 24.04 ships 6.1) refuse the filter. `None` if the help couldn't be read
/// (spawn failure or timeout), so callers don't mistake that for an old
/// FFmpeg. Asked once per program.
#[cfg(test)]
pub(crate) fn can_pace(ffmpeg: &Path) -> Option<bool> {
    use std::sync::Mutex;
    static KNOWN: Mutex<Vec<(PathBuf, Option<bool>)>> = Mutex::new(Vec::new());
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, ok)) = known.iter().find(|(p, _)| p == ffmpeg) {
        return ok;
    }
    let a = ["-hide_banner", "-h", "filter=setpts"].map(String::from);
    let ok = run_short(ffmpeg, &a, PROBE_TIMEOUT)
        .map(|(_, out, err)| help_lists_option(&format!("{out}\n{err}"), "strip_fps"));
    known.push((ffmpeg.to_path_buf(), ok));
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ffmpeg -hide_banner -h filter=setpts` of FFmpeg 6.1 (built from
    /// libavfilter/setpts.c and fftools/opt_common.c at tag n6.1.1).
    const SETPTS_HELP_6_1: &str = "Filter setpts\n  Set PTS for the output video frame.\n    \
        Inputs:\n       #0: default (video)\n    Outputs:\n       #0: default (video)\n\
        setpts AVOptions:\n   \
        expr              <string>     ..FV.....T. Expression determining the frame timestamp (default \"PTS\")\n\n";

    /// The same from the bundled FFmpeg 8 (BtbN N-127271).
    const SETPTS_HELP_8: &str = "Filter setpts\r\n  Set PTS for the output video frame.\r\n    \
        Inputs:\r\n       #0: default (video)\r\n    Outputs:\r\n       #0: default (video)\r\n\
        setpts AVOptions:\r\n   \
        expr              <string>     ..FV.....T. Expression determining the frame timestamp (default \"PTS\")\r\n   \
        strip_fps         <boolean>    ..FV....... Unset framerate metadata (default false)\r\n\r\n\r\n\
        Exiting with exit code 0\r\n";

    #[test]
    fn pacing_support_from_filter_help() {
        assert!(!help_lists_option(SETPTS_HELP_6_1, "strip_fps"));
        assert!(help_lists_option(SETPTS_HELP_6_1, "expr"));
        assert!(help_lists_option(SETPTS_HELP_8, "strip_fps"));
        // Only option names count, not words in the descriptions.
        assert!(!help_lists_option(SETPTS_HELP_8, "framerate"));
        assert!(!help_lists_option(
            "Unknown filter 'setpts'.\n",
            "strip_fps"
        ));
        assert!(!help_lists_option("", "strip_fps"));
        assert!(help_lists_option(
            "  -forced-idr       <boolean>    E..V.......",
            "forced-idr"
        ));
    }

    #[test]
    fn missing_ffmpeg_is_unknown_not_old() {
        assert_eq!(can_pace(Path::new("gloorpflorp-no-such-ffmpeg")), None);
        assert_eq!(can_pace(Path::new("gloorpflorp-no-such-ffmpeg")), None);
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gf-video-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn bundled_before_path() {
        let root = scratch("locate");
        let app = root.join("app");
        let bin = root.join("bin");
        touch(&bin.join(exe_name("ffmpeg")));
        touch(&bin.join(exe_name("mpv")));
        let path = std::env::join_paths([bin.clone()]).unwrap();

        let t = locate_in(Some(&app), Some(path.clone())).unwrap();
        assert_eq!(t.ffmpeg, bin.join(exe_name("ffmpeg")));
        assert_eq!(t.mpv, Some(bin.join(exe_name("mpv"))));

        touch(&app.join(exe_name("ffmpeg")));
        touch(&app.join("mpv").join(exe_name("mpv")));
        let t = locate_in(Some(&app), Some(path.clone())).unwrap();
        assert_eq!(t.ffmpeg, app.join(exe_name("ffmpeg")));
        assert_eq!(t.mpv, Some(app.join("mpv").join(exe_name("mpv"))));

        // The `ffmpeg/` subfolder wins over a flat exe (on Linux the two
        // names would clash, so only the subfolder is tested there).
        let app2 = root.join("app2");
        touch(&app2.join("ffmpeg").join(exe_name("ffmpeg")));
        if cfg!(windows) {
            touch(&app2.join(exe_name("ffmpeg")));
        }
        let t = locate_in(Some(&app2), Some(path)).unwrap();
        assert_eq!(t.ffmpeg, app2.join("ffmpeg").join(exe_name("ffmpeg")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_ffmpeg_is_a_plain_error() {
        let root = scratch("missing");
        let err = locate_in(Some(&root), None).unwrap_err().to_string();
        assert!(err.contains("Couldn't find FFmpeg"), "{err}");
        assert!(err.contains("Extract the whole zip again"));
        // mpv missing alone is fine.
        touch(&root.join(exe_name("ffmpeg")));
        let t = locate_in(Some(&root), None).unwrap();
        assert!(t.mpv.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_short_handles_missing_program() {
        assert!(run_short(Path::new("gloorpflorp-no-such-program"), &[], PROBE_TIMEOUT).is_none());
    }
}
