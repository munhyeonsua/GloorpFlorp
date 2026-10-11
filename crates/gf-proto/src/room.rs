//! Room codes: a short code like `K7QM-2XPA-9TD` that tells the joiner where
//! the host is and proves they were invited.
//!
//! Layout (55 bits, 11 Crockford base32 characters, shown as 4-4-3):
//! - 22 bits: host's Tailscale IPv4 address minus the 100.64.0.0/10 prefix
//! - 30 bits: random secret
//! - 3 bits: checksum (catches most typos)
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

impl RoomCode {
    /// New code for a host at `ip` with a fresh random secret.
    pub fn new_random(ip: Ipv4Addr) -> Result<Self, RoomError> {
        todo!()
    }

    /// Format as `XXXX-XXXX-XXX`.
    pub fn encode(&self) -> String {
        todo!()
    }

    pub fn parse(s: &str) -> Result<Self, RoomError> {
        todo!()
    }
}

/// True for addresses in Tailscale's CGNAT range 100.64.0.0/10.
pub fn is_tailscale_ipv4(ip: Ipv4Addr) -> bool {
    todo!()
}
