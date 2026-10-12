//! The optional lobby: an always-on server inside the tailnet that carries
//! text chat, who is around, and room invites. Rooms stay 1:1 between the
//! two PCs (gf-net); the lobby never sees video, audio or the room
//! connection, only the room code inside an invite.
//!
//! Protocol: one JSON object per line over TCP, tagged by `"t"`. The client
//! sends `Hello` first; the server answers `Welcome`, the recent chat (as
//! `Chat` lines) and the member list, then sends `Members` again whenever
//! someone's state changes. A line longer than the reader's cap ends the
//! connection; a line that doesn't parse is ignored (a newer version may send
//! kinds this one doesn't know).
//!
//! Members are identified by their connection's Tailscale address (Tailscale
//! already authenticates every device) plus the name they send. Connections
//! from the server's own machine (loopback, development only) are told apart
//! by name, so two copies of the app can use one PC.
//!
//! Keepalive: the client sends `Ping` every 20 s and the server answers
//! `Pong`; both drop a connection that sent nothing for 60 s.

pub mod client;
pub mod server;

use std::io::{self, BufRead, Read};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Bumped whenever the protocol changes incompatibly.
pub const PROTOCOL: u32 = 1;
pub const DEFAULT_PORT: u16 = 47900;
/// Longest name, in characters.
pub const MAX_NAME: usize = 40;
/// Longest chat message, in characters.
pub const MAX_TEXT: usize = 1000;
/// Chat messages kept for (and sent to) newcomers.
pub const HISTORY: usize = 200;
/// Longest line the server reads from a client.
pub const MAX_CLIENT_LINE: usize = 16 * 1024;
/// Longest line the client reads from the server (the member list).
pub const MAX_SERVER_LINE: usize = 1 << 20;
pub const PING_EVERY: Duration = Duration::from_secs(20);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Client to server.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToServer {
    Hello {
        v: u32,
        name: String,
    },
    Chat {
        text: String,
    },
    /// Ask member `to` to join my room.
    Invite {
        to: String,
        code: String,
    },
    /// Answer member `to`'s invite.
    Reply {
        to: String,
        accept: bool,
    },
    Ping,
}

/// Server to client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToClient {
    /// `you`: my member id.
    Welcome {
        v: u32,
        you: String,
    },
    Members {
        list: Vec<Member>,
    },
    Chat(ChatLine),
    Invited {
        from: String,
        name: String,
        code: String,
    },
    Replied {
        from: String,
        accept: bool,
    },
    /// A problem in plain words (wrong version, sending too fast, ...).
    Error {
        text: String,
    },
    Pong,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub state: Presence,
}

/// In the order the member list is sorted.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// GloorpFlorp is open and connected to the lobby.
    Lobby,
    /// The PC is on in Tailscale, GloorpFlorp isn't in the lobby.
    Online,
    Offline,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ChatLine {
    /// Member id.
    pub from: String,
    pub name: String,
    pub text: String,
}

/// `msg` as one line.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(msg).expect("lobby messages always serialize");
    line.push(b'\n');
    line
}

/// Reads one line (without its `\n`) of at most `max` bytes, so a peer can't
/// make us buffer without end. `Ok(None)` at the end of the stream; an
/// `InvalidData` error for a longer line.
pub fn read_line<'a>(
    r: &mut impl BufRead,
    buf: &'a mut Vec<u8>,
    max: usize,
) -> io::Result<Option<&'a [u8]>> {
    buf.clear();
    let n = Read::take(&mut *r, max as u64 + 1).read_until(b'\n', buf)?;
    if buf.last() != Some(&b'\n') {
        return if n > max {
            Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"))
        } else {
            // The end of the stream (possibly in the middle of a line).
            Ok(None)
        };
    }
    buf.pop();
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_capped() {
        let mut buf = Vec::new();
        let mut r = io::Cursor::new(b"12345\n123456\n1234567\n12".to_vec());
        assert_eq!(read_line(&mut r, &mut buf, 6).unwrap(), Some(&b"12345"[..]));
        assert_eq!(
            read_line(&mut r, &mut buf, 6).unwrap(),
            Some(&b"123456"[..])
        );
        assert!(read_line(&mut r, &mut buf, 6).is_err());

        let mut r = io::Cursor::new(b"12".to_vec());
        assert_eq!(read_line(&mut r, &mut buf, 6).unwrap(), None);
        let line = encode(&ToClient::Chat(ChatLine {
            from: "100.64.0.1".into(),
            name: "A".into(),
            text: "hi".into(),
        }));
        assert_eq!(
            String::from_utf8(line).unwrap(),
            "{\"t\":\"chat\",\"from\":\"100.64.0.1\",\"name\":\"A\",\"text\":\"hi\"}\n"
        );
    }
}
