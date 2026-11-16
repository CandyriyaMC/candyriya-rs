//! Payload codecs for Minecraft server forwarding modes.
//!
//! When a client connects to a proxy that fronts offline-mode backends, the
//! backend still needs to know who the player is. This crate implements the
//! two formats a proxy uses to hand that identity to a backend:
//!
//! * **Modern** ([`create_forwarding_data`] / [`verify_forwarding_data`]) —
//!   the binary, HMAC-SHA256 authenticated payload sent over the login
//!   plugin channel `velocity:player_info`.
//! * **Legacy** ([`create_legacy_forwarding_address`] /
//!   [`parse_legacy_forwarding_address`]) — the original BungeeCord scheme
//!   of smuggling the identity into the handshake `server_address`, with
//!   optional BungeeGuard token.
//!
//! Everything here is pure byte-in/byte-out: no network, no async, no
//! filesystem. All constructors are deterministic and total; all parsers
//! authenticate before parsing.

#![forbid(unsafe_code)]

mod error;
mod legacy;
mod modern;

pub use crate::error::ForwardingError;
pub use crate::legacy::{
    BUNGEEGUARD_TOKEN_NAME, create_legacy_forwarding_address, parse_legacy_forwarding_address,
};
pub use crate::modern::{
    FORWARDING_VERSION, SIGNATURE_LENGTH, clean_ipv6_scope, create_forwarding_data,
    verify_forwarding_data,
};
