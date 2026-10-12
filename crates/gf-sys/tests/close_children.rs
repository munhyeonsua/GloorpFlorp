//! `close_children` ends every child for the whole process, so it gets a
//! test program of its own.

use std::process::Command;
use std::time::{Duration, Instant};

fn long_running() -> Command {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("ping");
        c.args(["-n", "30", "127.0.0.1"]);
        c
    } else {
        let mut c = Command::new("sleep");
        c.arg("30");
        c
    };
    c.stdout(std::process::Stdio::null());
    c
}

#[test]
fn close_children_ends_them_and_refuses_new_ones() {
    let mut child = gf_sys::spawn_bound(&mut long_running()).unwrap();
    gf_sys::close_children();
    if cfg!(windows) {
        let t = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(t.elapsed() < Duration::from_secs(1), "child still running");
            std::thread::sleep(Duration::from_millis(10));
        }
    } else {
        // Only Windows has the job object; elsewhere just clean up.
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(gf_sys::spawn_bound(&mut long_running()).is_err());
}
