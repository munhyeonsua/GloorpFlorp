//! Child-process helpers shared by the encoder, the viewer and the probe.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Lines of stderr kept for error messages.
const TAIL_LINES: usize = 30;
/// Longer lines are cut (a misbehaving child can't make us buffer forever).
const MAX_LINE: usize = 400;

/// Collects the last lines a child prints on stderr, on its own thread.
pub(crate) struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl StderrTail {
    pub(crate) fn spawn(name: &str, stderr: Option<impl Read + Send + 'static>) -> StderrTail {
        let lines = Arc::new(Mutex::new(VecDeque::new()));
        let thread = stderr.and_then(|stderr| {
            let lines = lines.clone();
            let tag = name.to_string();
            std::thread::Builder::new()
                .name(format!("{name}-stderr"))
                .spawn(move || collect_lines(stderr, &tag, &lines))
                .ok()
        });
        StderrTail { lines, thread }
    }

    /// Waits (briefly) for the child's stderr to close, then returns its last lines.
    pub(crate) fn finish(mut self, wait: Duration) -> Vec<String> {
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + wait;
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
        self.snapshot()
    }

    pub(crate) fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }
}

fn collect_lines(mut r: impl Read, tag: &str, lines: &Mutex<VecDeque<String>>) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let push = |line: &mut Vec<u8>| {
        let text = String::from_utf8_lossy(line).trim_end().to_string();
        line.clear();
        if text.is_empty() {
            return;
        }
        log::debug!("{tag}: {text}");
        if let Ok(mut l) = lines.lock() {
            if l.len() == TAIL_LINES {
                l.pop_front();
            }
            l.push_back(text);
        }
    };
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                push(&mut line);
            } else if line.len() < MAX_LINE {
                line.push(b);
            }
        }
    }
    push(&mut line);
}

/// Kills the child and reaps it.
pub(crate) fn kill(child: &mut Child) -> Option<ExitStatus> {
    let _ = child.kill();
    child.wait().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_last_lines() {
        let mut text = String::new();
        for i in 0..50 {
            text.push_str(&format!("line {i}\r\n"));
        }
        text.push_str(&"x".repeat(5000));
        let tail = StderrTail::spawn("test", Some(std::io::Cursor::new(text.into_bytes())));
        let lines = tail.finish(Duration::from_secs(5));
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], "line 21");
        assert_eq!(lines[28], "line 49");
        assert_eq!(lines[29].len(), MAX_LINE);
    }
}
