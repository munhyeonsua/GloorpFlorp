//! A real server and real clients on 127.0.0.1 (told apart by name).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gf_lobby::client::{Client, State};
use gf_lobby::server::{serve, Config};
use gf_lobby::{ChatLine, Presence, ToClient, ToServer, MAX_CLIENT_LINE};

const WAIT: Duration = Duration::from_secs(10);

fn start_server(dir: &Path) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = Config {
        data_dir: dir.to_path_buf(),
        tailscale_socket: None,
    };
    // Runs until the test process ends.
    std::thread::spawn(move || serve(listener, cfg));
    addr
}

fn client(addr: SocketAddr, name: &str) -> Client {
    Client::start(&addr.to_string(), name, Arc::new(|| {}))
}

fn wait_for(c: &Client, what: &str, ok: impl Fn(&State) -> bool) -> State {
    let until = Instant::now() + WAIT;
    loop {
        let s = c.state();
        if ok(&s) {
            return s;
        }
        assert!(
            Instant::now() < until,
            "timed out waiting for {what}: {s:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn id_of(s: &State, name: &str) -> String {
    s.members
        .iter()
        .find(|m| m.name == name)
        .unwrap()
        .id
        .clone()
}

fn state_of(s: &State, name: &str) -> Option<Presence> {
    s.members.iter().find(|m| m.name == name).map(|m| m.state)
}

fn has_chat(s: &State, name: &str, text: &str) -> bool {
    s.chat.iter().any(|c| c.name == name && c.text == text)
}

#[test]
fn chat_invites_presence_and_bad_input() {
    let dir = std::env::temp_dir().join(format!("gf-lobby-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let addr = start_server(&dir);

    let a = client(addr, "Alice");
    let b = client(addr, "Bob");
    let both_here = |s: &State| {
        s.connected
            && state_of(s, "Alice") == Some(Presence::Lobby)
            && state_of(s, "Bob") == Some(Presence::Lobby)
    };
    let sa = wait_for(&a, "both in the lobby", both_here);
    let sb = wait_for(&b, "both in the lobby", both_here);
    let (alice, bob) = (id_of(&sa, "Alice"), id_of(&sa, "Bob"));
    assert_eq!(
        (sa.me.as_str(), sb.me.as_str()),
        (alice.as_str(), bob.as_str())
    );
    assert_ne!(alice, bob);

    // Chat reaches everyone, cleaned up.
    a.send(ToServer::Chat {
        text: " hi\nBob ".into(),
    });
    wait_for(&b, "Alice's message", |s| has_chat(s, "Alice", "hi Bob"));
    wait_for(&a, "her own message", |s| has_chat(s, "Alice", "hi Bob"));

    // An invite and its answer go to the right person only.
    a.send(ToServer::Invite {
        to: bob.clone(),
        code: "K7QM-2XPA-9TD".into(),
    });
    let ev = b.events.recv_timeout(WAIT).unwrap();
    assert_eq!(
        ev,
        ToClient::Invited {
            from: alice.clone(),
            name: "Alice".into(),
            code: "K7QM-2XPA-9TD".into()
        }
    );
    b.send(ToServer::Reply {
        to: alice.clone(),
        accept: false,
    });
    let ev = a.events.recv_timeout(WAIT).unwrap();
    assert_eq!(
        ev,
        ToClient::Replied {
            from: bob.clone(),
            accept: false
        }
    );
    assert!(a.events.try_recv().is_err() && b.events.try_recv().is_err());

    // A line that doesn't parse is skipped; an oversize line ends only that
    // connection.
    let mut raw = TcpStream::connect(addr).unwrap();
    raw.write_all(b"{\"t\":\"chat\",\"text\":\"before hello\"}\n")
        .unwrap();
    raw.write_all(b"{\"t\":\"hello\",\"v\":1,\"name\":\"Mallory\"}\nnot json\n{\"t\":\"nope\"}\n")
        .unwrap();
    raw.write_all(b"{\"t\":\"chat\",\"text\":\"after garbage\"}\n")
        .unwrap();
    let sa = wait_for(&a, "Mallory's message", |s| {
        has_chat(s, "Mallory", "after garbage")
    });
    assert!(!sa.chat.iter().any(|c| c.text == "before hello"));
    // The server may close (or reset) the connection before all of it is sent.
    let _ = raw.write_all(&vec![b'x'; MAX_CLIENT_LINE + 10]);
    raw.set_read_timeout(Some(WAIT)).unwrap();
    let _ = raw.read_to_end(&mut Vec::new());
    wait_for(&a, "Mallory gone", |s| {
        state_of(s, "Mallory") == Some(Presence::Offline)
    });

    // Bob leaves; Alice sees it, and the server keeps working.
    drop(b);
    wait_for(&a, "Bob gone", |s| {
        state_of(s, "Bob") == Some(Presence::Offline)
    });
    a.send(ToServer::Chat {
        text: "still here".into(),
    });
    wait_for(&a, "the server still works", |s| {
        has_chat(s, "Alice", "still here")
    });
    drop(a);

    // A restarted server on the same folder has the history and members.
    let addr = start_server(&dir);
    let c = client(addr, "Carol");
    let sc = wait_for(&c, "the history", |s| {
        s.chat.len() == 3 && state_of(s, "Carol") == Some(Presence::Lobby)
    });
    let texts: Vec<&str> = sc.chat.iter().map(|c: &ChatLine| c.text.as_str()).collect();
    assert_eq!(texts, ["hi Bob", "after garbage", "still here"]);
    for name in ["Alice", "Bob", "Mallory"] {
        assert_eq!(state_of(&sc, name), Some(Presence::Offline), "{name}");
    }

    // Too many messages at once: some are refused, with a note.
    for i in 0..10 {
        c.send(ToServer::Chat {
            text: format!("spam {i}"),
        });
    }
    let ev = c.events.recv_timeout(WAIT).unwrap();
    assert!(matches!(ev, ToClient::Error { .. }), "{ev:?}");
    let sc = wait_for(&c, "the allowed messages", |s| {
        has_chat(s, "Carol", "spam 0")
    });
    assert!(
        sc.chat.len() < 3 + 10,
        "{} messages got through",
        sc.chat.len() - 3
    );
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wrong_version_is_turned_away() {
    let dir = std::env::temp_dir().join(format!("gf-lobby-test-v-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let addr = start_server(&dir);
    let mut raw = TcpStream::connect(addr).unwrap();
    raw.write_all(b"{\"t\":\"hello\",\"v\":999,\"name\":\"Old\"}\n")
        .unwrap();
    raw.set_read_timeout(Some(WAIT)).unwrap();
    let mut reply = String::new();
    raw.read_to_string(&mut reply).unwrap();
    assert!(reply.contains("different version"), "{reply}");
    let _ = std::fs::remove_dir_all(&dir);
}
