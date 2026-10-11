//! Loopback tests: host and joiner in one process on 127.0.0.1.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use gf_proto::wire::{encode_tcp, encode_udp, read_tcp, TcpMsg, UdpHeader, UdpKind};
use gf_proto::{AudioStream, VideoCodec, VideoFrame, PROTOCOL_VERSION};

use crate::session::Timing;
use crate::*;

const SECRET: u32 = 0x1234_5678;
const WAIT: Duration = Duration::from_secs(10);

fn timing() -> Timing {
    Timing {
        tcp_ping: Duration::from_millis(200),
        dead_after: Duration::from_millis(1500),
        read_poll: Duration::from_millis(100),
        udp_ping: Duration::from_millis(100),
        udp_poll: Duration::from_millis(50),
        stats: Duration::from_millis(200),
        retry_every: Duration::from_millis(200),
        retry_for: Duration::from_secs(3),
        connect: Duration::from_secs(4),
        handshake: Duration::from_secs(2),
    }
}

fn host_on(port: u16) -> anyhow::Result<Net> {
    start_with(
        NetConfig {
            role: Role::Host {
                bind_ip: Ipv4Addr::LOCALHOST,
            },
            port,
            secret: SECRET,
            name: "Gloorp".into(),
            encoders: vec![VideoCodec::Av1, VideoCodec::Hevc],
        },
        timing(),
    )
}

/// A host on an OS-picked port, plus that port.
fn host() -> (Net, u16) {
    let net = host_on(0).unwrap();
    let NetEvent::Listening { addr } = next(&net, |e| matches!(e, NetEvent::Listening { .. }))
    else {
        unreachable!()
    };
    assert_ne!(addr.port(), 0);
    (net, addr.port())
}

fn join_with(port: u16, secret: u32) -> Net {
    start_with(
        NetConfig {
            role: Role::Join {
                host_ip: Ipv4Addr::LOCALHOST,
            },
            port,
            secret,
            name: "Florp".into(),
            encoders: vec![VideoCodec::H264],
        },
        timing(),
    )
    .unwrap()
}

fn join(port: u16) -> Net {
    join_with(port, SECRET)
}

/// Next event matching `pred`, skipping the others. Panics after [`WAIT`].
fn next(net: &Net, pred: impl Fn(&NetEvent) -> bool) -> NetEvent {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match net.events.recv_timeout(left) {
            Ok(e) if pred(&e) => return e,
            Ok(_) => {}
            Err(_) => panic!("timed out waiting for an event"),
        }
    }
}

fn connected(net: &Net) -> NetEvent {
    next(net, |e| matches!(e, NetEvent::Connected { .. }))
}

fn pair() -> (Net, Net, u16) {
    let (h, port) = host();
    let j = join(port);
    connected(&h);
    connected(&j);
    (h, j, port)
}

#[test]
fn handshake_and_control_messages() {
    let (h, port) = host();
    let j = join(port);
    match connected(&h) {
        NetEvent::Connected {
            peer_name,
            peer_encoders,
        } => {
            assert_eq!(peer_name, "Florp");
            assert_eq!(peer_encoders, vec![VideoCodec::H264]);
        }
        _ => unreachable!(),
    }
    match connected(&j) {
        NetEvent::Connected {
            peer_name,
            peer_encoders,
        } => {
            assert_eq!(peer_name, "Gloorp");
            assert_eq!(peer_encoders, vec![VideoCodec::Av1, VideoCodec::Hevc]);
        }
        _ => unreachable!(),
    }
    let started = TcpMsg::ShareStarted {
        codec: VideoCodec::Av1,
        width: 2560,
        height: 1440,
        fps: 60,
    };
    h.tx.send(started.clone());
    h.tx.send(TcpMsg::GameAudio { active: true });
    assert!(
        matches!(next(&j, |e| matches!(e, NetEvent::Msg(_))), NetEvent::Msg(m) if m == started)
    );
    assert!(matches!(
        next(&j, |e| matches!(e, NetEvent::Msg(_))),
        NetEvent::Msg(TcpMsg::GameAudio { active: true })
    ));
    j.tx.send(TcpMsg::Watching { on: true });
    assert!(matches!(
        next(&h, |e| matches!(e, NetEvent::Msg(_))),
        NetEvent::Msg(TcpMsg::Watching { on: true })
    ));
}

#[test]
fn wrong_secret_is_rejected_and_host_keeps_listening() {
    let (h, port) = host();
    let bad = join_with(port, SECRET ^ 1);
    match next(&bad, |e| {
        matches!(e, NetEvent::Rejected { .. } | NetEvent::Connected { .. })
    }) {
        NetEvent::Rejected { reason } => assert!(reason.contains("Wrong room code"), "{reason}"),
        e => panic!("{e:?}"),
    }
    assert!(!h
        .events
        .try_iter()
        .any(|e| matches!(e, NetEvent::Connected { .. })));
    let _good = join(port);
    connected(&h);
}

#[test]
fn version_mismatch_is_rejected() {
    let (_h, port) = host();
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let hello = TcpMsg::Hello {
        version: PROTOCOL_VERSION + 1,
        secret: SECRET,
        name: "Future".into(),
        encoders: vec![],
    };
    s.write_all(&encode_tcp(&hello)).unwrap();
    match read_tcp(&mut s).unwrap() {
        TcpMsg::Reject { reason } => assert!(reason.contains("version"), "{reason}"),
        m => panic!("{m:?}"),
    }
}

#[test]
fn second_joiner_finds_room_full() {
    let (h, j, port) = pair();
    let second = join(port);
    match next(&second, |e| {
        matches!(e, NetEvent::Rejected { .. } | NetEvent::Connected { .. })
    }) {
        NetEvent::Rejected { reason } => assert!(reason.contains("room is full"), "{reason}"),
        e => panic!("{e:?}"),
    }
    // The first connection is unaffected.
    j.tx.send(TcpMsg::ShareStopped);
    assert!(matches!(
        next(&h, |e| matches!(e, NetEvent::Msg(_))),
        NetEvent::Msg(TcpMsg::ShareStopped)
    ));
}

fn test_frame(i: usize) -> VideoFrame {
    let len = match i % 7 {
        0 => 1_000_000,
        1 => 1,
        _ => (i * 7919) % 300_000 + 1,
    };
    VideoFrame {
        codec: VideoCodec::Hevc,
        keyframe: i.is_multiple_of(30),
        sent_us: i as u64,
        data: (0..len).map(|b| (b * 31 + i) as u8).collect(),
    }
}

#[test]
fn video_frames_arrive_intact_and_in_order() {
    let (h, j, _) = pair();
    let mut got = 0;
    for batch in 0..20 {
        for i in batch * 10..batch * 10 + 10 {
            h.tx.send_video(test_frame(i));
        }
        while got < batch * 10 + 10 {
            let NetEvent::Video(f) = next(&j, |e| matches!(e, NetEvent::Video(_))) else {
                unreachable!()
            };
            assert!(f == test_frame(got), "frame {got} differs");
            got += 1;
        }
    }
    let NetEvent::Stats(s) = next(
        &h,
        |e| matches!(e, NetEvent::Stats(s) if s.video_send_kbps > 0.0),
    ) else {
        unreachable!()
    };
    assert_eq!(s.video_frames_dropped, 0);
}

fn recv_audio(net: &Net, send: impl Fn()) -> NetEvent {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        send();
        let until = Instant::now() + Duration::from_millis(100);
        while let Ok(e) = net.events.recv_deadline(until) {
            if matches!(e, NetEvent::Audio { .. }) {
                return e;
            }
        }
    }
    panic!("no audio arrived");
}

#[test]
fn audio_datagrams_both_ways() {
    let (h, j, port) = pair();
    // Junk and wrong-token datagrams to the host are ignored.
    let junk = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut buf = Vec::new();
    let hdr = UdpHeader {
        kind: UdpKind::Voice,
        token: 1,
        seq: 1,
        ts: 1,
    };
    for token in 0..50 {
        encode_udp(&UdpHeader { token, ..hdr }, b"nope", &mut buf);
        junk.send_to(&buf, (Ipv4Addr::LOCALHOST, port)).unwrap();
    }
    junk.send_to(b"garbage", (Ipv4Addr::LOCALHOST, port))
        .unwrap();

    let ev = recv_audio(&h, || {
        j.tx.send_audio(AudioStream::Voice, 65_535, 480, b"opus voice")
    });
    match ev {
        NetEvent::Audio {
            stream,
            seq,
            ts,
            payload,
        } => {
            assert_eq!((stream, seq, ts), (AudioStream::Voice, 65_535, 480));
            assert_eq!(payload, b"opus voice");
        }
        _ => unreachable!(),
    }
    let ev = recv_audio(&j, || {
        h.tx.send_audio(AudioStream::Game, 7, 96_000, &[9; 300])
    });
    match ev {
        NetEvent::Audio {
            stream,
            seq,
            ts,
            payload,
        } => {
            assert_eq!((stream, seq, ts), (AudioStream::Game, 7, 96_000));
            assert_eq!(payload, vec![9; 300]);
        }
        _ => unreachable!(),
    }
}

#[test]
fn rtt_and_loss_show_in_stats() {
    let (h, j, _) = pair();
    for net in [&h, &j] {
        let NetEvent::Stats(s) = next(
            net,
            |e| matches!(e, NetEvent::Stats(s) if s.rtt_ms.is_some()),
        ) else {
            unreachable!()
        };
        assert!(s.connected);
        let rtt = s.rtt_ms.unwrap();
        assert!((0.0..1000.0).contains(&rtt), "{rtt}");
    }
    // Every other voice packet "lost": about 50 %.
    let t0 = Instant::now();
    let mut seq = 0u16;
    while t0.elapsed() < Duration::from_millis(600) {
        j.tx.send_audio(AudioStream::Voice, seq, 0, &[1; 40]);
        seq = seq.wrapping_add(2);
        std::thread::sleep(Duration::from_millis(5));
    }
    // Let the last packets arrive, then take a fresh stats tick.
    std::thread::sleep(Duration::from_millis(100));
    h.events.try_iter().for_each(drop);
    let NetEvent::Stats(s) = next(&h, |e| matches!(e, NetEvent::Stats(_))) else {
        unreachable!()
    };
    assert!(
        (40.0..=60.0).contains(&s.audio_loss_pct),
        "{}",
        s.audio_loss_pct
    );
}

#[test]
fn vanished_joiner_is_detected_and_host_accepts_again() {
    let (h, j, port) = pair();
    j.tx.shared.abort();
    match next(&h, |e| matches!(e, NetEvent::Disconnected { .. })) {
        NetEvent::Disconnected { will_retry, .. } => assert!(!will_retry),
        _ => unreachable!(),
    }
    next(&h, |e| matches!(e, NetEvent::Listening { .. }));
    let j2 = join(port);
    connected(&h);
    connected(&j2);
}

#[test]
fn silent_peer_times_out() {
    let (h, port) = host();
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    let hello = TcpMsg::Hello {
        version: PROTOCOL_VERSION,
        secret: SECRET,
        name: "Silent".into(),
        encoders: vec![],
    };
    s.write_all(&encode_tcp(&hello)).unwrap();
    assert!(matches!(read_tcp(&mut s).unwrap(), TcpMsg::Welcome { .. }));
    connected(&h);
    let t0 = Instant::now();
    match next(&h, |e| matches!(e, NetEvent::Disconnected { .. })) {
        NetEvent::Disconnected { reason, will_retry } => {
            assert!(!will_retry);
            assert!(reason.contains("no answer"), "{reason}");
        }
        _ => unreachable!(),
    }
    assert!(
        t0.elapsed() >= Duration::from_millis(1200),
        "{:?}",
        t0.elapsed()
    );
    next(&h, |e| matches!(e, NetEvent::Listening { .. }));
}

#[test]
fn joiner_leaving_politely_then_rejoining() {
    let (h, j, port) = pair();
    let t0 = Instant::now();
    drop(j);
    assert!(
        t0.elapsed() < Duration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );
    match next(&h, |e| matches!(e, NetEvent::Disconnected { .. })) {
        NetEvent::Disconnected { reason, will_retry } => {
            assert!(!will_retry);
            assert!(reason.contains("left"), "{reason}");
        }
        _ => unreachable!(),
    }
    next(&h, |e| matches!(e, NetEvent::Listening { .. }));
    let j2 = join(port);
    connected(&h);
    connected(&j2);
}

#[test]
fn host_closing_stops_joiner() {
    let (h, j, _) = pair();
    let t0 = Instant::now();
    h.tx.close();
    assert!(
        t0.elapsed() < Duration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );
    match next(&j, |e| matches!(e, NetEvent::Disconnected { .. })) {
        NetEvent::Disconnected { will_retry, .. } => assert!(!will_retry),
        _ => unreachable!(),
    }
    // Calls after closing are ignored.
    h.tx.send(TcpMsg::ShareStopped);
    h.tx.send_audio(AudioStream::Voice, 0, 0, &[]);
}

fn rebind(port: u16) -> Net {
    let deadline = Instant::now() + WAIT;
    loop {
        match host_on(port) {
            Ok(n) => return n,
            Err(e) if Instant::now() < deadline => {
                log::debug!("rebind: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn joiner_reconnects_when_host_returns() {
    let (h, j, port) = pair();
    h.tx.shared.abort();
    match next(&j, |e| matches!(e, NetEvent::Disconnected { .. })) {
        NetEvent::Disconnected { will_retry, .. } => assert!(will_retry),
        _ => unreachable!(),
    }
    drop(h);
    let h2 = rebind(port);
    connected(&h2);
    connected(&j);
    j.tx.send(TcpMsg::Watching { on: false });
    assert!(matches!(
        next(&h2, |e| matches!(e, NetEvent::Msg(_))),
        NetEvent::Msg(TcpMsg::Watching { on: false })
    ));
}

#[test]
fn joiner_gives_up_after_retry_window() {
    let (h, j, _) = pair();
    h.tx.shared.abort();
    let t0 = Instant::now();
    match next(&j, |e| matches!(e, NetEvent::Disconnected { .. })) {
        NetEvent::Disconnected { will_retry, .. } => assert!(will_retry),
        _ => unreachable!(),
    }
    match next(&j, |e| {
        matches!(
            e,
            NetEvent::Disconnected { .. } | NetEvent::Connected { .. }
        )
    }) {
        NetEvent::Disconnected { will_retry, reason } => {
            assert!(!will_retry);
            assert!(reason.contains("reconnect"), "{reason}");
        }
        e => panic!("{e:?}"),
    }
    assert!(t0.elapsed() >= Duration::from_secs(3), "{:?}", t0.elapsed());
}

#[test]
fn nobody_hosting_is_a_plain_error() {
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let j = join(port);
    match next(&j, |e| {
        matches!(e, NetEvent::Error(_) | NetEvent::Connected { .. })
    }) {
        NetEvent::Error(msg) => assert!(
            msg.contains("isn't hosting") || (cfg!(windows) && msg.contains("Tailscale")),
            "{msg}"
        ),
        e => panic!("{e:?}"),
    }
}

#[test]
fn port_in_use_is_a_plain_error() {
    let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let err = host_on(port)
        .err()
        .expect("binding a taken port must fail")
        .to_string();
    assert_eq!(
        err,
        format!("Port {port} is already in use. Is GloorpFlorp already open?")
    );
}

#[test]
fn unavailable_address_is_a_plain_error() {
    // TEST-NET-1 is never assigned to this machine.
    let err = start_with(
        NetConfig {
            role: Role::Host {
                bind_ip: Ipv4Addr::new(192, 0, 2, 1),
            },
            port: 0,
            secret: SECRET,
            name: String::new(),
            encoders: vec![],
        },
        timing(),
    )
    .err()
    .expect("binding a foreign address must fail")
    .to_string();
    assert!(err.contains("Tailscale address isn't available"), "{err}");
}

#[test]
fn sockets_are_released_after_close() {
    let (h, port) = host();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    drop(h);
    // The background threads let go of the port shortly after.
    let h2 = rebind(addr.port());
    next(&h2, |e| matches!(e, NetEvent::Listening { .. }));
}
