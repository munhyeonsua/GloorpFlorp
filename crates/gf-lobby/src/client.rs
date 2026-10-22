//! The app's side of the lobby: connects, keeps the latest state for the
//! window, and reconnects (after 1 s, doubling up to 30 s) when the
//! connection drops. Per connection one thread writes (and pings), one reads.
//! Dropping the [`Client`] disconnects. [`upload`] and [`download`] move one
//! file each over a connection of their own.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::{select, tick, Receiver, RecvTimeoutError, Sender};

use crate::{
    encode, forget_image, read_line, ChatLine, Member, ToClient, ToServer, DEFAULT_PORT, HISTORY,
    IDLE_TIMEOUT, IMAGE_HISTORY, MAX_SERVER_LINE, PING_EVERY, PROTOCOL,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);
/// The server answers a file request at once; a lobby server from before
/// files ignores it and never answers.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);
const OLD_SERVER: &str = "The lobby server can't take files yet. Update it: in its folder run \
                          docker compose pull, then docker compose up -d.";

/// Called from the client's threads whenever the state changed or an event
/// arrived.
pub type ChangeCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, Default)]
pub struct State {
    /// Welcomed by the server.
    pub connected: bool,
    /// Why we aren't connected, in plain words.
    pub problem: Option<String>,
    /// My member id.
    pub me: String,
    pub members: Vec<Member>,
    /// The latest messages, oldest first (at most [`HISTORY`]); shared, so
    /// cloning the state stays cheap with pictures in it.
    pub chat: Vec<Arc<ChatLine>>,
}

pub struct Client {
    tx: Sender<ToServer>,
    state: Arc<Mutex<State>>,
    /// Invites, answers and problems from the server, for the window.
    pub events: Receiver<ToClient>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Client {
    /// Starts connecting to `addr` ("host:port", or "host" for port 47900)
    /// as `name`. Never blocks.
    pub fn start(addr: &str, name: &str, on_change: ChangeCallback) -> Client {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (events_tx, events) = crossbeam_channel::unbounded();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let (addr, name) = (addr.to_string(), name.to_string());
        let started = std::thread::Builder::new()
            .name("lobby".into())
            .spawn(move || run(&addr, &name, rx, &shared, &events_tx, &on_change));
        if let Err(e) = started {
            lock(&state).problem = Some(format!("Couldn't start the lobby connection ({e})."));
        }
        Client { tx, state, events }
    }

    /// Dropped while not connected.
    pub fn send(&self, msg: ToServer) {
        let _ = self.tx.send(msg);
    }

    pub fn state(&self) -> State {
        lock(&self.state).clone()
    }
}

fn run(
    addr: &str,
    name: &str,
    rx: Receiver<ToServer>,
    shared: &Arc<Mutex<State>>,
    events: &Sender<ToClient>,
    on_change: &ChangeCallback,
) {
    let mut wait = Duration::from_secs(1);
    loop {
        let problem = match connect(addr) {
            Ok(stream) => match session(stream, name, &rx, shared, events, on_change) {
                Some(why) => why,
                None => return,
            },
            Err(why) => why,
        };
        {
            let mut s = lock(shared);
            if std::mem::take(&mut s.connected) {
                wait = Duration::from_secs(1);
            }
            // Who is around isn't known any more (nobody can be invited);
            // the chat stays readable.
            s.members.clear();
            log::info!("lobby: {problem}");
            s.problem = Some(problem);
        }
        on_change();
        // Messages sent meanwhile are dropped.
        let until = Instant::now() + wait;
        loop {
            match rx.recv_deadline(until) {
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        wait = (wait * 2).min(MAX_RETRY_WAIT);
    }
}

fn connect(addr: &str) -> Result<TcpStream, String> {
    let with_port = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    };
    let addrs = with_port.to_socket_addrs().map_err(|e| {
        format!("Can't find the lobby server \"{addr}\" ({e}). Check the address, and that Tailscale is on.")
    })?;
    let mut last = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    let why = last.map_or("no address".to_string(), |e| e.to_string());
    Err(format!(
        "Can't reach the lobby server at {addr} ({why}). Check that it is running, and that Tailscale is on."
    ))
}

/// One connection, until it drops (`Some(why)`) or the client is dropped
/// (`None`).
fn session(
    mut stream: TcpStream,
    name: &str,
    rx: &Receiver<ToServer>,
    shared: &Arc<Mutex<State>>,
    events: &Sender<ToClient>,
    on_change: &ChangeCallback,
) -> Option<String> {
    let reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => return Some(format!("Couldn't use the lobby connection ({e}).")),
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IDLE_TIMEOUT));
    let (done_tx, done) = crossbeam_channel::bounded(1);
    {
        let (shared, events, on_change) = (shared.clone(), events.clone(), on_change.clone());
        let started = std::thread::Builder::new()
            .name("lobby-read".into())
            .spawn(move || {
                let _ = done_tx.send(read_loop(reader, &shared, &events, &on_change));
            });
        if let Err(e) = started {
            return Some(format!("Couldn't start the lobby connection ({e})."));
        }
    }
    let hello = ToServer::Hello {
        v: PROTOCOL,
        name: name.to_string(),
    };
    let ping = tick(PING_EVERY);
    let mut next = Some(hello);
    let why = loop {
        if let Some(msg) = next.take() {
            if let Err(e) = stream.write_all(&encode(&msg)) {
                break format!("Lost the connection to the lobby ({e}).");
            }
        }
        select! {
            recv(rx) -> msg => match msg {
                Ok(msg) => next = Some(msg),
                Err(_) => {
                    let _ = stream.shutdown(Shutdown::Both);
                    return None;
                }
            },
            recv(ping) -> _ => next = Some(ToServer::Ping),
            recv(done) -> why => break why.unwrap_or_default(),
        }
    };
    // Ends the reader too.
    let _ = stream.shutdown(Shutdown::Both);
    Some(why)
}

/// Applies what the server sends until the connection ends; returns why.
fn read_loop(
    stream: TcpStream,
    shared: &Mutex<State>,
    events: &Sender<ToClient>,
    on_change: &ChangeCallback,
) -> String {
    let mut r = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        let line = match read_line(&mut r, &mut buf, MAX_SERVER_LINE) {
            Ok(Some(line)) => line,
            Ok(None) => return "The lobby server closed the connection.".into(),
            Err(e) => return format!("Lost the connection to the lobby ({e})."),
        };
        let msg = match serde_json::from_slice::<ToClient>(line) {
            Ok(msg) => msg,
            Err(e) => {
                log::debug!("lobby: ignoring a line that doesn't parse ({e})");
                continue;
            }
        };
        {
            let mut s = lock(shared);
            match msg {
                ToClient::Welcome { you, .. } => {
                    log::info!("lobby: connected as {you}");
                    *s = State {
                        connected: true,
                        me: you,
                        ..State::default()
                    };
                }
                ToClient::Members { list } => s.members = list,
                ToClient::Chat(line) => {
                    s.chat.push(Arc::new(line));
                    if s.chat.len() > HISTORY {
                        s.chat.remove(0);
                    }
                    // Like the server: pictures only on the newest lines.
                    let n = s.chat.iter().filter(|l| l.image.is_some()).count();
                    for l in s
                        .chat
                        .iter_mut()
                        .filter(|l| l.image.is_some())
                        .take(n.saturating_sub(IMAGE_HISTORY))
                    {
                        forget_image(Arc::make_mut(l));
                    }
                }
                // Turned away (e.g. wrong version); the server closes now.
                ToClient::Error { text } if !s.connected => return text,
                ToClient::Pong => continue,
                other => {
                    let _ = events.send(other);
                }
            }
        }
        on_change();
    }
}

/// An upload or download, shared with the window.
#[derive(Debug, Default)]
pub struct Transfer {
    /// Bytes moved so far.
    pub done: AtomicU64,
    /// The file's size (0 until known).
    pub total: AtomicU64,
    /// Set to stop it.
    pub cancel: AtomicBool,
    /// Why it is waiting to try again, in plain words (cleared once moving).
    pub problem: Mutex<Option<String>>,
}

impl Transfer {
    pub fn problem(&self) -> Option<String> {
        lock(&self.problem).clone()
    }
}

/// How one try ended badly.
enum Failure {
    /// Trying again won't help; said in plain words.
    Refused(String),
    /// The connection broke: try again from where it stopped.
    Lost(String),
}

/// Uploads the file at `path` to the lobby at `addr` as file `id` (see
/// [`new_file_id`](crate::new_file_id)) for `who` (the name used in the
/// lobby). After a dropped connection it tries again, resuming, until the
/// file is stored (it then shows in the chat), cancelled, or refused.
pub fn upload(
    addr: &str,
    who: &str,
    id: &str,
    path: &Path,
    t: &Transfer,
    on_change: &ChangeCallback,
) -> Result<(), String> {
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let unreadable = |e: io::Error| Failure::Refused(format!("Can't read {name} ({e})."));
    keep_trying(t, on_change, || {
        let size = fs::metadata(path).map_err(unreadable)?.len();
        t.total.store(size, Ordering::Relaxed);
        let first = ToServer::Upload {
            id: id.into(),
            name: name.clone(),
            size,
            who: who.into(),
        };
        let (mut s, mut r) = open(addr, &first)?;
        let have = match answer(&mut r, true)? {
            ToClient::Ready { have } => have.min(size),
            _ => return Err(Failure::Lost(UNEXPECTED.into())),
        };
        let mut f = File::open(path).map_err(unreadable)?;
        f.seek(SeekFrom::Start(have)).map_err(unreadable)?;
        t.done.store(have, Ordering::Relaxed);
        match copy(&mut f, &mut s, size - have, t, on_change) {
            Ok(()) => {}
            Err(Stop::Read(e)) => return Err(unreadable(e)),
            // The server may have said why it stopped taking it.
            Err(Stop::Write(e)) => {
                let _ = r.get_ref().set_read_timeout(Some(Duration::from_secs(2)));
                answer(&mut r, false)?;
                return Err(lost(e));
            }
            Err(Stop::Cancelled) => return Ok(()),
        }
        let _ = r.get_ref().set_read_timeout(Some(IDLE_TIMEOUT));
        match answer(&mut r, false)? {
            ToClient::Stored => Ok(()),
            _ => Err(Failure::Lost(UNEXPECTED.into())),
        }
    })
}

/// Downloads file `id` from the lobby at `addr` into `part`, appending to
/// what an earlier try left there. After a dropped connection it tries again,
/// resuming, until the file is complete, cancelled, or refused.
pub fn download(
    addr: &str,
    id: &str,
    part: &Path,
    t: &Transfer,
    on_change: &ChangeCallback,
) -> Result<(), String> {
    let disk = |e: io::Error| Failure::Refused(format!("Can't save to {} ({e}).", part.display()));
    keep_trying(t, on_change, || {
        let have = fs::metadata(part).map_or(0, |m| m.len());
        let first = ToServer::Download {
            id: id.into(),
            from: have,
        };
        let (_s, mut r) = open(addr, &first)?;
        let size = match answer(&mut r, true)? {
            ToClient::Sending { size } => size,
            _ => return Err(Failure::Lost(UNEXPECTED.into())),
        };
        t.total.store(size, Ordering::Relaxed);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(part)
            .map_err(disk)?;
        if have > size {
            // Not the file the part came from: start over.
            file.set_len(0).map_err(disk)?;
            return Err(Failure::Lost(
                "The file changed on the lobby server.".into(),
            ));
        }
        t.done.store(have, Ordering::Relaxed);
        let _ = r.get_ref().set_read_timeout(Some(IDLE_TIMEOUT));
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let result = copy(&mut r, &mut out, size - have, t, on_change);
        match result.and_then(|()| out.flush().map_err(Stop::Write)) {
            Ok(()) | Err(Stop::Cancelled) => Ok(()),
            Err(Stop::Read(e)) => Err(lost(e)),
            Err(Stop::Write(e)) => Err(disk(e)),
        }
    })
}

const UNEXPECTED: &str = "The lobby server sent something unexpected.";

fn lost(e: io::Error) -> Failure {
    Failure::Lost(format!("Lost the connection to the lobby ({e})."))
}

/// A file connection with its first line sent, and a reader for answers.
fn open(addr: &str, first: &ToServer) -> Result<(TcpStream, BufReader<TcpStream>), Failure> {
    let mut s = connect(addr).map_err(Failure::Lost)?;
    let r = s.try_clone().map_err(lost)?;
    let _ = s.set_write_timeout(Some(IDLE_TIMEOUT));
    let _ = r.set_read_timeout(Some(ANSWER_TIMEOUT));
    s.write_all(&encode(first)).map_err(lost)?;
    Ok((s, BufReader::new(r)))
}

/// The server's next answer; an `Error` is a refusal. `first`: no answer in
/// time means a server too old for files.
fn answer(r: &mut BufReader<TcpStream>, first: bool) -> Result<ToClient, Failure> {
    let mut buf = Vec::new();
    match read_line(r, &mut buf, MAX_SERVER_LINE) {
        Ok(Some(line)) => match serde_json::from_slice(line) {
            Ok(ToClient::Error { text }) => Err(Failure::Refused(text)),
            Ok(msg) => Ok(msg),
            Err(_) => Err(Failure::Lost(UNEXPECTED.into())),
        },
        Ok(None) => Err(Failure::Lost(
            "The lobby server closed the connection.".into(),
        )),
        Err(e)
            if first
                && matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
        {
            Err(Failure::Refused(OLD_SERVER.into()))
        }
        Err(e) => Err(lost(e)),
    }
}

enum Stop {
    Read(io::Error),
    Write(io::Error),
    Cancelled,
}

/// Copies `len` bytes, counting them in `t.done`.
fn copy(
    from: &mut impl Read,
    to: &mut impl Write,
    len: u64,
    t: &Transfer,
    on_change: &ChangeCallback,
) -> Result<(), Stop> {
    let mut buf = vec![0; 1 << 20];
    let mut left = len;
    let mut shown = Instant::now();
    while left > 0 {
        if t.cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = from.read(&mut buf[..want]).map_err(Stop::Read)?;
        if n == 0 {
            return Err(Stop::Read(io::ErrorKind::UnexpectedEof.into()));
        }
        to.write_all(&buf[..n]).map_err(Stop::Write)?;
        left -= n as u64;
        t.done.fetch_add(n as u64, Ordering::Relaxed);
        if shown.elapsed() > Duration::from_millis(250) {
            lock(&t.problem).take();
            on_change();
            shown = Instant::now();
        }
    }
    Ok(())
}

/// Runs `once` until it succeeds, is refused or cancelled; waits 1 s after a
/// dropped connection, doubling up to 30 s while nothing gets through.
fn keep_trying(
    t: &Transfer,
    on_change: &ChangeCallback,
    mut once: impl FnMut() -> Result<(), Failure>,
) -> Result<(), String> {
    let mut wait = Duration::from_secs(1);
    loop {
        let before = t.done.load(Ordering::Relaxed);
        let result = once();
        on_change();
        if t.cancel.load(Ordering::Relaxed) {
            return Err("Cancelled.".into());
        }
        let text = match result {
            Ok(()) => return Ok(()),
            Err(Failure::Refused(text)) => return Err(text),
            Err(Failure::Lost(text)) => text,
        };
        log::info!("lobby file transfer: {text}");
        *lock(&t.problem) = Some(format!("{text} Trying again…"));
        on_change();
        if t.done.load(Ordering::Relaxed) > before {
            wait = Duration::from_secs(1);
        }
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if t.cancel.load(Ordering::Relaxed) {
                return Err("Cancelled.".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        wait = (wait * 2).min(MAX_RETRY_WAIT);
    }
}
