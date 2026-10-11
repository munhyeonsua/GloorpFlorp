//! The running session: shared state and the threads behind [`crate::Net`].
//!
//! Threads: host accept loop (or joiner connect/retry loop); per connection a
//! reader (the handshake/connection thread itself) and a writer; one UDP
//! thread that also emits the 1 s stats tick.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use gf_proto::wire::{
    decode_udp, encode_tcp, encode_udp, TcpMsg, UdpHeader, UdpKind, WireError, MAX_TCP_MSG,
};
use gf_proto::{AudioStream, VideoFrame, PROTOCOL_VERSION};

use crate::framer::Framer;
use crate::loss::LossMeter;
use crate::queue::VideoQueue;
use crate::{NetConfig, NetEvent, NetStats, Role, MAX_VIDEO_QUEUE};

/// Hello/Welcome/Reject are tiny; don't let an unauthenticated peer make us
/// allocate a big buffer.
const HANDSHAKE_MAX_MSG: usize = 64 * 1024;
/// How long `close()` waits for `Bye` to be handed to the OS.
const CLOSE_WAIT: Duration = Duration::from_millis(150);
/// After a local close, how long the reader waits for the peer to hang up.
const CLOSE_DRAIN: Duration = Duration::from_secs(2);
const ACCEPT_POLL: Duration = Duration::from_millis(50);
const NO_RTT: u64 = u64::MAX;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    pub tcp_ping: Duration,
    pub dead_after: Duration,
    pub read_poll: Duration,
    pub udp_ping: Duration,
    pub udp_poll: Duration,
    pub stats: Duration,
    pub retry_every: Duration,
    pub retry_for: Duration,
    pub connect: Duration,
    pub handshake: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            tcp_ping: Duration::from_secs(2),
            dead_after: Duration::from_secs(10),
            read_poll: Duration::from_secs(1),
            udp_ping: Duration::from_millis(500),
            udp_poll: Duration::from_millis(100),
            stats: Duration::from_secs(1),
            retry_every: Duration::from_secs(2),
            retry_for: Duration::from_secs(30),
            connect: Duration::from_secs(5),
            handshake: Duration::from_secs(5),
        }
    }
}

struct Outbox {
    /// Id of the current connection, 0 when there is none.
    conn: u64,
    control: VecDeque<TcpMsg>,
    video: VideoQueue,
    bye_sent: bool,
}

struct UdpTx {
    peer: Option<SocketAddr>,
    token: Option<u32>,
    buf: Vec<u8>,
}

#[derive(Default)]
struct Counters {
    video_sent: AtomicU64,
    video_recv: AtomicU64,
    audio_sent: AtomicU64,
    audio_recv: AtomicU64,
    tcp_rtt_us: AtomicU64,
}

pub(crate) struct Shared {
    cfg: NetConfig,
    timing: Timing,
    events: Sender<NetEvent>,
    epoch: Instant,
    /// Set by `close()` / `abort()` or when the joiner gives up; every thread exits.
    stopped: AtomicBool,
    out: Mutex<Outbox>,
    out_cv: Condvar,
    /// The current connection's socket, so `close()` can unblock its threads.
    stream: Mutex<Option<TcpStream>>,
    udp: UdpSocket,
    udp_tx: Mutex<UdpTx>,
    next_conn: AtomicU64,
    /// Bumped on every connect/disconnect so the UDP thread resets its meters.
    conn_gen: AtomicU64,
    counters: Counters,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking thread can't leave these structures in a broken state.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

enum End {
    /// The peer said goodbye.
    Bye,
    /// We closed the session.
    Closed,
    /// Connection died; the reason is plain language.
    Lost(String),
}

impl Shared {
    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Events are not delivered once the session was stopped locally.
    fn emit(&self, ev: NetEvent) {
        if !self.is_stopped() {
            let _ = self.events.send(ev);
        }
    }

    /// Makes `stream` the current connection. `None` if one already exists
    /// (or the session stopped).
    fn install(&self, stream: &TcpStream, token: u32, udp_peer: Option<SocketAddr>) -> Option<u64> {
        let clone = stream.try_clone().ok()?;
        let mut out = lock(&self.out);
        if out.conn != 0 || self.is_stopped() {
            return None;
        }
        let id = self.next_conn.fetch_add(1, Ordering::SeqCst);
        out.conn = id;
        out.control.clear();
        out.video.clear();
        out.bye_sent = false;
        *lock(&self.stream) = Some(clone);
        {
            let mut u = lock(&self.udp_tx);
            u.token = Some(token);
            u.peer = udp_peer;
        }
        self.counters.tcp_rtt_us.store(NO_RTT, Ordering::SeqCst);
        self.conn_gen.fetch_add(1, Ordering::SeqCst);
        Some(id)
    }

    fn teardown(&self, id: u64) {
        let mut out = lock(&self.out);
        if out.conn != id {
            return;
        }
        out.conn = 0;
        out.control.clear();
        out.video.clear();
        self.out_cv.notify_all();
        if let Some(s) = lock(&self.stream).take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        {
            let mut u = lock(&self.udp_tx);
            u.token = None;
            if matches!(self.cfg.role, Role::Host { .. }) {
                u.peer = None;
            }
        }
        self.conn_gen.fetch_add(1, Ordering::SeqCst);
    }

    fn queue_control(&self, msg: TcpMsg) {
        let mut out = lock(&self.out);
        if out.conn != 0 {
            out.control.push_back(msg);
            self.out_cv.notify_all();
        }
    }

    pub fn send(&self, msg: TcpMsg) {
        if self.is_stopped() {
            return;
        }
        if let TcpMsg::Video(frame) = msg {
            return self.send_video(frame);
        }
        self.queue_control(msg);
    }

    pub fn send_video(&self, frame: VideoFrame) {
        if self.is_stopped() {
            return;
        }
        let mut out = lock(&self.out);
        if out.conn != 0 {
            out.video.push(frame);
            self.out_cv.notify_all();
        }
    }

    pub fn send_audio(&self, stream: AudioStream, seq: u16, ts: u32, payload: &[u8]) {
        if self.is_stopped() {
            return;
        }
        let mut u = lock(&self.udp_tx);
        let (Some(peer), Some(token)) = (u.peer, u.token) else {
            return;
        };
        let kind = match stream {
            AudioStream::Voice => UdpKind::Voice,
            AudioStream::Game => UdpKind::Game,
        };
        encode_udp(
            &UdpHeader {
                kind,
                token,
                seq,
                ts,
            },
            payload,
            &mut u.buf,
        );
        match self.udp.send_to(&u.buf, peer) {
            Ok(n) => {
                self.counters
                    .audio_sent
                    .fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) => log::trace!("audio send failed: {e}"),
        }
    }

    /// Sends `Bye` (if connected) and stops everything. Waits at most
    /// [`CLOSE_WAIT`] for the `Bye` to leave.
    pub fn close(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        log::info!("closing the connection session");
        let bye_sent = {
            let mut out = lock(&self.out);
            if out.conn == 0 {
                false
            } else {
                let id = out.conn;
                out.video.clear();
                out.control.push_back(TcpMsg::Bye);
                self.out_cv.notify_all();
                let (out, _) = self
                    .out_cv
                    .wait_timeout_while(out, CLOSE_WAIT, |o| !o.bye_sent && o.conn == id)
                    .unwrap_or_else(|e| e.into_inner());
                out.bye_sent
            }
        };
        if let Some(s) = lock(&self.stream).as_ref() {
            // After a Bye, half-close so it is delivered and the reader can
            // wait for the peer to hang up; otherwise cut the connection.
            let how = if bye_sent {
                Shutdown::Write
            } else {
                Shutdown::Both
            };
            let _ = s.shutdown(how);
        }
    }

    /// Stops without saying goodbye, as if the PC vanished (tests only).
    #[cfg(test)]
    pub fn abort(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let mut out = lock(&self.out);
        if let Some(s) = lock(&self.stream).take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        out.conn = 0;
        self.out_cv.notify_all();
    }

    fn stats(&self, rtt_ms: Option<f32>, loss_pct: f32, secs: f32) -> NetStats {
        let kbps = |c: &AtomicU64| c.swap(0, Ordering::Relaxed) as f32 * 8.0 / 1000.0 / secs;
        let (connected, queue, dropped) = {
            let out = lock(&self.out);
            (out.conn != 0, out.video.len(), out.video.dropped())
        };
        let tcp_rtt = self.counters.tcp_rtt_us.load(Ordering::Relaxed);
        NetStats {
            connected,
            rtt_ms: rtt_ms.or((tcp_rtt != NO_RTT).then(|| tcp_rtt as f32 / 1000.0)),
            video_send_kbps: kbps(&self.counters.video_sent),
            video_recv_kbps: kbps(&self.counters.video_recv),
            audio_send_kbps: kbps(&self.counters.audio_sent),
            audio_recv_kbps: kbps(&self.counters.audio_recv),
            video_queue_frames: queue,
            video_frames_dropped: dropped,
            audio_loss_pct: loss_pct,
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(|_| ())
}

/// Plain-language text for a failure to bind the host port.
fn bind_error(e: &io::Error, port: u16) -> String {
    match e.kind() {
        ErrorKind::AddrInUse => format!("Port {port} is already in use. Is GloorpFlorp already open?"),
        ErrorKind::AddrNotAvailable => {
            "Your Tailscale address isn't available. Make sure Tailscale is connected, then try again.".into()
        }
        _ => format!(
            "Couldn't open port {port} for your friend to connect ({e}). Restart GloorpFlorp, then try again."
        ),
    }
}

/// Binds the host's TCP listener and UDP socket to the same port. With
/// port 0 the OS picks the TCP port, which may already be taken for UDP
/// (e.g. by another program's socket), so a few ports are tried.
fn bind_host(bind_ip: Ipv4Addr, port: u16) -> anyhow::Result<(TcpListener, UdpSocket)> {
    let mut attempts = 0;
    loop {
        let listener = TcpListener::bind((bind_ip, port))
            .map_err(|e| anyhow::anyhow!(bind_error(&e, port)))?;
        let actual = listener.local_addr()?.port();
        match UdpSocket::bind((bind_ip, actual)) {
            Ok(udp) => return Ok((listener, udp)),
            Err(e) if port == 0 && attempts < 20 => {
                log::debug!("UDP port {actual} is taken ({e}); trying another port");
                attempts += 1;
            }
            Err(e) => return Err(anyhow::anyhow!(bind_error(&e, actual))),
        }
    }
}

const UNREACHABLE: &str = "Couldn't reach your friend's PC over Tailscale. Check that both PCs show as connected in Tailscale and that your friend allowed GloorpFlorp through Windows Firewall.";

fn connect_error(e: &io::Error) -> String {
    match e.kind() {
        ErrorKind::ConnectionRefused => "Your friend's PC answered, but GloorpFlorp isn't hosting a room there (or Windows Firewall blocked it).".into(),
        _ => UNREACHABLE.into(),
    }
}

pub(crate) fn start(
    cfg: NetConfig,
    timing: Timing,
) -> anyhow::Result<(crossbeam_channel::Receiver<NetEvent>, Arc<Shared>)> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let port = cfg.port;
    let (listener, udp) = match cfg.role {
        Role::Host { bind_ip } => {
            let (listener, udp) = bind_host(bind_ip, port)?;
            listener.set_nonblocking(true)?;
            (Some(listener), udp)
        }
        Role::Join { .. } => {
            let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| {
                anyhow::anyhow!("Couldn't open a network socket for voice ({e}). Restart GloorpFlorp, then try again.")
            })?;
            (None, udp)
        }
    };
    udp.set_read_timeout(Some(timing.udp_poll))?;
    let sh = Arc::new(Shared {
        cfg,
        timing,
        events: tx,
        epoch: Instant::now(),
        stopped: AtomicBool::new(false),
        out: Mutex::new(Outbox {
            conn: 0,
            control: VecDeque::new(),
            video: VideoQueue::new(MAX_VIDEO_QUEUE),
            bye_sent: false,
        }),
        out_cv: Condvar::new(),
        stream: Mutex::new(None),
        udp,
        udp_tx: Mutex::new(UdpTx {
            peer: None,
            token: None,
            buf: Vec::with_capacity(1500),
        }),
        next_conn: AtomicU64::new(1),
        conn_gen: AtomicU64::new(0),
        counters: Counters {
            tcp_rtt_us: AtomicU64::new(NO_RTT),
            ..Default::default()
        },
    });
    {
        let sh = sh.clone();
        spawn("gf-net-udp", move || udp_loop(&sh))?;
    }
    if let Some(listener) = listener {
        let addr = listener.local_addr()?;
        log::info!("hosting a room on {addr}");
        sh.emit(NetEvent::Listening { addr });
        let sh2 = sh.clone();
        if let Err(e) = spawn("gf-net-accept", move || accept_loop(&sh2, listener, addr)) {
            sh.stopped.store(true, Ordering::SeqCst);
            return Err(e.into());
        }
    } else {
        let sh2 = sh.clone();
        if let Err(e) = spawn("gf-net-join", move || join_loop(&sh2)) {
            sh.stopped.store(true, Ordering::SeqCst);
            return Err(e.into());
        }
    }
    Ok((rx, sh))
}

// ---------------------------------------------------------------- host

fn accept_loop(sh: &Arc<Shared>, listener: TcpListener, addr: SocketAddr) {
    while !sh.is_stopped() {
        match listener.accept() {
            Ok((stream, from)) => {
                log::debug!("incoming connection from {from}");
                let sh = sh.clone();
                if let Err(e) = spawn("gf-net-conn", move || host_conn(&sh, stream, from, addr)) {
                    log::warn!("couldn't start a connection thread: {e}");
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(e) => {
                log::warn!("accepting a connection failed: {e}");
                thread::sleep(ACCEPT_POLL * 4);
            }
        }
    }
    log::debug!("accept loop stopped");
}

fn reject(mut stream: TcpStream, reason: &str) {
    log::info!("refusing a join: {reason}");
    let _ = stream.write_all(&encode_tcp(&TcpMsg::Reject {
        reason: reason.into(),
    }));
    let _ = stream.shutdown(Shutdown::Write);
}

fn host_conn(sh: &Arc<Shared>, stream: TcpStream, from: SocketAddr, listen_addr: SocketAddr) {
    // Windows hands out accepted sockets with the listener's non-blocking mode.
    if let Err(e) = stream
        .set_nonblocking(false)
        .and_then(|_| stream.set_nodelay(true))
    {
        log::warn!("couldn't set up the connection from {from}: {e}");
        return;
    }
    let hello = match read_handshake(&stream, sh.timing.handshake) {
        Ok(m) => m,
        Err(e) => {
            log::debug!("handshake with {from} failed: {e}");
            return;
        }
    };
    let TcpMsg::Hello {
        version,
        secret,
        name,
        encoders,
    } = hello
    else {
        log::debug!("{from} didn't start with Hello");
        return;
    };
    if version != PROTOCOL_VERSION {
        return reject(stream, "Your GloorpFlorp is a different version from your friend's. Both of you need the same (latest) version, then try again.");
    }
    if secret != sh.cfg.secret {
        return reject(
            stream,
            "Wrong room code. Ask your friend to send the code again and paste it exactly.",
        );
    }
    let token = rand::random::<u32>();
    let Some(id) = sh.install(&stream, token, None) else {
        if sh.is_stopped() {
            return;
        }
        return reject(
            stream,
            "This room is full (your friend is already connected to someone).",
        );
    };
    log::info!("{name} joined from {from}");
    sh.emit(NetEvent::Connected {
        peer_name: name,
        peer_encoders: encoders,
    });
    let welcome = TcpMsg::Welcome {
        name: sh.cfg.name.clone(),
        encoders: sh.cfg.encoders.clone(),
        udp_token: token,
    };
    let end = match (&stream).write_all(&encode_tcp(&welcome)) {
        Ok(()) => run_conn(sh, &stream, id),
        Err(e) => End::Lost(format!("The connection to your friend was lost ({e}).")),
    };
    sh.teardown(id);
    let reason = match end {
        End::Closed => return,
        End::Bye => "Your friend left the room.".to_string(),
        End::Lost(r) => r,
    };
    log::info!("peer disconnected: {reason}");
    sh.emit(NetEvent::Disconnected {
        reason,
        will_retry: false,
    });
    sh.emit(NetEvent::Listening { addr: listen_addr });
}

// ---------------------------------------------------------------- joiner

enum JoinFail {
    Rejected(String),
    Failed(String),
}

fn join_attempt(sh: &Shared, addr: SocketAddr) -> Result<(TcpStream, u64), JoinFail> {
    let stream = TcpStream::connect_timeout(&addr, sh.timing.connect).map_err(|e| {
        log::debug!("connecting to {addr} failed: {e}");
        JoinFail::Failed(connect_error(&e))
    })?;
    let no_answer = || {
        JoinFail::Failed("Your friend's PC accepted the connection but didn't answer like GloorpFlorp. Make sure your friend is hosting a room, then try again.".into())
    };
    stream
        .set_nodelay(true)
        .map_err(|e| JoinFail::Failed(connect_error(&e)))?;
    let hello = TcpMsg::Hello {
        version: PROTOCOL_VERSION,
        secret: sh.cfg.secret,
        name: sh.cfg.name.clone(),
        encoders: sh.cfg.encoders.clone(),
    };
    (&stream)
        .write_all(&encode_tcp(&hello))
        .map_err(|e| JoinFail::Failed(connect_error(&e)))?;
    match read_handshake(&stream, sh.timing.handshake) {
        Ok(TcpMsg::Welcome {
            name,
            encoders,
            udp_token,
        }) => {
            let id = sh
                .install(&stream, udp_token, Some(addr))
                .ok_or_else(|| JoinFail::Failed("The connection was closed.".into()))?;
            log::info!("joined {name}'s room at {addr}");
            sh.emit(NetEvent::Connected {
                peer_name: name,
                peer_encoders: encoders,
            });
            Ok((stream, id))
        }
        Ok(TcpMsg::Reject { reason }) => Err(JoinFail::Rejected(reason)),
        Ok(other) => {
            log::debug!("unexpected handshake reply: {other:?}");
            Err(no_answer())
        }
        Err(e) => {
            log::debug!("handshake with {addr} failed: {e}");
            Err(no_answer())
        }
    }
}

/// Sleeps in small steps; false if the session stopped meanwhile.
fn sleep_unless_stopped(sh: &Shared, d: Duration) -> bool {
    let until = Instant::now() + d;
    while !sh.is_stopped() {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(ACCEPT_POLL));
    }
    false
}

fn join_loop(sh: &Arc<Shared>) {
    let Role::Join { host_ip } = sh.cfg.role else {
        return;
    };
    let addr = SocketAddr::V4(SocketAddrV4::new(host_ip, sh.cfg.port));
    log::info!("joining a room at {addr}");
    let stop = |ev: NetEvent| {
        sh.emit(ev);
        sh.stopped.store(true, Ordering::SeqCst);
    };
    let (mut stream, mut id) = match join_attempt(sh, addr) {
        Ok(c) => c,
        Err(JoinFail::Rejected(reason)) => return stop(NetEvent::Rejected { reason }),
        Err(JoinFail::Failed(msg)) => return stop(NetEvent::Error(msg)),
    };
    loop {
        let end = run_conn(sh, &stream, id);
        sh.teardown(id);
        let reason = match end {
            End::Closed => return,
            End::Bye => {
                return stop(NetEvent::Disconnected {
                    reason: "Your friend closed the room.".into(),
                    will_retry: false,
                })
            }
            End::Lost(r) => r,
        };
        log::info!("connection lost ({reason}), reconnecting");
        sh.emit(NetEvent::Disconnected {
            reason,
            will_retry: true,
        });
        let give_up = Instant::now() + sh.timing.retry_for;
        loop {
            if !sleep_unless_stopped(sh, sh.timing.retry_every) {
                return;
            }
            match join_attempt(sh, addr) {
                Ok(c) => {
                    (stream, id) = c;
                    break;
                }
                // During a reconnect the host may still hold the old,
                // dead connection ("room is full"), so keep trying.
                Err(JoinFail::Rejected(r) | JoinFail::Failed(r)) => {
                    log::debug!("reconnect attempt failed: {r}");
                    if Instant::now() >= give_up {
                        let secs = sh.timing.retry_for.as_secs();
                        return stop(NetEvent::Disconnected {
                            reason: format!("Couldn't reconnect to your friend for {secs} seconds. {r} Join again when your friend is back."),
                            will_retry: false,
                        });
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- connection

fn read_handshake(stream: &TcpStream, timeout: Duration) -> Result<TcpMsg, WireError> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    let deadline = Instant::now() + timeout;
    let mut framer = Framer::new(HANDSHAKE_MAX_MSG);
    loop {
        if let Some(m) = framer.poll(&mut &*stream)? {
            return Ok(m);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::from(ErrorKind::TimedOut).into());
        }
    }
}

fn lost_reason(e: &WireError) -> String {
    match e {
        WireError::Io(e) if e.kind() == ErrorKind::UnexpectedEof => {
            "The connection to your friend was closed unexpectedly.".into()
        }
        WireError::Io(e) => format!("The connection to your friend was lost ({e})."),
        _ => format!("Your friend's GloorpFlorp sent something this version doesn't understand ({e}). Make sure you both use the same version."),
    }
}

/// Runs the reader on this thread (and a writer thread) until the
/// connection ends.
fn run_conn(sh: &Arc<Shared>, stream: &TcpStream, id: u64) -> End {
    match stream.try_clone() {
        Ok(w) => {
            let sh2 = sh.clone();
            if let Err(e) = spawn("gf-net-writer", move || writer(&sh2, w, id)) {
                return End::Lost(format!("Couldn't start sending ({e})."));
            }
        }
        Err(e) => return End::Lost(format!("Couldn't start sending ({e}).")),
    }
    if let Err(e) = stream.set_read_timeout(Some(sh.timing.read_poll)) {
        return End::Lost(format!("The connection to your friend was lost ({e})."));
    }
    let mut framer = Framer::new(MAX_TCP_MSG);
    let mut last_rx = Instant::now();
    let mut closing_since: Option<Instant> = None;
    loop {
        let res = framer.poll(&mut &*stream);
        if framer.take_progress() {
            last_rx = Instant::now();
        }
        if sh.is_stopped() {
            // Wait (briefly) for the peer to hang up after our Bye.
            let since = *closing_since.get_or_insert_with(Instant::now);
            if res.is_err() || since.elapsed() >= CLOSE_DRAIN {
                return End::Closed;
            }
            continue;
        }
        match res {
            Ok(Some(msg)) => {
                if let Some(end) = handle_msg(sh, msg) {
                    return end;
                }
            }
            Ok(None) => {}
            Err(e) => return End::Lost(lost_reason(&e)),
        }
        if last_rx.elapsed() >= sh.timing.dead_after {
            let secs = sh.timing.dead_after.as_secs_f32().round();
            return End::Lost(format!(
                "Lost the connection to your friend (no answer for {secs} seconds)."
            ));
        }
    }
}

fn handle_msg(sh: &Shared, msg: TcpMsg) -> Option<End> {
    match msg {
        TcpMsg::Ping { nonce } => sh.queue_control(TcpMsg::Pong { nonce }),
        TcpMsg::Pong { nonce } => {
            if let Some(rtt) = sh.now_us().checked_sub(nonce) {
                sh.counters.tcp_rtt_us.store(rtt, Ordering::Relaxed);
            }
        }
        TcpMsg::Video(frame) => {
            sh.counters
                .video_recv
                .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
            sh.emit(NetEvent::Video(frame));
        }
        TcpMsg::Bye => return Some(End::Bye),
        TcpMsg::Hello { .. } | TcpMsg::Welcome { .. } | TcpMsg::Reject { .. } => {
            log::debug!("ignoring a handshake message on an open connection");
        }
        other => sh.emit(NetEvent::Msg(other)),
    }
    None
}

fn writer(sh: &Shared, mut stream: TcpStream, id: u64) {
    let mut next_ping = Instant::now() + sh.timing.tcp_ping;
    let mut buf = Vec::new();
    loop {
        let (control, video) = {
            let mut out = lock(&sh.out);
            loop {
                if out.conn != id {
                    return;
                }
                let now = Instant::now();
                if now >= next_ping {
                    out.control.push_back(TcpMsg::Ping { nonce: sh.now_us() });
                    next_ping = now + sh.timing.tcp_ping;
                }
                if !out.control.is_empty() || !out.video.is_empty() {
                    break;
                }
                out = sh
                    .out_cv
                    .wait_timeout(out, next_ping - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            let control: Vec<TcpMsg> = out.control.drain(..).collect();
            (control, out.video.pop())
        };
        buf.clear();
        let mut bye = false;
        for m in &control {
            bye |= matches!(m, TcpMsg::Bye);
            buf.extend_from_slice(&encode_tcp(m));
        }
        let res = (|| -> io::Result<()> {
            if !buf.is_empty() {
                stream.write_all(&buf)?;
            }
            if bye {
                return Ok(());
            }
            if let Some(frame) = video {
                let enc = encode_tcp(&TcpMsg::Video(frame));
                stream.write_all(&enc)?;
                sh.counters
                    .video_sent
                    .fetch_add(enc.len() as u64, Ordering::Relaxed);
            }
            Ok(())
        })();
        if let Err(e) = res {
            log::debug!("connection write failed: {e}");
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        if bye {
            let mut out = lock(&sh.out);
            out.bye_sent = true;
            sh.out_cv.notify_all();
            return;
        }
    }
}

// ---------------------------------------------------------------- UDP

fn udp_loop(sh: &Shared) {
    let t = sh.timing;
    // Max UDP size, so an oversized datagram is never a receive error (Windows).
    let mut buf = vec![0u8; 65_536];
    let mut out = Vec::with_capacity(64);
    let mut loss = LossMeter::default();
    let mut rtt: Option<(f32, Instant)> = None;
    let mut seen_gen = u64::MAX;
    let mut ping_seq: u16 = 0;
    let mut next_ping = Instant::now();
    let mut last_stats = Instant::now();
    let mut warned = false;
    while !sh.is_stopped() {
        let now = Instant::now();
        let gen = sh.conn_gen.load(Ordering::SeqCst);
        if gen != seen_gen {
            seen_gen = gen;
            loss.reset();
            rtt = None;
            next_ping = now;
        }
        if now >= next_ping {
            next_ping = now + t.udp_ping;
            let u = lock(&sh.udp_tx);
            if let (Some(peer), Some(token)) = (u.peer, u.token) {
                ping_seq = ping_seq.wrapping_add(1);
                let hdr = UdpHeader {
                    kind: UdpKind::Ping,
                    token,
                    seq: ping_seq,
                    ts: 0,
                };
                encode_udp(&hdr, &sh.now_us().to_le_bytes(), &mut out);
                if let Err(e) = sh.udp.send_to(&out, peer) {
                    log::trace!("UDP ping failed: {e}");
                }
            }
        }
        if now.duration_since(last_stats) >= t.stats {
            let secs = now.duration_since(last_stats).as_secs_f32();
            last_stats = now;
            let fresh = rtt.filter(|(_, at)| now.duration_since(*at) < t.udp_ping * 6);
            sh.emit(NetEvent::Stats(sh.stats(
                fresh.map(|r| r.0),
                loss.loss_pct(now),
                secs,
            )));
        }
        let (n, src) = match sh.udp.recv_from(&mut buf) {
            Ok(r) => r,
            // ConnectionReset: Windows reports an ICMP "port unreachable"
            // from an earlier send this way; harmless.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock
                        | ErrorKind::TimedOut
                        | ErrorKind::Interrupted
                        | ErrorKind::ConnectionReset
                ) =>
            {
                continue
            }
            Err(e) => {
                if !warned {
                    log::warn!("receiving audio failed: {e}");
                    warned = true;
                }
                thread::sleep(t.udp_poll);
                continue;
            }
        };
        let Some((hdr, payload)) = decode_udp(&buf[..n]) else {
            continue;
        };
        {
            let mut u = lock(&sh.udp_tx);
            if u.token != Some(hdr.token) {
                continue;
            }
            if matches!(sh.cfg.role, Role::Host { .. }) && u.peer != Some(src) {
                log::info!("friend's audio address is {src}");
                u.peer = Some(src);
            }
        }
        match hdr.kind {
            UdpKind::Ping => {
                let pong = UdpHeader {
                    kind: UdpKind::Pong,
                    ..hdr
                };
                encode_udp(&pong, payload, &mut out);
                let _ = sh.udp.send_to(&out, src);
            }
            UdpKind::Pong => {
                let Ok(sent) = <[u8; 8]>::try_from(payload) else {
                    continue;
                };
                if let Some(us) = sh.now_us().checked_sub(u64::from_le_bytes(sent)) {
                    let ms = us as f32 / 1000.0;
                    let avg = rtt.map_or(ms, |(r, _)| r * 0.75 + ms * 0.25);
                    rtt = Some((avg, Instant::now()));
                }
            }
            UdpKind::Voice | UdpKind::Game => {
                let stream = if hdr.kind == UdpKind::Voice {
                    AudioStream::Voice
                } else {
                    AudioStream::Game
                };
                sh.counters
                    .audio_recv
                    .fetch_add(n as u64, Ordering::Relaxed);
                loss.record(stream, hdr.seq, Instant::now());
                sh.emit(NetEvent::Audio {
                    stream,
                    seq: hdr.seq,
                    ts: hdr.ts,
                    payload: payload.to_vec(),
                });
            }
        }
    }
    log::debug!("UDP loop stopped");
}
