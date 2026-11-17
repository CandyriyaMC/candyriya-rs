//! Minecraft login encryption: RSA key exchange and AES-128-CFB8 stream
//! cipher.
//!
//! This crate implements the cryptographic side of the online-mode login
//! for the Java Edition protocol, as described by
//! <https://minecraft.wiki/w/Java_Edition_protocol/Encryption>:
//!
//! * [`EncryptionKeypair`] - the server's RSA-2048 keypair. The public key is
//!   distributed in the Encryption Request packet as DER-encoded
//!   `SubjectPublicKeyInfo` (X.509) bytes, and encrypts/decrypts the shared
//!   secret and verify token with PKCS#1 v1.5 padding.
//! * [`SharedSecret`] - the 16-byte shared secret. Zeroized on drop, redacted
//!   in logs.
//! * [`AesCfb8`] - the AES-128-CFB8 cipher pair (encrypt + decrypt) used once
//!   both sides hold the secret. Key and IV are both the shared secret, and
//!   the cipher state is continuous across packets (it is not re-initialized
//!   per packet).
//! * [`server_id_hash`] - the session-server `serverId` value:
//!   SHA-1 over the ASCII server id, the shared secret and the public key
//!   DER bytes, interpreted as a signed two's-complement big-endian integer
//!   and printed in lowercase base 16 (the protocol's non-standard
//!   hexdigest).
//!
//! Flow: the server sends Encryption Request with its RSA public key; the
//! client generates the shared secret and sends it (and the verify token)
//! RSA-encrypted in Encryption Response; the server decrypts both and checks
//! the token; from that point on, everything on the wire is encrypted with
//! AES-128-CFB8.
//!
//! This crate performs no network I/O and knows nothing about packets beyond
//! the byte encodings above; packet framing lives in `candyriya-protocol`.

#![forbid(unsafe_code)]

mod cfb8;
mod error;
mod keypair;
mod rng;
mod server_id;
mod shared_secret;

pub use crate::cfb8::{AesCfb8, Cfb8DecryptHalf, Cfb8EncryptHalf};
pub use crate::error::EncryptionError;
pub use crate::keypair::EncryptionKeypair;
pub use crate::server_id::server_id_hash;
pub use crate::shared_secret::SharedSecret;
