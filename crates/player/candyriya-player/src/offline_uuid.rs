//! Derivation of the deterministic offline-mode UUID.
//!
//! Per the Minecraft protocol (minecraft.wiki, "UUID" and "Java Edition
//! protocol/Packets"), a Java Edition server in offline mode cannot receive
//! a Mojang-issued UUID, so the server derives one from the player name:
//! it is a **UUID version 3**, computed as the MD5 hash of the UTF-8 bytes
//! of `OfflinePlayer:<name>`, with the version nibble set to 3 and the
//! variant bits set to RFC 4122 (`10` in the two most significant bits).
//!
//! The implementation mirrors Java's
//! `UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes(UTF_8))`
//! exactly.

use uuid::Uuid;

/// Derive the offline-mode UUID for a player `name`.
///
/// The derivation is deterministic: the same name always yields the same
/// UUID, independent of platform, and matches what vanilla servers hand to
/// plugins and backends. Names are **not** validated — any string derives a
/// valid, distinct UUID; the caller is responsible for enforcing the
/// protocol's name rules before login if it wants them.
///
/// The protocol name rule for Java Edition is 3–16 chars,
/// `[A-Za-z0-9_]` — see `candyriya-auth`'s `validate_username` for the
/// shared validator. A 1-2 character name is not a legal offline-mode
/// name, whatever this function accepts.
///
/// ```
/// use candyriya_player::offline_uuid;
///
/// // Worked example from the wiki: "OfflinePlayer:Notch".
/// assert_eq!(
///     offline_uuid("Notch").to_string(),
///     "b50ad385-829d-3141-a216-7e7d7539ba7f",
/// );
/// ```
pub fn offline_uuid(name: &str) -> Uuid {
    // format! pushes at most 14 bytes of prefix plus the name into one
    // allocation — cheap, allocation happens once, no copies afterwards.
    let input = format!("OfflinePlayer:{name}");
    let digest = md5::compute(input.as_bytes());
    let mut bytes = digest.0;

    // RFC 4122 §4.2: set the version nibble to 3 (name-based, MD5).
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    // RFC 4122 §4.2.2: set the variant to 10.., the RFC 4122 variant.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    Uuid::from_bytes(bytes)
}
