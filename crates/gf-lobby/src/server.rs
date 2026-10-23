//! The lobby server (see the crate docs). One thread accepts connections, one
//! reads each connection, and one hub thread owns all state and writes to
//! every connection (with a write timeout, so a stuck client can't hold up
//! the others for long). On Linux another thread asks tailscaled every few
//! seconds which devices are online.
//!
//! Files in the data folder: `members.json` (everyone who ever joined),
//! `chat.jsonl` (the chat, one JSON object per line; pictures are big, so
//! once it passes `REWRITE_CHAT_AT` it is cut down to the history) and
//! `files/` (sent files by id; `<id>.part` while an upload is coming in).
//! File connections are served on their connection's own thread, so the hub
//! never waits for a disk or a slow link. So is the web page for phones (see
//! `web`), which shares the port.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use crate::{
    encode, file_id_ok, forget_image, read_line, unix_now, ChatLine, Image, Member, Presence,
    SharedFile, ToClient, ToServer, FILE_DAYS, HISTORY, IDLE_TIMEOUT, IMAGE_HISTORY,
    MAX_CLIENT_LINE, MAX_FILE, MAX_NAME, MAX_TEXT, PROTOCOL,
};

mod web;

pub struct Config {
    pub data_dir: PathBuf,
    /// tailscaled's LocalAPI socket. `None` (or a socket that isn't there):
    /// presence comes from lobby connections only.
    pub tailscale_socket: Option<PathBuf>,
}

/// Threads are cheap but not free; the lobby is for a handful of friends.
const MAX_CONNECTIONS: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// chat.jsonl is rewritten from the history (at most HISTORY lines with
/// IMAGE_HISTORY pictures) once it grows past this.
const REWRITE_CHAT_AT: u64 = 64 << 20;
#[cfg(unix)]
const TAILNET_POLL: Duration = Duration::from_secs(5);
/// Chat messages and invites: bursts of up to this many, then one a second.
const BURST: f32 = 5.0;
/// Room codes are 13 characters; leave room for spaces.
const MAX_CODE: usize = 40;
const TOO_FAST: &str = "You're sending messages too fast. Wait a moment.";
/// Longest file name shown in the chat, in characters.
const MAX_FILE_NAME: usize = 200;

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
    /// The newcomer's history has been written (or couldn't be).
    Replayed {
        id: u64,
        ok: bool,
    },
    /// Tailscale address -> online.
    #[cfg_attr(not(unix), allow(dead_code))]
    Tailnet(HashMap<IpAddr, bool>),
    /// A file upload is complete; `who` is the name the sender gave.
    Shared {
        ip: IpAddr,
        who: String,
        file: SharedFile,
    },
    /// The web page asks what's new since chat line `since` (see `web`).
    WebPoll {
        ip: IpAddr,
        name: String,
        since: u64,
        reply: Sender<Vec<u8>>,
    },
    /// The web page posts a chat line.
    WebChat {
        ip: IpAddr,
        name: String,
        text: String,
        image: Option<Image>,
        reply: Sender<Result<(), String>>,
    },
}

/// A member using the web page.
struct WebVisitor {
    /// Its last poll; it counts as in the lobby for `WEB_PRESENCE` after it.
    seen: Instant,
    tokens: f32,
    refilled: Instant,
}

/// The page polls every 2 s while it is open.
const WEB_PRESENCE: Duration = Duration::from_secs(10);

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
    /// Lines for it while a thread still writes it the history (see
    /// `Ev::Replayed`).
    waiting: Option<Vec<u8>>,
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
    /// For the history-writing threads to report back.
    tx: Sender<Ev>,
    /// Size of chat.jsonl.
    chat_bytes: u64,
    /// Chat lines added since the server started, plus the history it
    /// loaded: `history` holds lines `total - history.len()` to `total - 1`.
    total: u64,
    /// By member id.
    web: HashMap<String, WebVisitor>,
}

/// Runs the lobby on `listener` until the process ends. Fails at the start if
/// the data folder can't be read, or later if the hub thread died.
pub fn serve(listener: TcpListener, cfg: Config) -> io::Result<()> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut hub = Hub::load(&cfg.data_dir, tx.clone())?;
    log::info!(
        "lobby on {} with {} member(s) and {} message(s) of history in {}",
        listener.local_addr()?,
        hub.members.len(),
        hub.history.len(),
        cfg.data_dir.display()
    );
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
        let files = cfg.data_dir.join("files");
        let started = spawn("lobby-conn", move || {
            read_conn(id, ip, stream, &conn_tx, &files);
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

fn read_conn(id: u64, ip: IpAddr, stream: TcpStream, tx: &Sender<Ev>, files: &Path) {
    let mut r = BufReader::new(stream);
    let mut buf = Vec::new();
    let mut first = true;
    loop {
        match read_line(&mut r, &mut buf, MAX_CLIENT_LINE) {
            Ok(Some(line)) if first && web::is_request(line) => {
                let line = line.to_vec();
                web::serve(&mut r, &line, ip, tx, files);
                return;
            }
            Ok(Some(line)) => match serde_json::from_slice::<ToServer>(line) {
                // A file connection carries one file and nothing else.
                Ok(msg @ (ToServer::Upload { .. } | ToServer::Download { .. })) if first => {
                    file_conn(&mut r, msg, files, ip, tx);
                    return;
                }
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
        first = false;
    }
}

/// Serves one upload or download; a problem goes back as `Error`.
fn file_conn(r: &mut BufReader<TcpStream>, msg: ToServer, dir: &Path, ip: IpAddr, tx: &Sender<Ev>) {
    let Ok(mut w) = r.get_ref().try_clone() else {
        return;
    };
    // The other end may be a slow disk taking a 50 GB file.
    let _ = w.set_write_timeout(Some(IDLE_TIMEOUT));
    let result = match msg {
        ToServer::Upload {
            id,
            name,
            size,
            who,
        } => receive(r, &mut w, dir, &id, size).and_then(|new| {
            if new {
                let name = clean(&name, MAX_FILE_NAME);
                log::info!("file {id} ({name}, {size} bytes) stored from {ip}");
                let file = SharedFile { id, name, size };
                let _ = tx.send(Ev::Shared { ip, who, file });
            }
            reply(&mut w, &ToClient::Stored)
        }),
        ToServer::Download { id, from } => send_file(&mut w, dir, &id, from),
        _ => Ok(()),
    };
    if let Err(text) = result {
        log::info!("file transfer with {ip}: {text}");
        let _ = w.write_all(&encode(&ToClient::Error { text }));
    }
}

fn reply(w: &mut TcpStream, msg: &ToClient) -> Result<(), String> {
    w.write_all(&encode(msg))
        .map_err(|e| format!("the connection broke off ({e})"))
}

/// Takes the rest of upload `id` into `<id>.part`, then renames it to `id`.
/// `Ok(false)`: it was already complete (the sender missed `Stored`).
fn receive(
    r: &mut BufReader<TcpStream>,
    w: &mut TcpStream,
    dir: &Path,
    id: &str,
    size: u64,
) -> Result<bool, String> {
    check_upload(id, size)?;
    if dir.join(id).is_file() {
        reply(w, &ToClient::Ready { have: size })?;
        return Ok(false);
    }
    let (file, have) = open_part(dir, id, size)?;
    reply(w, &ToClient::Ready { have })?;
    store(r, file, dir, id, size - have)?;
    Ok(true)
}

fn check_upload(id: &str, size: u64) -> Result<(), String> {
    if !file_id_ok(id) {
        return Err("That file can't be sent (bad id). Update GloorpFlorp.".into());
    }
    if size > MAX_FILE {
        return Err(format!(
            "That file is too big for the lobby (up to {} GB).",
            MAX_FILE >> 30
        ));
    }
    Ok(())
}

/// A problem with the server's disk, in plain words.
fn disk(id: &str, e: io::Error) -> String {
    log::warn!("couldn't store file {id}: {e}");
    if e.kind() == io::ErrorKind::StorageFull {
        "The lobby server's disk is full.".to_string()
    } else {
        format!("The lobby server couldn't save the file ({e}).")
    }
}

/// `<id>.part`, opened to append, and how many of the `size` bytes it has.
fn open_part(dir: &Path, id: &str, size: u64) -> Result<(fs::File, u64), String> {
    fs::create_dir_all(dir).map_err(|e| disk(id, e))?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{id}.part")))
        .map_err(|e| disk(id, e))?;
    let mut have = file.metadata().map_err(|e| disk(id, e))?.len();
    if have > size {
        file.set_len(0).map_err(|e| disk(id, e))?;
        have = 0;
    }
    Ok((file, have))
}

/// Appends the next `left` bytes from `r` to `file` (`<id>.part`), then
/// renames it to `id`.
fn store(r: &mut impl Read, file: fs::File, dir: &Path, id: &str, left: u64) -> Result<(), String> {
    // Dropped (and so flushed) at a break-off too, so the sender resumes
    // after the last byte that arrived.
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut buf = vec![0; 1 << 20];
    let mut left = left;
    while left > 0 {
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = r
            .read(&mut buf[..want])
            .map_err(|e| format!("the upload broke off ({e})"))?;
        if n == 0 {
            return Err("the upload broke off".into());
        }
        out.write_all(&buf[..n]).map_err(|e| disk(id, e))?;
        left -= n as u64;
    }
    out.flush().map_err(|e| disk(id, e))?;
    drop(out);
    fs::rename(dir.join(format!("{id}.part")), dir.join(id)).map_err(|e| disk(id, e))
}

/// Sent file `id`, opened, and its size.
fn open_shared(dir: &Path, id: &str) -> Result<(fs::File, u64), String> {
    let gone = || {
        format!("That file isn't on the lobby server any more (files are deleted after {FILE_DAYS} days).")
    };
    if !file_id_ok(id) {
        return Err(gone());
    }
    let f = fs::File::open(dir.join(id)).map_err(|_| gone())?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    Ok((f, size))
}

/// Sends file `id` from byte `from` on.
fn send_file(w: &mut TcpStream, dir: &Path, id: &str, from: u64) -> Result<(), String> {
    let (mut f, size) = open_shared(dir, id)?;
    f.seek(SeekFrom::Start(from.min(size)))
        .map_err(|e| e.to_string())?;
    reply(w, &ToClient::Sending { size })?;
    io::copy(&mut f, w).map_err(|e| format!("the download broke off ({e})"))?;
    Ok(())
}

/// A chat message's text and picture as they are posted, `None` if there is
/// nothing to post, or why it can't be.
fn chat_message(
    text: &str,
    image: Option<Image>,
) -> Result<Option<(String, Option<Image>)>, String> {
    let text = clean(text, MAX_TEXT);
    if text.is_empty() && image.is_none() {
        return Ok(None);
    }
    if image.as_ref().is_some_and(|i| !i.ok()) {
        return Err("That picture can't be sent: only PNG or JPEG files up to 1 MB.".into());
    }
    Ok(Some((text, image)))
}

/// Takes one token from a rate limit of [`BURST`], refilled one a second.
fn take_token(tokens: &mut f32, refilled: &mut Instant) -> bool {
    let now = Instant::now();
    *tokens = (*tokens + now.duration_since(*refilled).as_secs_f32()).min(BURST);
    *refilled = now;
    if *tokens < 1.0 {
        return false;
    }
    *tokens -= 1.0;
    true
}

/// Deletes sent files (and broken-off uploads) older than [`FILE_DAYS`].
fn expire_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let keep = Duration::from_secs(FILE_DAYS * 24 * 3600);
    for e in entries.flatten() {
        let modified = e.metadata().and_then(|m| m.modified());
        if !modified.is_ok_and(|t| t.elapsed().is_ok_and(|age| age > keep)) {
            continue;
        }
        match fs::remove_file(e.path()) {
            Ok(()) => log::info!(
                "deleted {} (older than {FILE_DAYS} days)",
                e.path().display()
            ),
            Err(err) => log::warn!("couldn't delete {}: {err}", e.path().display()),
        }
    }
}

/// The name a member goes by (see `Hello`).
fn member_name(name: &str) -> String {
    match clean(name, MAX_NAME) {
        n if n.is_empty() => "Someone".into(),
        n => n,
    }
}

/// The Tailscale address; on the server's own machine (development) also the
/// name, so two copies of the app there are two members.
fn member_id(ip: IpAddr, name: &str) -> String {
    if ip.is_loopback() {
        format!("{ip}/{name}")
    } else {
        ip.to_string()
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

/// Adds a line to the history: the last [`HISTORY`] lines, with pictures
/// only on the newest [`IMAGE_HISTORY`] of them.
fn remember(history: &mut VecDeque<ChatLine>, line: ChatLine) {
    history.push_back(line);
    if history.len() > HISTORY {
        history.pop_front();
    }
    let n = history.iter().filter(|l| l.image.is_some()).count();
    for l in history
        .iter_mut()
        .filter(|l| l.image.is_some())
        .take(n.saturating_sub(IMAGE_HISTORY))
    {
        forget_image(l);
    }
}

impl Hub {
    fn load(dir: &Path, tx: Sender<Ev>) -> io::Result<Hub> {
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
        let mut chat_bytes = 0;
        match fs::File::open(dir.join("chat.jsonl")) {
            Ok(f) => {
                chat_bytes = f.metadata().map_or(0, |m| m.len());
                for line in BufReader::new(f).split(b'\n') {
                    // A line cut short by a crash is skipped.
                    if let Ok(c) = serde_json::from_slice::<ChatLine>(&line?) {
                        remember(&mut history, c);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut hub = Hub {
            dir: dir.to_path_buf(),
            members,
            total: history.len() as u64,
            history,
            conns: HashMap::new(),
            tailnet: HashMap::new(),
            sent: Vec::new(),
            tx,
            chat_bytes,
            web: HashMap::new(),
        };
        if hub.chat_bytes > REWRITE_CHAT_AT {
            hub.rewrite_chat();
        }
        Ok(hub)
    }

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Open { id, ip, stream } => {
                let conn = Conn {
                    stream,
                    ip,
                    member: None,
                    waiting: None,
                    tokens: BURST,
                    refilled: Instant::now(),
                };
                self.conns.insert(id, conn);
                // Often enough without a timer of its own.
                expire_files(&self.dir.join("files"));
            }
            Ev::Line { id, msg } => self.message(id, msg),
            Ev::Closed { id } => {
                self.conns.remove(&id);
            }
            Ev::Replayed { id, ok } => {
                let waiting = self.conns.get_mut(&id).and_then(|c| c.waiting.take());
                if !ok {
                    log::info!("connection {id}: couldn't send the history");
                    self.close(id);
                } else if let Some(lines) = waiting {
                    self.write(id, &lines);
                }
            }
            Ev::Tailnet(map) => self.tailnet = map,
            Ev::Shared { ip, who, file } => {
                let me = member_id(ip, &member_name(&who));
                let name = self
                    .members
                    .get(&me)
                    .map_or_else(|| member_name(&who), |k| k.name.clone());
                self.post(ChatLine {
                    from: me,
                    name,
                    at: unix_now(),
                    text: String::new(),
                    image: None,
                    file: Some(file),
                });
            }
            Ev::WebPoll {
                ip,
                name,
                since,
                reply,
            } => {
                let me = self.join(ip, &name);
                self.visitor(&me).seen = Instant::now();
                let first = self.total - self.history.len() as u64;
                // Anything else (e.g. the server restarted): everything.
                let from = if (first..=self.total).contains(&since) {
                    since
                } else {
                    first
                };
                let poll = web::Poll {
                    you: me,
                    members: self.member_list(),
                    next: self.total,
                    lines: self.history.iter().skip((from - first) as usize).collect(),
                };
                let _ = reply.send(serde_json::to_vec(&poll).unwrap_or_default());
            }
            Ev::WebChat {
                ip,
                name,
                text,
                image,
                reply,
            } => {
                let me = self.join(ip, &name);
                let result = chat_message(&text, image).and_then(|message| {
                    let Some((text, image)) = message else {
                        return Ok(());
                    };
                    let v = self.visitor(&me);
                    if !take_token(&mut v.tokens, &mut v.refilled) {
                        return Err(TOO_FAST.into());
                    }
                    self.post(ChatLine {
                        name: self.members[&me].name.clone(),
                        from: me,
                        at: unix_now(),
                        text,
                        image,
                        file: None,
                    });
                    Ok(())
                });
                let _ = reply.send(result);
            }
        }
        self.members_changed();
    }

    /// Shows a new line to everyone and keeps it.
    fn post(&mut self, line: ChatLine) {
        self.broadcast(&ToClient::Chat(line.clone()));
        remember(&mut self.history, line.clone());
        self.total += 1;
        self.append_chat(&line);
    }

    fn visitor(&mut self, me: &str) -> &mut WebVisitor {
        self.web
            .entry(me.to_string())
            .or_insert_with(|| WebVisitor {
                seen: Instant::now(),
                tokens: BURST,
                refilled: Instant::now(),
            })
    }

    /// Makes (or keeps) the device at `ip` a member under `name`; returns its
    /// member id.
    fn join(&mut self, ip: IpAddr, name: &str) -> String {
        let name = member_name(name);
        let me = member_id(ip, &name);
        let known = Known { ip, name };
        if self.members.get(&me) != Some(&known) {
            self.members.insert(me.clone(), known);
            self.save_members();
        }
        me
    }

    fn message(&mut self, id: u64, msg: ToServer) {
        let Some(conn) = self.conns.get(&id) else {
            return;
        };
        match (msg, conn.member.clone()) {
            (ToServer::Ping, _) => self.send(id, &ToClient::Pong),
            (ToServer::Hello { v, name }, None) => self.hello(id, v, &name),
            // Once per connection: each one replays the whole history.
            (ToServer::Hello { .. }, Some(_)) => {}
            // Only as a file connection's first line (see `read_conn`).
            (ToServer::Upload { .. } | ToServer::Download { .. }, _) => {}
            // Everything else needs a Hello first.
            (_, None) => {}
            (ToServer::Chat { text, image }, Some(me)) => {
                let (text, image) = match chat_message(&text, image) {
                    Ok(Some(message)) => message,
                    Ok(None) => return,
                    Err(text) => return self.send(id, &ToClient::Error { text }),
                };
                if !self.allow(id) {
                    let text = TOO_FAST.into();
                    self.send(id, &ToClient::Error { text });
                    return;
                }
                self.post(ChatLine {
                    name: self.members[&me].name.clone(),
                    from: me,
                    at: unix_now(),
                    text,
                    image,
                    file: None,
                });
            }
            (ToServer::Invite { to, code }, Some(me)) => {
                let code = clean(&code, MAX_CODE);
                let invite = ToClient::Invited {
                    name: self.members[&me].name.clone(),
                    from: me,
                    code: code.clone(),
                };
                // The inviter's room waits for an answer, so say why none
                // will come.
                let text = if !self.allow(id) {
                    TOO_FAST
                } else if !self.send_to_member(&to, &invite) {
                    "They aren't in the lobby any more."
                } else {
                    return;
                };
                let text = text.into();
                self.send(id, &ToClient::InviteFailed { to, code, text });
            }
            // Not checked against sent invites: the apps match the room
            // code, which only the two of them know.
            (ToServer::Reply { to, code, accept }, Some(me)) => {
                let code = clean(&code, MAX_CODE);
                let replied = ToClient::Replied {
                    from: me,
                    code,
                    accept,
                };
                self.send_to_member(&to, &replied);
            }
            (ToServer::Cancel { to, code }, Some(me)) => {
                let code = clean(&code, MAX_CODE);
                self.send_to_member(&to, &ToClient::Cancelled { from: me, code });
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
        let Some(ip) = self.conns.get(&id).map(|c| c.ip) else {
            return;
        };
        let me = self.join(ip, name);
        let Some(conn) = self.conns.get_mut(&id) else {
            return;
        };
        conn.member = Some(me.clone());
        conn.waiting = Some(Vec::new());
        let stream = conn.stream.try_clone();
        // The history can be megabytes (pictures), so a thread writes it and
        // the hub goes on; what the newcomer should get meanwhile waits in
        // `Conn::waiting` until `Ev::Replayed`.
        let mut lines = encode(&ToClient::Welcome {
            v: PROTOCOL,
            you: me,
        });
        for line in &self.history {
            lines.extend(encode(&ToClient::Chat(line.clone())));
        }
        let tx = self.tx.clone();
        let started = stream.and_then(|mut stream| {
            spawn("lobby-replay", move || {
                let ok = stream.write_all(&lines).is_ok();
                let _ = tx.send(Ev::Replayed { id, ok });
            })
        });
        if let Err(e) = started {
            log::info!("connection {id}: couldn't send the history ({e})");
            self.close(id);
            return;
        }
        // Everyone gets the list again; the newcomer needs it anyway.
        self.sent.clear();
    }

    /// Takes one token from the connection's rate limit.
    fn allow(&mut self, id: u64) -> bool {
        self.conns
            .get_mut(&id)
            .is_some_and(|c| take_token(&mut c.tokens, &mut c.refilled))
    }

    fn member_list(&self) -> Vec<Member> {
        let mut list: Vec<Member> = self
            .members
            .iter()
            .map(|(id, k)| {
                let on_page = self
                    .web
                    .get(id)
                    .is_some_and(|v| v.seen.elapsed() < WEB_PRESENCE);
                let state = if on_page || self.conns.values().any(|c| c.member.as_ref() == Some(id))
                {
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

    // shortcut: the hub writes to each socket itself, so a connection that
    // stops reading holds everyone up for up to WRITE_TIMEOUT before it is
    // dropped; give each connection its own writer thread and bounded queue
    // if the lobby is ever shared beyond friends' devices.
    fn write(&mut self, id: u64, line: &[u8]) {
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        if let Some(waiting) = &mut c.waiting {
            waiting.extend_from_slice(line);
            return;
        }
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

    fn append_chat(&mut self, line: &ChatLine) {
        let path = self.dir.join("chat.jsonl");
        let bytes = encode(line);
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&bytes));
        if let Err(e) = result {
            log::warn!("couldn't save a chat message to {}: {e}", path.display());
        }
        self.chat_bytes += bytes.len() as u64;
        if self.chat_bytes > REWRITE_CHAT_AT {
            self.rewrite_chat();
        }
    }

    /// Replaces chat.jsonl with the history, so forgotten pictures leave the
    /// disk too.
    fn rewrite_chat(&mut self) {
        let path = self.dir.join("chat.jsonl");
        let tmp = path.with_extension("jsonl.tmp");
        let mut bytes = Vec::new();
        for line in &self.history {
            bytes.extend(encode(line));
        }
        match fs::write(&tmp, &bytes).and_then(|()| fs::rename(&tmp, &path)) {
            Ok(()) => {
                log::info!(
                    "{} cut down to the last {} message(s)",
                    path.display(),
                    self.history.len()
                );
                self.chat_bytes = bytes.len() as u64;
            }
            Err(e) => log::warn!("couldn't cut down {}: {e}", path.display()),
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
    fn chat_file_is_cut_down_to_the_history() {
        let dir = std::env::temp_dir().join(format!("gf-lobby-rewrite-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut hub = Hub::load(&dir, tx.clone()).unwrap();
        let line = |i: u64| ChatLine {
            from: "a".into(),
            name: "A".into(),
            at: i,
            text: format!("m{i}"),
            image: None,
            file: None,
        };
        for i in 0..HISTORY as u64 + 5 {
            remember(&mut hub.history, line(i));
            hub.append_chat(&line(i));
        }
        let lines = |hub: &Hub| {
            let file = fs::read_to_string(hub.dir.join("chat.jsonl")).unwrap();
            (file.lines().count(), file.len() as u64)
        };
        assert_eq!(lines(&hub).0, HISTORY + 5);
        hub.rewrite_chat();
        assert_eq!(lines(&hub), (HISTORY, hub.chat_bytes));
        let again = Hub::load(&dir, tx).unwrap();
        assert_eq!(again.history.len(), HISTORY);
        assert_eq!(again.history[0].text, "m5");
        assert_eq!(again.chat_bytes, hub.chat_bytes);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_files_are_deleted() {
        let dir = std::env::temp_dir().join(format!("gf-lobby-expire-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("0123456789"), dir.join("abcdef0123.part"));
        fs::write(&old, b"x").unwrap();
        fs::write(&new, b"x").unwrap();
        let eight_days = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(eight_days)
            .unwrap();
        expire_files(&dir);
        assert!(!old.exists() && new.exists());
        let _ = fs::remove_dir_all(&dir);
    }

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
