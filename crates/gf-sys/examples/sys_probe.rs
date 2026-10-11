//! Prints what gf-sys sees on this PC and records 3 s of game audio.
//! Run on Windows: `cargo run -p gf-sys --example sys_probe [pid]`
//! (no pid = everything except this program).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gf_sys::{GameAudioSource, LoopbackCapture};

fn main() {
    println!("computer name: {}", gf_sys::computer_name());
    println!("tailscale ipv4: {:?}", gf_sys::tailscale_ipv4());
    for m in gf_sys::list_monitors() {
        println!("monitor: {m:?}");
    }
    for a in gf_sys::list_audio_apps() {
        println!("app: {:>6} {:<30} {}", a.pid, a.exe, a.title);
    }
    for vk in [0x05, 0x06, 0x56, 0x14, 0xC0, 0xE2, 0xE5] {
        println!("key {vk:#04x}: {}", gf_sys::keys::name(vk));
    }

    let mut cmd = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" });
    if cfg!(windows) {
        cmd.args(["/c", "exit"]);
    }
    gf_sys::hide_console_window(&mut cmd);
    if let Ok(mut child) = cmd.spawn() {
        gf_sys::bind_child_to_app_lifetime(&child);
        let _ = child.wait();
        println!("child process bound to job object and exited");
    }

    let source = match std::env::args().nth(1).and_then(|a| a.parse().ok()) {
        Some(pid) => GameAudioSource::Process { pid },
        None => GameAudioSource::AllExceptThisApp,
    };
    let frames = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(Mutex::new(0f32));
    let (f, p) = (frames.clone(), peak.clone());
    let started = LoopbackCapture::start(
        source,
        Box::new(move |s: &[f32]| {
            f.fetch_add(s.len() / 2, Ordering::Relaxed);
            let m = s.iter().fold(0f32, |m, x| m.max(x.abs()));
            if let Ok(mut p) = p.try_lock() {
                *p = p.max(m);
            }
        }),
    );
    match started {
        Ok(cap) => {
            std::thread::sleep(Duration::from_secs(3));
            println!(
                "loopback {source:?}: {} frames in 3 s (expect ~144000 while something plays), peak {:.3}, error {:?}",
                frames.load(Ordering::Relaxed),
                *peak.lock().unwrap(),
                cap.error()
            );
        }
        Err(e) => println!("loopback {source:?} failed: {e}"),
    }
}
