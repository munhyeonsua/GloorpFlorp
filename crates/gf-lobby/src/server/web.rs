//! The lobby as a web page, for phones: a connection that starts with an HTTP
//! request line gets one HTTP request served (then the connection closes).
//! `GET /` is the page; it polls `GET /poll` every 2 s, posts `POST /chat`,
//! sends files with `POST /upload` (the raw file as the body) and gets them
//! with `GET /file/<id>` (which resumes with `Range`). Like the app, only
//! Tailscale addresses get here at all; on top of that a request must name
//! the server by a Tailscale name or address (so another website can't read
//! the chat through DNS rebinding), and a `POST` must carry `X-Gloorp` (which
//! a website can't add to a request it makes to another site without the
//! server agreeing, which it doesn't).

use std::io::{BufReader, Seek, SeekFrom, Write};
use std::net::{IpAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use super::{check_upload, open_part, open_shared, store, Ev, MAX_FILE_NAME};
use crate::{new_file_id, read_line, ChatLine, Image, Member, SharedFile};
use crate::{IDLE_TIMEOUT, MAX_CLIENT_LINE};

const PAGE: &str = include_str!("lobby.html");
const MAX_HEADER_LINE: usize = 8 << 10;
const MAX_HEADERS: usize = 64;

/// The answer to `GET /poll`.
#[derive(Serialize)]
pub(super) struct Poll<'a> {
    pub you: String,
    pub members: Vec<Member>,
    /// Ask for lines from this one on next time.
    pub next: u64,
    pub lines: Vec<&'a ChatLine>,
}

pub(super) fn is_request(line: &[u8]) -> bool {
    line.starts_with(b"GET ") || line.starts_with(b"POST ")
}

/// A status and why, for the answer to a request that failed.
type Fail = (u16, String);

pub(super) fn serve(
    r: &mut BufReader<TcpStream>,
    first: &[u8],
    ip: IpAddr,
    tx: &Sender<Ev>,
    files: &Path,
) {
    let Ok(mut w) = r.get_ref().try_clone() else {
        return;
    };
    // A phone may take a while to save a big file.
    let _ = w.set_write_timeout(Some(IDLE_TIMEOUT));
    let result = request(r, first).and_then(|req| route(r, &mut w, &req, ip, tx, files));
    if let Err((status, text)) = result {
        log::debug!("web request from {ip}: {status} {text}");
        let _ = respond(&mut w, status, "text/plain; charset=utf-8", text.as_bytes());
    }
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    host: String,
    length: Option<u64>,
    range: Option<String>,
    /// It has `X-Gloorp` (sent by the page itself).
    ours: bool,
    expect_continue: bool,
}

impl Request {
    fn get(&self, key: &str) -> String {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

fn request(r: &mut BufReader<TcpStream>, first: &[u8]) -> Result<Request, Fail> {
    let bad = || (400, "Bad request.".to_string());
    let first = std::str::from_utf8(first).map_err(|_| bad())?.trim_end();
    let mut parts = first.split(' ');
    let (method, target) = (parts.next().ok_or_else(bad)?, parts.next().ok_or_else(bad)?);
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut req = Request {
        method: method.into(),
        path: decode(path),
        query: query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect(),
        host: String::new(),
        length: None,
        range: None,
        ours: false,
        expect_continue: false,
    };
    let mut buf = Vec::new();
    for _ in 0..MAX_HEADERS {
        let line = read_line(r, &mut buf, MAX_HEADER_LINE)
            .ok()
            .flatten()
            .ok_or_else(bad)?;
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end();
        if line.is_empty() {
            return Ok(req);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(bad());
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => req.host = value.into(),
            "content-length" => req.length = Some(value.parse().map_err(|_| bad())?),
            "range" => req.range = Some(value.into()),
            "x-gloorp" => req.ours = true,
            "expect" => req.expect_continue = value.eq_ignore_ascii_case("100-continue"),
            "transfer-encoding" => return Err((411, "Send a Content-Length.".into())),
            _ => {}
        }
    }
    Err(bad())
}

fn route(
    r: &mut BufReader<TcpStream>,
    w: &mut TcpStream,
    req: &Request,
    ip: IpAddr,
    tx: &Sender<Ev>,
    files: &Path,
) -> Result<(), Fail> {
    if !host_ok(&req.host) {
        return Err((
            403,
            "Open the lobby by its Tailscale name or address.".into(),
        ));
    }
    if req.method == "POST" && !req.ours {
        return Err((403, "Use the lobby's own page.".into()));
    }
    let lost = |_| (500, "The lobby server isn't running properly.".to_string());
    let gone = |_| (503, "The lobby server didn't answer.".to_string());
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => respond(w, 200, "text/html; charset=utf-8", PAGE.as_bytes()).map_err(io),
        ("GET", "/poll") => {
            let (reply, answer) = crossbeam_channel::bounded(1);
            let since = req.get("since").parse().unwrap_or(0);
            tx.send(Ev::WebPoll {
                ip,
                name: req.get("name"),
                since,
                reply,
            })
            .map_err(lost)?;
            let body = answer.recv_timeout(Duration::from_secs(10)).map_err(gone)?;
            respond(w, 200, "application/json", &body).map_err(io)
        }
        ("POST", "/chat") => {
            #[derive(Deserialize)]
            struct Post {
                #[serde(default)]
                text: String,
                #[serde(default)]
                image: Option<Image>,
            }
            let len = req.length.unwrap_or(0);
            if len > MAX_CLIENT_LINE as u64 {
                return Err((413, "That message is too big.".into()));
            }
            let mut body = vec![0; len as usize];
            std::io::Read::read_exact(r, &mut body).map_err(io)?;
            let post: Post = serde_json::from_slice(&body)
                .map_err(|_| (400, "That message can't be read.".to_string()))?;
            let (reply, answer) = crossbeam_channel::bounded(1);
            tx.send(Ev::WebChat {
                ip,
                name: req.get("name"),
                text: post.text,
                image: post.image,
                reply,
            })
            .map_err(lost)?;
            answer
                .recv_timeout(Duration::from_secs(10))
                .map_err(gone)?
                .map_err(|text| (400, text))?;
            respond(w, 200, "application/json", b"{}").map_err(io)
        }
        ("POST", "/upload") => {
            let size = req
                .length
                .ok_or_else(|| (411, "Send a Content-Length.".to_string()))?;
            let id = new_file_id();
            check_upload(&id, size).map_err(|text| (413, text))?;
            let (file, _) = open_part(files, &id, size).map_err(|text| (507, text))?;
            if req.expect_continue {
                w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").map_err(io)?;
            }
            store(r, file, files, &id, size).map_err(|text| (500, text))?;
            let name = super::clean(&req.get("file"), MAX_FILE_NAME);
            let name = if name.is_empty() { "file".into() } else { name };
            log::info!("file {id} ({name}, {size} bytes) stored from {ip} (web)");
            let file = SharedFile { id, name, size };
            let who = req.get("name");
            tx.send(Ev::Shared { ip, who, file }).map_err(lost)?;
            respond(w, 200, "application/json", b"{}").map_err(io)
        }
        ("GET", path) if path.starts_with("/file/") => download(w, req, files),
        _ => Err((404, "Not found.".into())),
    }
}

fn io(e: std::io::Error) -> Fail {
    (500, format!("The connection broke off ({e})."))
}

/// File `/file/<id>` as a download named `?name=`; `Range: bytes=N-` gives
/// the rest from byte N on (how browsers resume).
fn download(w: &mut TcpStream, req: &Request, files: &Path) -> Result<(), Fail> {
    let id = &req.path["/file/".len()..];
    let (mut f, size) = open_shared(files, id).map_err(|text| (404, text))?;
    let from = req
        .range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|n| n.parse::<u64>().ok())
        .filter(|&n| n < size);
    let name = req.get("name");
    let mut head = format!(
        "Content-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\n\
         Content-Disposition: attachment; filename=\"download\"; filename*=UTF-8''{}\r\n",
        encode(if name.is_empty() { "download" } else { &name })
    );
    let status = match from {
        Some(n) => {
            f.seek(SeekFrom::Start(n)).map_err(io)?;
            head += &format!("Content-Range: bytes {n}-{}/{size}\r\n", size - 1);
            206
        }
        None => 200,
    };
    let len = size - from.unwrap_or(0);
    write_head(w, status, &head, len).map_err(io)?;
    std::io::copy(&mut f, w).map_err(io)?;
    Ok(())
}

fn respond(w: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "Content-Type: {content_type}\r\nX-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: default-src 'self'; img-src 'self' data: blob:; \
         style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'\r\n"
    );
    write_head(w, status, &head, body.len() as u64)?;
    w.write_all(body)
}

fn write_head(w: &mut TcpStream, status: u16, head: &str, len: u64) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Internal Server Error",
    };
    let all = format!(
        "HTTP/1.1 {status} {reason}\r\n{head}Content-Length: {len}\r\nCache-Control: no-store\r\n\
         Connection: close\r\n\r\n"
    );
    w.write_all(all.as_bytes())
}

/// The `Host` a browser sends for the lobby itself: an address, a Tailscale
/// (MagicDNS) name like `gloorp-lobby` or `gloorp-lobby.tail1234.ts.net`, or
/// `localhost`. Not some other site's name pointed at this address.
fn host_ok(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => {
            return v6
                .split(']')
                .next()
                .is_some_and(|a| a.parse::<IpAddr>().is_ok())
        }
        None => host.rsplit_once(':').map_or(host, |(n, _)| n),
    }
    .to_ascii_lowercase();
    !name.is_empty()
        && (name.parse::<IpAddr>().is_ok() || !name.contains('.') || name.ends_with(".ts.net"))
}

/// Undoes URL encoding (`%41` and `+` for a space).
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// URL-encodes everything but letters, digits and `-._~`.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_lobby_s_own_names() {
        for ok in [
            "gloorp-lobby:47900",
            "gloorp-lobby",
            "100.67.196.24:47900",
            "gloorp-lobby.tail1234.ts.net:47900",
            "localhost:47900",
            "[fd7a:115c:a1e0::1]:47900",
        ] {
            assert!(host_ok(ok), "{ok}");
        }
        for bad in ["evil.example.com:47900", "", "ts.net.evil.com", "[evil]:1"] {
            assert!(!host_ok(bad), "{bad}");
        }
    }

    #[test]
    fn url_coding() {
        assert_eq!(decode("big%20file+(2).zip%"), "big file (2).zip%");
        assert_eq!(decode("%E2%86%92%zz"), "→%zz");
        assert_eq!(encode("game clip ü.mp4"), "game%20clip%20%C3%BC.mp4");
    }
}
