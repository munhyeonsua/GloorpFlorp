//! The lobby server (see the crate docs). One thread accepts connections, one
//! reads each connection, and one hub thread owns all state and writes to
//! every connection (with a write timeout, so a stuck client can't hold up
//! the others for long). On Linux another thread asks tailscaled every few
//! seconds which devices are online.
//!
//! Files in the data folder: `members.json` (everyone who ever joined) and
//! `chat.jsonl` (every chat message, one JSON object per line).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use crate::{
    encode, read_line, ChatLine, Member, Presence, ToClient, ToServer, HISTORY, IDLE_TIMEOUT,
    MAX_CLIENT_LINE, MAX_NAME, MAX_TEXT, PROTOCOL,
};

pub struct Config {
    pub data_dir: PathBuf,
    /// tailscaled's LocalAPI socket. `None` (or a socket that isn't there):
    /// presence comes from lobby connections only.
    pub tailscale_socket: Option<PathBuf>,
}

/// Threads are cheap but not free; the lobby is for a handful of friends.
const MAX_CONNECTIONS: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const TAILNET_POLL: Duration = Duration::from_secs(5);
/// Chat messages and invites: bursts of up to this many, then one a second.
const BURST: f32 = 5.0;
/// Room codes are 13 characters; leave room for spaces.
const MAX_CODE: usize = 40;

enum Ev {
    Open {
        id: u64,
        ip: IpAddr,
        stream: TcpStream,
    },
    Line {
        id: u64,
        msg: ToServer,
    },
    Closed {
        id: u64,
    },
    /// Tailscale address -> online.
    #[cfg_attr(not(unix), allow(dead_code))]
    Tailnet(HashMap<IpAddr, bool>),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Known {
    ip: IpAddr,
    name: String,
}

struct Conn {
    stream: TcpStream,
    ip: IpAddr,
    /// Member id, once it said `Hello`.
    member: Option<String>,
    tokens: f32,
    refilled: Instant,
}

struct Hub {
    dir: PathBuf,
    /// By member id.
    members: BTreeMap<String, Known>,
    history: VecDeque<ChatLine>,
    conns: HashMap<u64, Conn>,
    tailnet: HashMap<IpAddr, bool>,
    /// The member list everyone has.
    sent: Vec<Member>,
}

/// Runs the lobby on `listener` until the process ends. Fails at the start if
/// the data folder can't be read, or later if the hub thread died.
pub fn serve(listener: TcpListener, cfg: Config) -> io::Result<()> {
    let mut hub = Hub::load(&cfg.data_dir)?;
    log::info!(
        "lobby on {} with {} member(s) and {} message(s) of history in {}",
        listener.local_addr()?,
        hub.members.len(),
        hub.history.len(),
        cfg.data_dir.display()
    );
    let (tx, rx) = crossbeam_channel::unbounded();
    #[cfg(unix)]
    if let Some(path) = cfg.tailscale_socket {
        let tx = tx.clone();
        spawn("tailnet", move || poll_tailnet(path, tx))?;
    }
    #[cfg(not(unix))]
    if cfg.tailscale_socket.is_some() {
        log::info!("asking tailscaled needs Linux; showing only who is in the lobby");
    }
    spawn("hub", move || {
        for ev in rx {
            hub.handle(ev);
        }
    })?;
    let active = Arc::new(AtomicUsize::new(0));
    let mut next_id = 0;
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept failed: {e}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let Ok(peer) = stream.peer_addr() else {
            continue;
        };
        let ip = peer.ip().to_canonical();
        if !allowed(ip) {
            log::info!("refused {ip}: not a Tailscale address");
            continue;
        }
        if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            log::warn!("refused {ip}: already {MAX_CONNECTIONS} connections");
            continue;
        }
        let Ok(writer) = stream.try_clone() else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
        let _ = writer.set_write_timeout(Some(WRITE_TIMEOUT));
        next_id += 1;
        let id = next_id;
        if tx
            .send(Ev::Open {
                id,
                ip,
                stream: writer,
            })
            .is_err()
        {
            break;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let (conn_tx, conn_active) = (tx.clone(), active.clone());
        let started = spawn("lobby-conn", move || {
            read_conn(id, stream, &conn_tx);
            conn_active.fetch_sub(1, Ordering::Relaxed);
            let _ = conn_tx.send(Ev::Closed { id });
        });
        if let Err(e) = started {
            log::warn!("couldn't start a connection thread: {e}");
            active.fetch_sub(1, Ordering::Relaxed);
            let _ = tx.send(Ev::Closed { id });
        }
    }
    Err(io::Error::other("the lobby's hub thread stopped"))
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(drop)
}

/// Tailscale addresses (100.64.0.0/10, fd7a:115c:a1e0::/48), and the server's
/// own machine for development. Anything else could be the LAN, which
/// Tailscale didn't authenticate.
fn allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || (v4.octets()[0] == 100 && v4.octets()[1] & 0xC0 == 64)
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

fn read_conn(id: u64, stream: TcpStream, tx: &Sender<Ev>) {
    let mut r = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        match read_line(&mut r, &mut buf, MAX_CLIENT_LINE) {
            Ok(Some(line)) => match serde_json::from_slice::<ToServer>(line) {
                Ok(msg) => {
                    if tx.send(Ev::Line { id, msg }).is_err() {
                        return;
                    }
                }
                Err(e) => log::debug!("connection {id}: ignoring a line that doesn't parse ({e})"),
            },
            Ok(None) => return,
            Err(e) => {
                log::debug!("connection {id} ends: {e}");
                return;
            }
        }
    }
}

/// One line of at most `max` characters, control characters made spaces.
fn clean(s: &str, max: usize) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect();
    s.trim().to_string()
}

impl Hub {
    fn load(dir: &Path) -> io::Result<Hub> {
        fs::create_dir_all(dir)?;
        let path = dir.join("members.json");
        let members = match fs::read(&path) {
            // Refuse to start rather than overwrite a damaged file.
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is damaged ({e}); fix or delete it", path.display()),
                )
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        let mut history = VecDeque::new();
        match fs::File::open(dir.join("chat.jsonl")) {
            // shortcut: reads (and keeps) the whole file at start; fine for
            // years of chat between friends, rotate it if it ever gets big.
            Ok(f) => {
                for line in BufReader::new(f).split(b'\n') {
                    // A line cut short by a crash is skipped.
                    if let Ok(c) = serde_json::from_slice::<ChatLine>(&line?) {
                        history.push_back(c);
                        if history.len() > HISTORY {
                            history.pop_front();
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Hub {
            dir: dir.to_path_buf(),
            members,
            history,
            conns: HashMap::new(),
            tailnet: HashMap::new(),
            sent: Vec::new(),
        })
    }

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Open { id, ip, stream } => {
                let conn = Conn {
                    stream,
                    ip,
                    member: None,
                    tokens: BURST,
                    refilled: Instant::now(),
                };
                self.conns.insert(id, conn);
            }
            Ev::Line { id, msg } => self.message(id, msg),
            Ev::Closed { id } => {
                self.conns.remove(&id);
            }
            Ev::Tailnet(map) => self.tailnet = map,
        }
        self.members_changed();
    }

    fn message(&mut self, id: u64, msg: ToServer) {
        let Some(conn) = self.conns.get(&id) else {
            return;
        };
        match (msg, conn.member.clone()) {
            (ToServer::Hello { v, name }, _) => self.hello(id, v, &name),
            (ToServer::Ping, _) => self.send(id, &ToClient::Pong),
            // Everything else needs a Hello first.
            (_, None) => {}
            (ToServer::Chat { text }, Some(me)) => {
                let text = clean(&text, MAX_TEXT);
                if text.is_empty() || !self.allow(id) {
                    return;
                }
                let line = ChatLine {
                    name: self.members[&me].name.clone(),
                    from: me,
                    text,
                };
                self.append_chat(&line);
                self.history.push_back(line.clone());
                if self.history.len() > HISTORY {
                    self.history.pop_front();
                }
                self.broadcast(&ToClient::Chat(line));
            }
            (ToServer::Invite { to, code }, Some(me)) => {
                if !self.allow(id) {
                    return;
                }
                let invite = ToClient::Invited {
                    name: self.members[&me].name.clone(),
                    from: me,
                    code: clean(&code, MAX_CODE),
                };
                if !self.send_to_member(&to, &invite) {
                    let text = "They aren't in the lobby any more.".into();
                    self.send(id, &ToClient::Error { text });
                }
            }
            // shortcut: a reply isn't checked against a sent invite, so any
            // member could close someone's waiting room; track invites if the
            // lobby is ever shared with people you don't trust.
            (ToServer::Reply { to, accept }, Some(me)) => {
                self.send_to_member(&to, &ToClient::Replied { from: me, accept });
            }
        }
    }

    fn hello(&mut self, id: u64, v: u32, name: &str) {
        if v != PROTOCOL {
            let text = format!(
                "The lobby server is a different version (protocol {PROTOCOL}, your GloorpFlorp \
                 has {v}). Update GloorpFlorp and the lobby server to the same version."
            );
            self.send(id, &ToClient::Error { text });
            self.close(id);
            return;
        }
        let Some(conn) = self.conns.get_mut(&id) else {
            return;
        };
        let mut name = clean(name, MAX_NAME);
        if name.is_empty() {
            name = "Someone".into();
        }
        let ip = conn.ip;
        let me = if ip.is_loopback() {
            format!("{ip}/{name}")
        } else {
            ip.to_string()
        };
        conn.member = Some(me.clone());
        let known = Known { ip, name };
        if self.members.get(&me) != Some(&known) {
            self.members.insert(me.clone(), known);
            self.save_members();
        }
        self.send(
            id,
            &ToClient::Welcome {
                v: PROTOCOL,
                you: me,
            },
        );
        for line in self.history.clone() {
            self.send(id, &ToClient::Chat(line));
        }
        // Everyone gets the list again; the newcomer needs it anyway.
        self.sent.clear();
    }

    /// Takes one token from the connection's rate limit.
    fn allow(&mut self, id: u64) -> bool {
        let Some(c) = self.conns.get_mut(&id) else {
            return false;
        };
        let now = Instant::now();
        let refill = now.duration_since(c.refilled).as_secs_f32();
        c.tokens = (c.tokens + refill).min(BURST);
        c.refilled = now;
        if c.tokens < 1.0 {
            let text = "You're sending messages too fast. Wait a moment.".into();
            self.send(id, &ToClient::Error { text });
            return false;
        }
        c.tokens -= 1.0;
        true
    }

    fn member_list(&self) -> Vec<Member> {
        let mut list: Vec<Member> = self
            .members
            .iter()
            .map(|(id, k)| {
                let state = if self.conns.values().any(|c| c.member.as_ref() == Some(id)) {
                    Presence::Lobby
                } else if self.tailnet.get(&k.ip) == Some(&true) {
                    Presence::Online
                } else {
                    Presence::Offline
                };
                Member {
                    id: id.clone(),
                    name: k.name.clone(),
                    state,
                }
            })
            .collect();
        list.sort_by_cached_key(|m| (m.state, m.name.to_lowercase()));
        list
    }

    /// Sends the member list to everyone if it changed.
    fn members_changed(&mut self) {
        // A failed send drops a connection, which changes the list again.
        loop {
            let list = self.member_list();
            if list == self.sent {
                return;
            }
            self.sent = list.clone();
            self.broadcast(&ToClient::Members { list });
        }
    }

    fn send(&mut self, id: u64, msg: &ToClient) {
        self.write(id, &encode(msg));
    }

    fn write(&mut self, id: u64, line: &[u8]) {
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        if let Err(e) = c.stream.write_all(line) {
            log::info!("dropping connection {id} ({}): {e}", c.ip);
            self.close(id);
        }
    }

    /// The reader thread then ends too.
    fn close(&mut self, id: u64) {
        if let Some(c) = self.conns.remove(&id) {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
    }

    /// To everyone who said `Hello`.
    fn broadcast(&mut self, msg: &ToClient) {
        let line = encode(msg);
        let ids: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.member.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.write(id, &line);
        }
    }

    /// False if the member isn't connected.
    fn send_to_member(&mut self, member: &str, msg: &ToClient) -> bool {
        let line = encode(msg);
        let ids: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.member.as_deref() == Some(member))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            self.write(*id, &line);
        }
        !ids.is_empty()
    }

    fn save_members(&self) {
        let path = self.dir.join("members.json");
        let tmp = path.with_extension("json.tmp");
        // Via a temporary file, so a crash never leaves half of one.
        let result = serde_json::to_vec_pretty(&self.members)
            .map_err(io::Error::other)
            .and_then(|json| fs::write(&tmp, json))
            .and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = result {
            log::warn!("couldn't save {}: {e}", path.display());
        }
    }

    fn append_chat(&self, line: &ChatLine) {
        let path = self.dir.join("chat.jsonl");
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&encode(line)));
        if let Err(e) = result {
            log::warn!("couldn't save a chat message to {}: {e}", path.display());
        }
    }
}

#[cfg(unix)]
fn poll_tailnet(path: PathBuf, tx: Sender<Ev>) {
    let mut worked = None;
    loop {
        let result = tailnet_status(&path);
        // Log only changes, not every 5 s.
        if worked != Some(result.is_ok()) {
            worked = Some(result.is_ok());
            match &result {
                Ok(_) => log::info!("reading who is online from {}", path.display()),
                Err(e) => log::warn!(
                    "can't ask tailscaled at {} ({e}); showing only who is in the lobby",
                    path.display()
                ),
            }
        }
        if tx.send(Ev::Tailnet(result.unwrap_or_default())).is_err() {
            return;
        }
        std::thread::sleep(TAILNET_POLL);
    }
}

/// Asks tailscaled's LocalAPI over its Unix socket.
#[cfg(unix)]
fn tailnet_status(path: &Path) -> io::Result<HashMap<IpAddr, bool>> {
    use std::io::Read;
    let mut s = std::os::unix::net::UnixStream::connect(path)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    // HTTP/1.0: no chunked body, and tailscaled closes the connection after
    // answering. The LocalAPI only accepts this Host.
    s.write_all(b"GET /localapi/v0/status HTTP/1.0\r\nHost: local-tailscaled.sock\r\n\r\n")?;
    let mut response = Vec::new();
    s.take(16 << 20).read_to_end(&mut response)?;
    parse_status(&response)
}

/// Tailscale address -> online, from tailscaled's HTTP response to
/// `/localapi/v0/status`. `Self` is the lobby's own machine.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_status(response: &[u8]) -> io::Result<HashMap<IpAddr, bool>> {
    #[derive(Deserialize)]
    struct Status {
        #[serde(rename = "Self")]
        me: Option<Node>,
        #[serde(rename = "Peer")]
        peers: Option<HashMap<String, Node>>,
    }
    #[derive(Deserialize)]
    struct Node {
        #[serde(rename = "TailscaleIPs")]
        ips: Option<Vec<IpAddr>>,
        #[serde(rename = "Online", default)]
        online: bool,
    }
    let bad = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("no HTTP header".into()))?;
    let status_line = String::from_utf8_lossy(&response[..end]);
    let status_line = status_line.lines().next().unwrap_or_default();
    if status_line.split(' ').nth(1) != Some("200") {
        return Err(bad(format!("tailscaled answered \"{status_line}\"")));
    }
    let status: Status =
        serde_json::from_slice(&response[end + 4..]).map_err(|e| bad(e.to_string()))?;
    let mut online = HashMap::new();
    if let Some(me) = status.me {
        online.extend(me.ips.unwrap_or_default().into_iter().map(|ip| (ip, true)));
    }
    for node in status.peers.unwrap_or_default().into_values() {
        online.extend(
            node.ips
                .unwrap_or_default()
                .into_iter()
                .map(|ip| (ip, node.online)),
        );
    }
    Ok(online)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_status_parsing() {
        // Trimmed from a real `tailscale status --json` (same field names),
        // with made-up names and addresses.
        let body = r#"{
            "Version": "1.90.0", "BackendState": "Running",
            "Self": { "ID": "n1", "HostName": "homeserver", "OS": "linux",
                      "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"], "Online": true },
            "Peer": {
                "nodekey:aaaa": { "HostName": "alex-pc", "OS": "windows",
                    "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"], "Online": true,
                    "LastSeen": "0001-01-01T00:00:00Z" },
                "nodekey:bbbb": { "HostName": "sam-pc", "OS": "windows",
                    "TailscaleIPs": ["100.64.0.3"], "Online": false },
                "nodekey:cccc": { "HostName": "no-ips", "TailscaleIPs": null }
            },
            "User": {}
        }"#;
        let response = format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}");
        let online = parse_status(response.as_bytes()).unwrap();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(online.get(&ip("100.64.0.1")), Some(&true));
        assert_eq!(online.get(&ip("100.64.0.2")), Some(&true));
        assert_eq!(online.get(&ip("fd7a:115c:a1e0::2")), Some(&true));
        assert_eq!(online.get(&ip("100.64.0.3")), Some(&false));
        assert_eq!(online.len(), 5);

        let no_peers = "HTTP/1.0 200 OK\r\n\r\n{\"Self\": null, \"Peer\": null}";
        assert!(parse_status(no_peers.as_bytes()).unwrap().is_empty());
        let forbidden = "HTTP/1.0 403 Forbidden\r\n\r\ninvalid localapi request\n";
        assert!(parse_status(forbidden.as_bytes()).is_err());
        assert!(parse_status(b"garbage").is_err());
    }

    #[test]
    fn only_tailscale_and_loopback_addresses() {
        for ok in [
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "fd7a:115c:a1e0::5",
            "::1",
        ] {
            assert!(allowed(ok.parse().unwrap()), "{ok}");
        }
        for bad in [
            "100.128.0.1",
            "100.63.0.1",
            "192.168.1.5",
            "10.0.0.1",
            "fd7a:115c:a1e1::1",
        ] {
            assert!(!allowed(bad.parse().unwrap()), "{bad}");
        }
        assert_eq!(clean(" a\nb\u{7}c ", 40), "a b c");
        assert_eq!(clean("abcdef", 3), "abc");
    }
}
