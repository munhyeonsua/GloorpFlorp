//! A real server and real clients on 127.0.0.1 (told apart by name).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gf_lobby::client::{Client, State};
use gf_lobby::server::{serve, Config};
use gf_lobby::{Image, Presence, ToClient, ToServer, MAX_CLIENT_LINE, PROTOCOL};

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
        image: None,
    });
    wait_for(&b, "Alice's message", |s| has_chat(s, "Alice", "hi Bob"));
    wait_for(&a, "her own message", |s| has_chat(s, "Alice", "hi Bob"));

    // A picture goes along; junk is refused with a note.
    let png = Image(b"\x89PNG\r\n\x1a\nnot really".to_vec());
    a.send(ToServer::Chat {
        text: String::new(),
        image: Some(png.clone()),
    });
    wait_for(&b, "Alice's picture", |s| {
        s.chat.iter().any(|c| c.image.as_ref() == Some(&png))
    });
    a.send(ToServer::Chat {
        text: "x".into(),
        image: Some(Image(b"GIF89a".to_vec())),
    });
    let ev = a.events.recv_timeout(WAIT).unwrap();
    assert!(
        matches!(&ev, ToClient::Error { text } if text.contains("PNG or JPEG")),
        "{ev:?}"
    );

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
        code: "K7QM-2XPA-9TD".into(),
        accept: false,
    });
    let ev = a.events.recv_timeout(WAIT).unwrap();
    assert_eq!(
        ev,
        ToClient::Replied {
            from: bob.clone(),
            code: "K7QM-2XPA-9TD".into(),
            accept: false
        }
    );
    a.send(ToServer::Cancel {
        to: bob.clone(),
        code: "K7QM-2XPA-9TD".into(),
    });
    let ev = b.events.recv_timeout(WAIT).unwrap();
    assert_eq!(
        ev,
        ToClient::Cancelled {
            from: alice.clone(),
            code: "K7QM-2XPA-9TD".into()
        }
    );
    // An invite nobody can get comes back, so the room doesn't wait.
    a.send(ToServer::Invite {
        to: "100.64.0.9".into(),
        code: "AAAA-BBBB-CCC".into(),
    });
    let ev = a.events.recv_timeout(WAIT).unwrap();
    assert!(
        matches!(&ev, ToClient::InviteFailed { to, code, .. }
            if to == "100.64.0.9" && code == "AAAA-BBBB-CCC"),
        "{ev:?}"
    );
    assert!(a.events.try_recv().is_err() && b.events.try_recv().is_err());

    // A line that doesn't parse is skipped, and so is a second Hello; an
    // oversize line ends only that connection.
    let mut raw = TcpStream::connect(addr).unwrap();
    raw.write_all(b"{\"t\":\"chat\",\"text\":\"before hello\"}\n")
        .unwrap();
    let hello = |name: &str| format!("{{\"t\":\"hello\",\"v\":{PROTOCOL},\"name\":\"{name}\"}}\n");
    raw.write_all(format!("{}not json\n{{\"t\":\"nope\"}}\n", hello("Mallory")).as_bytes())
        .unwrap();
    raw.write_all(hello("Again").as_bytes()).unwrap();
    raw.write_all(b"{\"t\":\"chat\",\"text\":\"after garbage\"}\n")
        .unwrap();
    let sa = wait_for(&a, "Mallory's message", |s| {
        has_chat(s, "Mallory", "after garbage")
    });
    assert!(!sa.chat.iter().any(|c| c.text == "before hello"));
    assert_eq!(state_of(&sa, "Again"), None);
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
        image: None,
    });
    wait_for(&a, "the server still works", |s| {
        has_chat(s, "Alice", "still here")
    });
    drop(a);

    // A restarted server on the same folder has the history and members.
    let addr = start_server(&dir);
    let c = client(addr, "Carol");
    let sc = wait_for(&c, "the history", |s| {
        s.chat.len() == 4 && state_of(s, "Carol") == Some(Presence::Lobby)
    });
    let texts: Vec<&str> = sc.chat.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, ["hi Bob", "", "after garbage", "still here"]);
    assert_eq!(sc.chat[1].image.as_ref(), Some(&png), "the picture is kept");
    assert!(sc.chat.iter().all(|c| c.at > 0), "{:?}", sc.chat);
    for name in ["Alice", "Bob", "Mallory"] {
        assert_eq!(state_of(&sc, name), Some(Presence::Offline), "{name}");
    }

    // Too many messages at once: some are refused, with a note.
    for i in 0..10 {
        c.send(ToServer::Chat {
            text: format!("spam {i}"),
            image: None,
        });
    }
    let ev = c.events.recv_timeout(WAIT).unwrap();
    assert!(matches!(ev, ToClient::Error { .. }), "{ev:?}");
    let sc = wait_for(&c, "the allowed messages", |s| {
        has_chat(s, "Carol", "spam 0")
    });
    assert!(
        sc.chat.len() < 4 + 10,
        "{} messages got through",
        sc.chat.len() - 4
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

#[test]
fn files_go_through_the_lobby_and_resume() {
    use gf_lobby::client::{download, upload, Transfer};
    use gf_lobby::{new_file_id, MAX_FILE};

    let dir = std::env::temp_dir().join(format!("gf-lobby-test-f-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let addr = start_server(&dir.join("server"));
    let a = client(addr, "Alice");
    wait_for(&a, "connected", |s| s.connected);
    let quiet: gf_lobby::client::ChangeCallback = Arc::new(|| {});
    let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
    let src = dir.join("big file.bin");
    std::fs::write(&src, &bytes).unwrap();
    let addr = addr.to_string();

    // Up, shown in the chat, and down again.
    let id = new_file_id();
    let t = Transfer::default();
    upload(&addr, "Alice", &id, &src, &t, &quiet).unwrap();
    assert_eq!(t.done.load(std::sync::atomic::Ordering::Relaxed), 3_000_000);
    let s = wait_for(&a, "the file in the chat", |s| {
        s.chat.iter().any(|c| c.file.is_some())
    });
    let line = s.chat.iter().find(|c| c.file.is_some()).unwrap();
    let shared = line.file.as_ref().unwrap();
    assert_eq!(
        (line.name.as_str(), shared.name.as_str(), shared.size),
        ("Alice", "big file.bin", 3_000_000)
    );
    let part = dir.join("got.part");
    download(&addr, &shared.id, &part, &Transfer::default(), &quiet).unwrap();
    assert!(std::fs::read(&part).unwrap() == bytes);

    // A broken-off download goes on from its part file.
    std::fs::write(&part, &bytes[..1_000_000]).unwrap();
    let t = Transfer::default();
    download(&addr, &shared.id, &part, &t, &quiet).unwrap();
    assert!(std::fs::read(&part).unwrap() == bytes);
    assert_eq!(t.done.load(std::sync::atomic::Ordering::Relaxed), 3_000_000);

    // A broken-off upload goes on from what the server has.
    let id = new_file_id();
    let files = dir.join("server").join("files");
    std::fs::write(files.join(format!("{id}.part")), &bytes[..1_234_567]).unwrap();
    upload(&addr, "Alice", &id, &src, &Transfer::default(), &quiet).unwrap();
    assert!(std::fs::read(files.join(&id)).unwrap() == bytes);

    // Refusals come back in plain words.
    let gone = download(
        &addr,
        "0123456789",
        &dir.join("x.part"),
        &Transfer::default(),
        &quiet,
    );
    assert!(gone.unwrap_err().contains("isn't on the lobby server"));
    let ask = |line: String| {
        let mut raw = TcpStream::connect(&addr).unwrap();
        raw.write_all(line.as_bytes()).unwrap();
        raw.set_read_timeout(Some(WAIT)).unwrap();
        let mut reply = String::new();
        let _ = raw.read_to_string(&mut reply);
        reply
    };
    let too_big = format!(
        "{{\"t\":\"upload\",\"id\":\"0123456789\",\"name\":\"x\",\"size\":{},\"who\":\"A\"}}\n",
        MAX_FILE + 1
    );
    assert!(ask(too_big).contains("too big"));
    let sneaky = "{\"t\":\"download\",\"id\":\"../members.json\",\"from\":0}\n".to_string();
    assert!(ask(sneaky).contains("isn't on the lobby server"));
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}

/// One HTTP request; the answer's head (status line and headers) and body.
fn http(addr: SocketAddr, request: &str, body: &[u8]) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let mut all = Vec::new();
    let _ = s.read_to_end(&mut all);
    let end = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (
        String::from_utf8_lossy(&all[..end]).into_owned(),
        all[end + 4..].to_vec(),
    )
}

#[test]
fn web_page_for_phones() {
    let dir = std::env::temp_dir().join(format!("gf-lobby-test-w-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let addr = start_server(&dir);
    let a = client(addr, "Alice");
    wait_for(&a, "connected", |s| s.connected);
    let host = "Host: gloorp-lobby:47900\r\n";
    let get = |path: &str, extra: &str| {
        http(
            addr,
            &format!("GET {path} HTTP/1.1\r\n{host}{extra}\r\n"),
            b"",
        )
    };
    let post = |path: &str, extra: &str, body: &[u8]| {
        let head = format!(
            "POST {path} HTTP/1.1\r\n{host}{extra}Content-Length: {}\r\n\r\n",
            body.len()
        );
        http(addr, &head, body)
    };

    let (head, page) = get("/", "");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(page.starts_with(b"<!doctype html>"));

    // Polling makes the phone a member the app sees in the lobby.
    let (head, body) = get("/poll?name=My+Phone&since=0", "");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let poll: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(poll["you"], "127.0.0.1/My Phone");
    wait_for(&a, "the phone in the lobby", |s| {
        state_of(s, "My Phone") == Some(Presence::Lobby)
    });

    // Chat both ways; a POST without the page's header is turned away.
    let msg = br#"{"text":"hi from the phone"}"#;
    let (head, _) = post("/chat?name=My%20Phone", "", msg);
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    let (head, _) = post("/chat?name=My%20Phone", "X-Gloorp: 1\r\n", msg);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    wait_for(&a, "the phone's message", |s| {
        has_chat(s, "My Phone", "hi from the phone")
    });
    a.send(ToServer::Chat {
        text: "hi from the PC".into(),
        image: None,
    });
    let since = poll["next"].as_u64().unwrap();
    let until = Instant::now() + WAIT;
    loop {
        let (_, body) = get(&format!("/poll?name=My+Phone&since={since}"), "");
        let poll: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let texts: Vec<&str> = poll["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|l| l["text"].as_str())
            .collect();
        if texts == ["hi from the phone", "hi from the PC"] {
            break;
        }
        assert!(Instant::now() < until, "{texts:?}");
        std::thread::sleep(Duration::from_millis(20));
    }

    // A file up from the phone shows in the app, and comes down whole or
    // from where a broken-off download stopped.
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let (head, _) = post(
        "/upload?name=My%20Phone&file=clip%20(1).mp4",
        "X-Gloorp: 1\r\n",
        &data,
    );
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let s = wait_for(&a, "the phone's file", |s| {
        s.chat
            .iter()
            .any(|c| c.name == "My Phone" && c.file.is_some())
    });
    let file = s.chat.iter().find_map(|c| c.file.clone()).unwrap();
    assert_eq!((file.name.as_str(), file.size), ("clip (1).mp4", 200_000));
    let (head, body) = get(&format!("/file/{}?name=clip%20(1).mp4", file.id), "");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.contains("filename*=UTF-8''clip%20%281%29.mp4"),
        "{head}"
    );
    assert!(body == data);
    let (head, body) = get(&format!("/file/{}", file.id), "Range: bytes=150000-\r\n");
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert!(
        head.contains("Content-Range: bytes 150000-199999/200000"),
        "{head}"
    );
    assert!(body == data[150_000..]);

    // Another website's name pointed at the lobby gets nothing.
    let (head, _) = http(
        addr,
        "GET /poll?name=x HTTP/1.1\r\nHost: evil.example.com\r\n\r\n",
        b"",
    );
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}
