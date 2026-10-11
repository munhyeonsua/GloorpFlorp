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

pub(crate) fn locate_in(exe_dir: Option<&Path>, path_var: Option<OsString>) -> anyhow::Result<Tools> {
    let ffmpeg = find_program("ffmpeg", exe_dir, path_var.clone()).ok_or_else(|| {
        anyhow::anyhow!(
            "Couldn't find FFmpeg next to GloorpFlorp.exe. Extract the whole zip again, \
             then restart GloorpFlorp."
        )
    })?;
    let mpv = find_program("mpv", exe_dir, path_var);
    log::info!("FFmpeg: {}", ffmpeg.display());
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
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("couldn't run {}: {e}", program.display());
            return None;
        }
    };
    gf_sys::bind_child_to_app_lifetime(&child);
    let read_all = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r {
                let _ = r.take(1 << 20).read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = read_all(child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let err = read_all(child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
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

#[cfg(test)]
mod tests {
    use super::*;

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

        touch(&app.join("ffmpeg").join(exe_name("ffmpeg")));
        let t = locate_in(Some(&app), Some(path)).unwrap();
        assert_eq!(t.ffmpeg, app.join("ffmpeg").join(exe_name("ffmpeg")));
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
