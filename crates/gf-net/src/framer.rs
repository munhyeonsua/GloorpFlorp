//! Incremental TCP message reader that survives read timeouts.
//!
//! `gf_proto::wire::read_tcp` uses `read_exact`, which loses partially read
//! bytes when a read times out. The connection reader polls with a short
//! timeout (to notice a dead peer or a local close), so it keeps its partial
//! message here between calls instead.

use std::io::{self, Read};

use gf_proto::wire::{decode_tcp_body, TcpMsg, WireError};

pub(crate) struct Framer {
    max: usize,
    hdr: [u8; 4],
    hdr_got: usize,
    body: Vec<u8>,
    body_got: usize,
    in_body: bool,
    progressed: bool,
}

impl Framer {
    /// `max` is the largest accepted message body in bytes.
    pub fn new(max: usize) -> Self {
        Framer {
            max,
            hdr: [0; 4],
            hdr_got: 0,
            body: Vec::new(),
            body_got: 0,
            in_body: false,
            progressed: false,
        }
    }

    /// Reads until one whole message is available (`Ok(Some)`) or the reader
    /// times out / would block (`Ok(None)`, partial bytes are kept).
    /// End of stream is an `UnexpectedEof` I/O error.
    pub fn poll(&mut self, r: &mut impl Read) -> Result<Option<TcpMsg>, WireError> {
        loop {
            if self.in_body && self.body_got == self.body.len() {
                let msg = decode_tcp_body(&self.body);
                self.in_body = false;
                self.hdr_got = 0;
                self.body = Vec::new();
                self.body_got = 0;
                return msg.map(Some);
            }
            let res = if self.in_body {
                r.read(&mut self.body[self.body_got..])
            } else {
                r.read(&mut self.hdr[self.hdr_got..])
            };
            match res {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
                Ok(n) => {
                    self.progressed = true;
                    if self.in_body {
                        self.body_got += n;
                    } else {
                        self.hdr_got += n;
                        if self.hdr_got == 4 {
                            let len = u32::from_le_bytes(self.hdr) as usize;
                            if len > self.max {
                                return Err(WireError::TooLarge(len));
                            }
                            self.body = vec![0; len];
                            self.in_body = true;
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// True if any bytes arrived since the last call (a slow, large message
    /// still shows the peer is alive).
    pub fn take_progress(&mut self) -> bool {
        std::mem::take(&mut self.progressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gf_proto::wire::{encode_tcp, MAX_TCP_MSG};
    use gf_proto::{VideoCodec, VideoFrame};

    /// Hands out the input in fixed-size chunks with a timeout between each.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
        timeout_next: bool,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.timeout_next = !self.timeout_next;
            if !self.timeout_next {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let n = buf.len().min(self.chunk).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn msgs() -> Vec<TcpMsg> {
        vec![
            TcpMsg::Ping { nonce: 7 },
            TcpMsg::Video(VideoFrame {
                codec: VideoCodec::Hevc,
                keyframe: true,
                sent_us: 99,
                data: (0..5000u32).map(|i| i as u8).collect(),
            }),
            TcpMsg::Watching { on: true },
            TcpMsg::Bye,
        ]
    }

    #[test]
    fn survives_timeouts_mid_message() {
        for chunk in [1, 3, 4, 7, 1000] {
            let data: Vec<u8> = msgs().iter().flat_map(encode_tcp).collect();
            let mut r = Trickle {
                data,
                pos: 0,
                chunk,
                timeout_next: false,
            };
            let mut f = Framer::new(MAX_TCP_MSG);
            let mut got = Vec::new();
            let mut timeouts = 0;
            while got.len() < msgs().len() {
                match f.poll(&mut r).unwrap() {
                    Some(m) => got.push(m),
                    None => timeouts += 1,
                }
            }
            assert_eq!(got, msgs(), "chunk {chunk}");
            assert!(timeouts > 0);
            assert!(f.take_progress());
            assert!(!f.take_progress());
            assert!(matches!(f.poll(&mut r), Ok(None)));
            match f.poll(&mut r) {
                Err(WireError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
                other => panic!("expected EOF, got {other:?}"),
            }
        }
    }

    #[test]
    fn refuses_oversized_and_garbage() {
        let mut f = Framer::new(100);
        let big = encode_tcp(&TcpMsg::Reject {
            reason: "x".repeat(200),
        });
        assert!(matches!(f.poll(&mut &big[..]), Err(WireError::TooLarge(_))));
        let mut f = Framer::new(100);
        assert!(matches!(
            f.poll(&mut &[1u8, 0, 0, 0, 250][..]),
            Err(WireError::Malformed(_))
        ));
        let mut f = Framer::new(100);
        assert!(matches!(
            f.poll(&mut &[0u8, 0, 0, 0][..]),
            Err(WireError::Malformed(_))
        ));
    }
}
