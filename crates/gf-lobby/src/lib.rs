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
//! Invites: `Invite` reaches the member as `Invited`; they answer with
//! `Reply` (`Replied`), or the inviter takes it back with `Cancel`
//! (`Cancelled`). All carry the room code, so an answer belongs to exactly one
//! invite. An invite that can't be delivered comes back as `InviteFailed`.
//!
//! Pictures: a chat line may carry one PNG or JPEG file (base64, at most
//! [`MAX_IMAGE`] bytes). Only the newest [`IMAGE_HISTORY`] of them are kept
//! for newcomers; older lines keep their text and say there was a picture.
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

use std::fmt;
use std::io::{self, BufRead, Read};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::prelude::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Bumped whenever the protocol changes incompatibly.
pub const PROTOCOL: u32 = 2;
pub const DEFAULT_PORT: u16 = 47900;
/// Longest name, in characters.
pub const MAX_NAME: usize = 40;
/// Longest chat message, in characters.
pub const MAX_TEXT: usize = 1000;
/// Chat messages kept for (and sent to) newcomers.
pub const HISTORY: usize = 200;
/// Longest picture file (PNG or JPEG bytes) in a chat message.
pub const MAX_IMAGE: usize = 1 << 20;
/// Chat lines whose picture is kept (each can be a megabyte, and every
/// newcomer gets the whole history); older lines lose theirs.
pub const IMAGE_HISTORY: usize = 10;
/// Longest line the server reads from a client (a picture in base64 plus
/// its text).
pub const MAX_CLIENT_LINE: usize = 2 << 20;
/// Longest line the client reads from the server (a picture, the member list).
pub const MAX_SERVER_LINE: usize = 2 << 20;
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<Image>,
    },
    /// Ask member `to` to join my room.
    Invite {
        to: String,
        code: String,
    },
    /// Answer member `to`'s invite to room `code`.
    Reply {
        to: String,
        code: String,
        accept: bool,
    },
    /// Take back my invite to member `to` (my room closed before they
    /// answered).
    Cancel {
        to: String,
        code: String,
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
        code: String,
        accept: bool,
    },
    /// `from` took back their invite to room `code`.
    Cancelled {
        from: String,
        code: String,
    },
    /// My invite to `to` didn't go out; `text` says why in plain words.
    InviteFailed {
        to: String,
        code: String,
        text: String,
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
    /// When the server got it, in Unix seconds (0: not known).
    #[serde(default)]
    pub at: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
}

/// A picture file (PNG or JPEG) in a chat message; base64 on the wire.
#[derive(Clone, PartialEq, Eq)]
pub struct Image(pub Vec<u8>);

impl Image {
    /// A PNG or JPEG of at most [`MAX_IMAGE`] bytes (the server checks
    /// before passing it on, so nobody gets junk to decode).
    pub fn ok(&self) -> bool {
        let b = &self.0;
        b.len() <= MAX_IMAGE
            && (b.starts_with(b"\x89PNG\r\n\x1a\n") || b.starts_with(&[0xFF, 0xD8, 0xFF]))
    }
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Image({} bytes)", self.0.len())
    }
}

impl Serialize for Image {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&BASE64_STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Image {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        BASE64_STANDARD
            .decode(text)
            .map(Image)
            .map_err(serde::de::Error::custom)
    }
}

/// Drops the line's picture; the text says there was one.
pub fn forget_image(line: &mut ChatLine) {
    if line.image.take().is_some() {
        line.text = format!("{} (picture)", line.text).trim().to_string();
    }
}

/// Now, in Unix seconds.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
            at: 1_790_000_000,
            text: "hi".into(),
            image: None,
        }));
        assert_eq!(
            String::from_utf8(line).unwrap(),
            "{\"t\":\"chat\",\"from\":\"100.64.0.1\",\"name\":\"A\",\"at\":1790000000,\"text\":\"hi\"}\n"
        );
        // Saved before messages had a time.
        let old: ChatLine =
            serde_json::from_str("{\"from\":\"100.64.0.1\",\"name\":\"A\",\"text\":\"hi\"}")
                .unwrap();
        assert_eq!(old.at, 0);
    }

    #[test]
    fn pictures_travel_as_base64() {
        let png = Image(b"\x89PNG\r\n\x1a\n\0\0".to_vec());
        let msg = ToServer::Chat {
            text: String::new(),
            image: Some(png.clone()),
        };
        let line = encode(&msg);
        assert_eq!(
            String::from_utf8(line.clone()).unwrap(),
            "{\"t\":\"chat\",\"text\":\"\",\"image\":\"iVBORw0KGgoAAA==\"}\n"
        );
        assert_eq!(serde_json::from_slice::<ToServer>(&line).unwrap(), msg);
        assert!(png.ok());
        assert!(Image(vec![0xFF, 0xD8, 0xFF, 0xE0]).ok());
        assert!(!Image(b"GIF89a".to_vec()).ok());
        assert!(!Image(vec![0xFF; MAX_IMAGE + 1]).ok());

        let mut line = ChatLine {
            from: "a".into(),
            name: "A".into(),
            at: 1,
            text: "look".into(),
            image: Some(png),
        };
        forget_image(&mut line);
        assert_eq!(
            (line.image.is_none(), line.text.as_str()),
            (true, "look (picture)")
        );
        line.text.clear();
        line.image = Some(Image(vec![0xFF, 0xD8, 0xFF]));
        forget_image(&mut line);
        assert_eq!(line.text, "(picture)");
    }
}
