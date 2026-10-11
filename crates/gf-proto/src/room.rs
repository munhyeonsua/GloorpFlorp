//! Room codes: a short code like `K7QM-2XPA-9TD` that tells the joiner where
//! the host is and proves they were invited.
//!
//! Layout (55 bits, 11 Crockford base32 characters, shown as 4-4-3):
//! - 22 bits: host's Tailscale IPv4 address minus the 100.64.0.0/10 prefix
//! - 30 bits: random secret
//! - 3 bits: checksum (catches most typos)
//!
//! The bits are packed most significant first, `ip_offset << 33 | secret << 3
//! | checksum`, and written 5 bits per character, most significant first.
//! The checksum is the top 3 bits of the SplitMix64 finalizer applied to the
//! 52-bit payload `ip_offset << 30 | secret`; any change to the payload flips
//! it with probability 7/8, and any change to the checksum bits is always
//! caught.
//!
//! Parsing is forgiving: case-insensitive, spaces and dashes ignored, and the
//! usual Crockford confusables (I/L -> 1, O -> 0) accepted.

use std::net::Ipv4Addr;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RoomError {
    #[error("{0} is not a Tailscale address (100.64.0.0 – 100.127.255.255)")]
    NotTailscale(Ipv4Addr),
    #[error("a room code has 11 letters/digits, this one has {0}")]
    WrongLength(usize),
    #[error("'{0}' can't appear in a room code")]
    BadChar(char),
    #[error("that code has a typo in it (checksum mismatch)")]
    Checksum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomCode {
    pub ip: Ipv4Addr,
    /// Only the low 30 bits are used.
    pub secret: u32,
}

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_CHARS: usize = 11;
const TAILSCALE_BASE: u32 = 0x6440_0000; // 100.64.0.0
const IP_BITS: u32 = 22;
const SECRET_MASK: u32 = (1 << 30) - 1;

fn checksum(payload: u64) -> u64 {
    let mut z = payload.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z >> 61
}

fn char_value(c: char) -> Option<u64> {
    let v = match c.to_ascii_uppercase() {
        c @ '0'..='9' => c as u8 - b'0',
        'I' | 'L' => 1,
        'O' => 0,
        c => ALPHABET.iter().position(|&a| a as char == c)? as u8,
    };
    Some(v as u64)
}

impl RoomCode {
    /// New code for a host at `ip` with a fresh random secret.
    pub fn new_random(ip: Ipv4Addr) -> Result<Self, RoomError> {
        if !is_tailscale_ipv4(ip) {
            return Err(RoomError::NotTailscale(ip));
        }
        Ok(RoomCode {
            ip,
            secret: rand::random::<u32>() & SECRET_MASK,
        })
    }

    /// Format as `XXXX-XXXX-XXX`. Only the low 22 bits of the offset from
    /// 100.64.0.0 are encoded, so `ip` must be a Tailscale address (as
    /// [`RoomCode::new_random`] guarantees) to survive a round trip.
    pub fn encode(&self) -> String {
        let offset = u32::from(self.ip).wrapping_sub(TAILSCALE_BASE) & ((1 << IP_BITS) - 1);
        let payload = (u64::from(offset) << 30) | u64::from(self.secret & SECRET_MASK);
        let bits = (payload << 3) | checksum(payload);
        let mut out = String::with_capacity(CODE_CHARS + 2);
        for i in 0..CODE_CHARS {
            if i == 4 || i == 8 {
                out.push('-');
            }
            let shift = 5 * (CODE_CHARS - 1 - i);
            out.push(ALPHABET[((bits >> shift) & 31) as usize] as char);
        }
        out
    }

    pub fn parse(s: &str) -> Result<Self, RoomError> {
        let mut bits = 0u64;
        let mut count = 0usize;
        for c in s.chars() {
            if c.is_whitespace() || matches!(c, '-' | '‐' | '–' | '—') {
                continue;
            }
            let v = char_value(c).ok_or(RoomError::BadChar(c))?;
            count += 1;
            if count <= CODE_CHARS {
                bits = (bits << 5) | v;
            }
        }
        if count != CODE_CHARS {
            return Err(RoomError::WrongLength(count));
        }
        let payload = bits >> 3;
        if bits & 7 != checksum(payload) {
            return Err(RoomError::Checksum);
        }
        let offset = (payload >> 30) as u32;
        Ok(RoomCode {
            ip: Ipv4Addr::from(TAILSCALE_BASE + offset),
            secret: payload as u32 & SECRET_MASK,
        })
    }
}

impl std::fmt::Display for RoomCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.encode())
    }
}

impl std::str::FromStr for RoomCode {
    type Err = RoomError;
    fn from_str(s: &str) -> Result<Self, RoomError> {
        RoomCode::parse(s)
    }
}

/// True for addresses in Tailscale's CGNAT range 100.64.0.0/10.
pub fn is_tailscale_ipv4(ip: Ipv4Addr) -> bool {
    u32::from(ip) & 0xFFC0_0000 == TAILSCALE_BASE
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn random_ip(rng: &mut StdRng) -> Ipv4Addr {
        Ipv4Addr::from(TAILSCALE_BASE + rng.random_range(0..1u32 << IP_BITS))
    }

    #[test]
    fn tailscale_range() {
        assert!(is_tailscale_ipv4(Ipv4Addr::new(100, 64, 0, 0)));
        assert!(is_tailscale_ipv4(Ipv4Addr::new(100, 101, 7, 9)));
        assert!(is_tailscale_ipv4(Ipv4Addr::new(100, 127, 255, 255)));
        assert!(!is_tailscale_ipv4(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(!is_tailscale_ipv4(Ipv4Addr::new(100, 128, 0, 0)));
        assert!(!is_tailscale_ipv4(Ipv4Addr::new(192, 168, 1, 2)));
        assert!(!is_tailscale_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn rejects_non_tailscale_ip() {
        let ip = Ipv4Addr::new(192, 168, 1, 20);
        assert_eq!(RoomCode::new_random(ip), Err(RoomError::NotTailscale(ip)));
    }

    #[test]
    fn round_trip_many() {
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..20_000 {
            let code = RoomCode {
                ip: random_ip(&mut rng),
                secret: rng.random::<u32>() & SECRET_MASK,
            };
            let s = code.encode();
            assert_eq!(s.len(), 13);
            assert_eq!(&s[4..5], "-");
            assert_eq!(&s[9..10], "-");
            assert_eq!(RoomCode::parse(&s), Ok(code), "{s}");
            assert_eq!(s.parse::<RoomCode>(), Ok(code));
        }
        for ip in [
            Ipv4Addr::new(100, 64, 0, 0),
            Ipv4Addr::new(100, 127, 255, 255),
        ] {
            for secret in [0, SECRET_MASK] {
                let code = RoomCode { ip, secret };
                assert_eq!(RoomCode::parse(&code.encode()), Ok(code));
            }
        }
    }

    #[test]
    fn new_random_round_trips() {
        let ip = Ipv4Addr::new(100, 100, 12, 34);
        let a = RoomCode::new_random(ip).unwrap();
        let b = RoomCode::new_random(ip).unwrap();
        assert!(a.secret <= SECRET_MASK);
        assert_ne!(a.secret, b.secret, "secrets should be random");
        assert_eq!(RoomCode::parse(&a.to_string()), Ok(a));
    }

    #[test]
    fn forgiving_parse() {
        let code = RoomCode {
            ip: Ipv4Addr::new(100, 90, 1, 1),
            secret: 0x1234_5678 & SECRET_MASK,
        };
        let s = code.encode();
        let plain: String = s.chars().filter(|&c| c != '-').collect();
        assert_eq!(RoomCode::parse(&s.to_lowercase()), Ok(code));
        assert_eq!(RoomCode::parse(&plain), Ok(code));
        assert_eq!(
            RoomCode::parse(&format!(
                "  {} {} {}\n",
                &plain[..3],
                &plain[3..7],
                &plain[7..]
            )),
            Ok(code)
        );
        assert_eq!(RoomCode::parse(&s.replace('-', "–")), Ok(code));
        // Confusables map to the digits they look like.
        let ones = s.replace('1', "I");
        let zeros = s.replace('0', "o");
        assert_eq!(RoomCode::parse(&ones), Ok(code));
        assert_eq!(RoomCode::parse(&zeros), Ok(code));
        assert_eq!(RoomCode::parse(&s.replace('1', "l")), Ok(code));
    }

    #[test]
    fn wrong_length() {
        assert_eq!(RoomCode::parse(""), Err(RoomError::WrongLength(0)));
        assert_eq!(
            RoomCode::parse("ABCD-EFGH-JK"),
            Err(RoomError::WrongLength(10))
        );
        assert_eq!(
            RoomCode::parse("ABCD-EFGH-JKMN"),
            Err(RoomError::WrongLength(12))
        );
        assert_eq!(
            RoomCode::parse(&"7".repeat(500)),
            Err(RoomError::WrongLength(500))
        );
    }

    #[test]
    fn bad_chars() {
        assert_eq!(
            RoomCode::parse("ABCD-EFGH-JKU"),
            Err(RoomError::BadChar('U'))
        );
        assert_eq!(
            RoomCode::parse("abcd-efgh-jku"),
            Err(RoomError::BadChar('u'))
        );
        assert_eq!(
            RoomCode::parse("ABCD_EFGH_JKM"),
            Err(RoomError::BadChar('_'))
        );
        assert_eq!(
            RoomCode::parse("ABCD-EFGH-JK!"),
            Err(RoomError::BadChar('!'))
        );
        assert_eq!(
            RoomCode::parse("ÄBCD-EFGH-JKM"),
            Err(RoomError::BadChar('Ä'))
        );
    }

    #[test]
    fn typos_usually_caught() {
        let mut rng = StdRng::seed_from_u64(7);
        let (mut tries, mut caught) = (0u32, 0u32);
        for _ in 0..5_000 {
            let code = RoomCode {
                ip: random_ip(&mut rng),
                secret: rng.random::<u32>() & SECRET_MASK,
            };
            let mut chars: Vec<u8> = code.encode().into_bytes();
            let mut pos = rng.random_range(0..chars.len());
            if chars[pos] == b'-' {
                pos += 1;
            }
            let orig = chars[pos];
            let mut new = orig;
            while new == orig {
                new = ALPHABET[rng.random_range(0..32)];
            }
            chars[pos] = new;
            tries += 1;
            match RoomCode::parse(std::str::from_utf8(&chars).unwrap()) {
                Err(RoomError::Checksum) => caught += 1,
                Ok(other) => assert_ne!(other, code),
                Err(e) => panic!("unexpected error {e}"),
            }
        }
        let rate = f64::from(caught) / f64::from(tries);
        assert!(
            rate > 0.83,
            "only {rate:.3} of single-character typos caught"
        );
    }

    #[test]
    fn checksum_bits_always_checked() {
        let code = RoomCode {
            ip: Ipv4Addr::new(100, 64, 3, 4),
            secret: 99,
        };
        let s = code.encode();
        let last = s.as_bytes()[12];
        let v = ALPHABET.iter().position(|&a| a == last).unwrap();
        for flip in 1..8 {
            let mut bad = s.clone().into_bytes();
            bad[12] = ALPHABET[v ^ flip];
            assert_eq!(
                RoomCode::parse(std::str::from_utf8(&bad).unwrap()),
                Err(RoomError::Checksum)
            );
        }
    }
}
