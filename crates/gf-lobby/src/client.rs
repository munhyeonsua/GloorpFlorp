//! The app's side of the lobby: connects, keeps the latest state for the
//! window, and reconnects (after 1 s, doubling up to 30 s) when the
//! connection drops. Per connection one thread writes (and pings), one reads.
//! Dropping the [`Client`] disconnects.

use std::io::{BufReader, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::{select, tick, Receiver, RecvTimeoutError, Sender};

use crate::{
    encode, read_line, ChatLine, Member, ToClient, ToServer, DEFAULT_PORT, HISTORY, IDLE_TIMEOUT,
    MAX_SERVER_LINE, PING_EVERY, PROTOCOL,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

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
    /// The latest messages, oldest first (at most [`HISTORY`]).
    pub chat: Vec<ChatLine>,
}

pub struct Client {
    tx: Sender<ToServer>,
    state: Arc<Mutex<State>>,
    /// `Invited`, `Replied` and `Error` from the server, for the window.
    pub events: Receiver<ToClient>,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
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
                    s.chat.push(line);
                    if s.chat.len() > HISTORY {
                        s.chat.remove(0);
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
