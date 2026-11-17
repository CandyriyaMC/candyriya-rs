//! The travel direction of a packet on `candyriya:main`.
//!
//! Both directions share one channel name but disjoint packet id ranges
//! (`0x01..=0x0F` for serverbound, `0x80..=0x8F` for clientbound), so a
//! single channel carries the conversation without either side having to
//! know which end of the connection a payload came from. The direction is
//! not encoded on the wire; it is implied by which side produced the
//! payload. Encoding or decoding with the wrong direction is an error,
//! never a silent reinterpretation.

use std::fmt;

/// Which way a packet on `candyriya:main` travels.
///
/// `Serverbound` packets are produced by a backend and consumed by the
/// proxy; `Clientbound` packets are produced by the proxy and consumed by
/// a backend. The packet id ranges do not overlap, but [`crate::ServerboundMessage`]
/// and [`crate::ClientboundMessage`] still take the direction as an
/// explicit argument so a mix-up fails loudly instead of producing bytes
/// the other side would reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Backend to proxy (packet ids `0x01..=0x0F`).
    Serverbound,
    /// Proxy to backend (packet ids `0x80..=0x8F`).
    Clientbound,
}

impl Direction {
    /// The direction as a lowercase word, for error messages and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Serverbound => "serverbound",
            Self::Clientbound => "clientbound",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
