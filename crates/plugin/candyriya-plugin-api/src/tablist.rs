//! The player info (tab list) packet bodies, since the 1.19.3 split.
//!
//! Two packets exist on the wire (verified against the wiki, 2026-11-17):
//!
//! - `Player Info Remove` — `Prefixed Array` of player UUIDs; the tab
//!   list entries for those players vanish.
//! - `Player Info Update` — an actions bitmask byte, a `Prefixed Array`
//!   of player UUIDs, then — per player, in the same order — one payload
//!   per action bit that is set, written in mask order (`0x01` first,
//!   then `0x04`, `0x08`, `0x10`, `0x20`, `0x40`, `0x80`). The mask is
//!   per packet, not per player, so every entry in the packet carries
//!   payloads for exactly the same set of actions; this is enforced on
//!   encode and guaranteed on decode.
//!
//! The actions:
//!
//! | Mask | Action | Payload |
//! |---|---|---|
//! | `0x01` | Add Player | name `String (16)`, properties `Prefixed Array (16)` of { name `String (64)`, value `String (32767)`, signature `Prefixed Optional String (1024)` } |
//! | `0x02` | Initialize Chat | **unsupported** — see below |
//! | `0x04` | Update Game Mode | `VarInt` (`0` survival, `1` creative, `2` adventure, `3` spectator) |
//! | `0x08` | Update Listed | `Boolean`, shown on the tab list |
//! | `0x10` | Update Latency | `VarInt` ping in milliseconds; negative means "not yet known" |
//! | `0x20` | Update Display Name | `Prefixed Optional` [`Component`] (absent = clear the custom name) |
//! | `0x40` | Update List Priority | `VarInt`, higher sorts first |
//! | `0x80` | Update Hat | `Boolean`, show the hat skin layer |
//!
//! ### Notes and deviations
//!
//! - The pre-split (pre-1.19.3) `Player List Item` packet had a single
//!   `Add Player` action carrying the name, properties, game mode, ping
//!   and display name inline. That layout is intentionally not modelled;
//!   this crate is modern-only.
//! - `Update List Priority` and the current `Update Hat` bit assignment
//!   (`0x40` / `0x80` respectively) are the current-spec layout. Earlier
//!   releases used `0x40` for hat and had no list-priority action at
//!   all (the exact release boundary is not recorded here). The proxy
//!   owns any per-version mask translation; this crate documents the
//!   current layout only.
//! - The `Initialize Chat` action (`0x02`) is deliberately not
//!   implemented: it carries chat session crypto data (session id, key
//!   expiry, public key, signature) which the host never forges, and
//!   authoring a fake one would produce packets the client treats as
//!   authentic. Any mask carrying `0x02` is rejected.
//! - The brief sketched a flat `Vec<TabListAction>` API; the modern wire
//!   is a bitmask, so the types here follow the wire. Setting several
//!   `Option` payloads on one entry (`add` + `update_latency` +
//!   `update_listed`, ...) expresses what the brief meant by a full
//!   "add with display name, game mode and latency".
//!
//! Everything here is plain data with serde derives, so the same types
//! can travel across the plugin ABI as JSON. This module never writes
//! packet ids — the proxy picks the id for the negotiated protocol
//! version from its registry.

use bytes::{Buf, BufMut};
use lobsterchat::component::Component;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::wire::{
    MAX_ENTRIES, MAX_PROPERTIES, VisualPacketError, ensure_exhausted, read_bool, read_component,
    read_u8, read_uuid_wire, read_varint, write_bool, write_component, write_uuid_wire,
};
use candyriya_protocol::write_varint;

/// The `Add Player` action bit.
pub const ACTION_ADD_PLAYER: u8 = 0x01;
/// The `Initialize Chat` action bit (unsupported; rejected on decode).
pub const ACTION_INITIALIZE_CHAT: u8 = 0x02;
/// The `Update Game Mode` action bit.
pub const ACTION_UPDATE_GAME_MODE: u8 = 0x04;
/// The `Update Listed` action bit.
pub const ACTION_UPDATE_LISTED: u8 = 0x08;
/// The `Update Latency` action bit.
pub const ACTION_UPDATE_LATENCY: u8 = 0x10;
/// The `Update Display Name` action bit.
pub const ACTION_UPDATE_DISPLAY_NAME: u8 = 0x20;
/// The `Update List Priority` action bit.
pub const ACTION_UPDATE_LIST_PRIORITY: u8 = 0x40;
/// The `Update Hat` action bit.
pub const ACTION_UPDATE_HAT: u8 = 0x80;

/// The actions bitmask of a `Player Info Update` packet.
///
/// A transparent `u8` on the wire; serializes as the raw byte. Bit
/// assignments are the current-spec layout documented at the module
/// level; use the associated constants instead of raw literals.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlayerInfoActionMask(u8);

impl PlayerInfoActionMask {
    /// No actions set.
    pub const NONE: Self = Self(0);
    /// The `Add Player` action bit.
    pub const ADD_PLAYER: Self = Self(ACTION_ADD_PLAYER);
    /// The `Initialize Chat` action bit — unsupported, never constructible
    /// from [`PlayerInfoEntry`], rejected on decode.
    pub const INITIALIZE_CHAT: Self = Self(ACTION_INITIALIZE_CHAT);
    /// The `Update Game Mode` action bit.
    pub const UPDATE_GAME_MODE: Self = Self(ACTION_UPDATE_GAME_MODE);
    /// The `Update Listed` action bit.
    pub const UPDATE_LISTED: Self = Self(ACTION_UPDATE_LISTED);
    /// The `Update Latency` action bit.
    pub const UPDATE_LATENCY: Self = Self(ACTION_UPDATE_LATENCY);
    /// The `Update Display Name` action bit.
    pub const UPDATE_DISPLAY_NAME: Self = Self(ACTION_UPDATE_DISPLAY_NAME);
    /// The `Update List Priority` action bit.
    pub const UPDATE_LIST_PRIORITY: Self = Self(ACTION_UPDATE_LIST_PRIORITY);
    /// The `Update Hat` action bit.
    pub const UPDATE_HAT: Self = Self(ACTION_UPDATE_HAT);

    /// Creates a mask from the raw byte. Unknown/unsupported bits are
    /// preserved; [`Self::contains`]-style queries and the encoder
    /// interpretation decide what to do with them.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The raw byte for the wire.
    pub const fn bits(&self) -> u8 {
        self.0
    }

    /// Whether `mask` is set.
    pub const fn contains(&self, mask: Self) -> bool {
        self.0 & mask.0 != 0
    }

    /// Whether no action bit is set.
    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for PlayerInfoActionMask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = [
            (ACTION_ADD_PLAYER, "add_player"),
            (ACTION_INITIALIZE_CHAT, "initialize_chat"),
            (ACTION_UPDATE_GAME_MODE, "update_game_mode"),
            (ACTION_UPDATE_LISTED, "update_listed"),
            (ACTION_UPDATE_LATENCY, "update_latency"),
            (ACTION_UPDATE_DISPLAY_NAME, "update_display_name"),
            (ACTION_UPDATE_LIST_PRIORITY, "update_list_priority"),
            (ACTION_UPDATE_HAT, "update_hat"),
        ];
        let mut first = true;
        for (bit, name) in names {
            if self.0 & bit != 0 {
                if !first {
                    f.write_str("+")?;
                }
                first = false;
                f.write_str(name)?;
            }
        }
        if first {
            f.write_str("-")?;
        }
        Ok(())
    }
}

/// One game profile property: a namespaced key/value pair, optionally
/// signed by Mojang (the `textures` property is the usual one).
///
/// Wire layout: name `String (64)`, value `String (32767)`, signature
/// `Prefixed Optional String (1024)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameProfileProperty {
    /// The property name, e.g. `textures`. At most 64 UTF-16 code units.
    pub name: String,
    /// The property value, usually base64 for `textures`. At most 32767
    /// UTF-16 code units.
    pub value: String,
    /// The Mojang signature over the value, if any. At most 1024 UTF-16
    /// code units.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl GameProfileProperty {
    /// Creates an unsigned property.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            signature: None,
        }
    }

    /// Creates a signed property.
    pub fn signed(
        name: impl Into<String>,
        value: impl Into<String>,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            signature: Some(signature.into()),
        }
    }

    fn encode(&self, dst: &mut impl BufMut) -> Result<(), VisualPacketError> {
        candyriya_protocol::write_string(dst, &self.name, 64)?;
        candyriya_protocol::write_string(
            dst,
            &self.value,
            candyriya_protocol::DEFAULT_MAX_STRING_LENGTH,
        )?;
        write_bool(dst, self.signature.is_some());
        if let Some(signature) = &self.signature {
            candyriya_protocol::write_string(dst, signature, 1024)?;
        }
        Ok(())
    }

    fn decode(src: &mut impl Buf) -> Result<Self, VisualPacketError> {
        use candyriya_protocol::{DEFAULT_MAX_STRING_LENGTH, read_string};
        let name = read_string(src, 64)?;
        let value = read_string(src, DEFAULT_MAX_STRING_LENGTH)?;
        let signature = if read_bool(src)? {
            Some(read_string(src, 1024)?)
        } else {
            None
        };
        Ok(Self {
            name,
            value,
            signature,
        })
    }
}

impl std::fmt::Display for GameProfileProperty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GameProfileProperty({}{})",
            self.name,
            if self.signature.is_some() {
                ", signed"
            } else {
                ""
            }
        )
    }
}

/// The `Add Player` (`0x01`) payload: the player's name and game profile
/// properties.
///
/// Game mode, latency, display name, list priority and hat are *separate*
/// actions (see the module docs); a fresh tab entry typically means an
/// [`PlayerInfoEntry`] with this set alongside the others.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddPlayerData {
    /// The player's name, 3–16 characters in vanilla; at most 16 UTF-16
    /// code units on the wire.
    pub name: String,
    /// The game profile properties, at most 16 on the wire. Usually a
    /// single `textures` property (base64) carrying skin and cape.
    pub properties: Vec<GameProfileProperty>,
}

impl AddPlayerData {
    fn encode(&self, dst: &mut impl BufMut) -> Result<(), VisualPacketError> {
        if self.name.is_empty() {
            return Err(VisualPacketError::EmptyCollection {
                what: "player name",
            });
        }
        if self.properties.len() > MAX_PROPERTIES {
            return Err(VisualPacketError::CollectionTooLarge {
                what: "properties",
                cap: MAX_PROPERTIES,
            });
        }
        candyriya_protocol::write_string(dst, &self.name, 16)?;
        write_varint(self.properties.len() as i32, dst);
        for property in &self.properties {
            property.encode(dst)?;
        }
        Ok(())
    }

    fn decode(src: &mut impl Buf) -> Result<Self, VisualPacketError> {
        use candyriya_protocol::read_string;
        let name = read_string(src, 16)?;
        let count = read_varint(src)?;
        let count = checked_count(count, "properties", MAX_PROPERTIES)?;
        let mut properties = Vec::with_capacity(count);
        for _ in 0..count {
            properties.push(GameProfileProperty::decode(src)?);
        }
        Ok(Self { name, properties })
    }
}

impl std::fmt::Display for AddPlayerData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AddPlayer(name={}, properties={})",
            self.name,
            self.properties.len()
        )
    }
}

/// One player entry inside a `Player Info Update` packet.
///
/// Every `Option` field is one action: `None` means "this action is not
/// part of the packet for this player". The wire mask is per packet, so
/// all entries of a packet must carry payloads for exactly the same set
/// of actions — see [`PlayerInfoUpdate`].
///
/// `update_display_name` is a double option, mirroring the wire's
/// `Prefixed Optional Text Component`:
///
/// - `None` — the `0x20` bit is not part of the mask;
/// - `Some(None)` — the bit is present and the custom display name is
///   *cleared* (the player shows with their plain name again);
/// - `Some(Some(component))` — the bit is present and the player's
///   display name is set to `component`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerInfoEntry {
    /// The player's UUID.
    pub uuid: Uuid,
    /// The `Add Player` (`0x01`) payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add: Option<AddPlayerData>,
    /// The `Update Game Mode` (`0x04`) payload: `0..=3` for the vanilla
    /// modes (survival, creative, adventure, spectator).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_game_mode: Option<i32>,
    /// The `Update Listed` (`0x08`) payload: whether the player shows on
    /// the tab list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_listed: Option<bool>,
    /// The `Update Latency` (`0x10`) payload: ping in milliseconds;
    /// negative means "not yet known" (client shows the no-connection
    /// icon).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_latency: Option<i32>,
    /// The `Update Display Name` (`0x20`) payload; see the type-level
    /// docs for the double-option meaning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_display_name: Option<Option<Component>>,
    /// The `Update List Priority` (`0x40`) payload: a higher value sorts
    /// the player earlier in the tab list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_list_priority: Option<i32>,
    /// The `Update Hat` (`0x80`) payload: whether the hat skin layer is
    /// shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_hat: Option<bool>,
}

impl PlayerInfoEntry {
    /// Creates a bare entry with no actions set; set the payload fields
    /// you need before encoding.
    pub fn new(uuid: Uuid) -> Self {
        Self {
            uuid,
            add: None,
            update_game_mode: None,
            update_listed: None,
            update_latency: None,
            update_display_name: None,
            update_list_priority: None,
            update_hat: None,
        }
    }

    /// The action mask this entry implies (the bits for every `Some`
    /// payload it carries).
    pub fn action_mask(&self) -> PlayerInfoActionMask {
        let mut mask = 0u8;
        if self.add.is_some() {
            mask |= ACTION_ADD_PLAYER;
        }
        if self.update_game_mode.is_some() {
            mask |= ACTION_UPDATE_GAME_MODE;
        }
        if self.update_listed.is_some() {
            mask |= ACTION_UPDATE_LISTED;
        }
        if self.update_latency.is_some() {
            mask |= ACTION_UPDATE_LATENCY;
        }
        if self.update_display_name.is_some() {
            mask |= ACTION_UPDATE_DISPLAY_NAME;
        }
        if self.update_list_priority.is_some() {
            mask |= ACTION_UPDATE_LIST_PRIORITY;
        }
        if self.update_hat.is_some() {
            mask |= ACTION_UPDATE_HAT;
        }
        PlayerInfoActionMask::from_bits(mask)
    }
}

impl std::fmt::Display for PlayerInfoEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlayerInfoEntry({}, {})", self.uuid, self.action_mask())
    }
}

/// A `Player Info Update` packet body: the entries of the tab list to add
/// or update.
///
/// The action mask is derived from the entries, not stored: all entries
/// of one packet must imply the same mask (the wire mask is per packet),
/// and encoding fails with [`VisualPacketError::MaskMismatch`] when they
/// do not. Use [`PlayerInfoUpdate::from_entries`] to build one.
///
/// # Wire format
///
/// `Actions` (`u8`) then a `Prefixed Array` of player UUIDs, then — per
/// player, in order — one payload per set action bit, in the fixed bit
/// order documented at the module level. There is no packet id here —
/// the proxy writes it, resolved from its per-version registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerInfoUpdate {
    /// The entries, at most 4096, all implying the same action mask.
    pub entries: Vec<PlayerInfoEntry>,
}

impl PlayerInfoUpdate {
    /// Creates an update from the entries, deriving the common action
    /// mask. Fails if the entries are empty, all imply no actions, or
    /// imply different masks.
    pub fn from_entries(entries: Vec<PlayerInfoEntry>) -> Result<Self, VisualPacketError> {
        let update = Self { entries };
        update.validate()?;
        Ok(update)
    }

    /// The common action mask implied by the entries.
    ///
    /// Returns `Err(EmptyCollection)` for zero entries and
    /// `Err(MaskMismatch)` when the entries disagree; `Ok` otherwise.
    pub fn mask(&self) -> Result<PlayerInfoActionMask, VisualPacketError> {
        let mut iter = self.entries.iter();
        let Some(first) = iter.next() else {
            return Err(VisualPacketError::EmptyCollection {
                what: "player info update entries",
            });
        };
        let mask = first.action_mask();
        for entry in iter {
            let other = entry.action_mask();
            if other != mask {
                return Err(VisualPacketError::MaskMismatch(other.bits(), mask.bits()));
            }
        }
        Ok(mask)
    }

    /// Validates the packet for encoding.
    pub fn validate(&self) -> Result<(), VisualPacketError> {
        if self.entries.len() > MAX_ENTRIES {
            return Err(VisualPacketError::CollectionTooLarge {
                what: "player info update entries",
                cap: MAX_ENTRIES,
            });
        }
        let mask = self.mask()?;
        if mask.is_empty() {
            return Err(VisualPacketError::EmptyActions);
        }
        for entry in &self.entries {
            if let Some(mode) = entry.update_game_mode
                && !(0..=3).contains(&mode)
            {
                return Err(VisualPacketError::InvalidGameMode(mode));
            }
            if let Some(add) = &entry.add {
                if add.name.is_empty() {
                    return Err(VisualPacketError::EmptyCollection {
                        what: "player name",
                    });
                }
                if add.properties.len() > MAX_PROPERTIES {
                    return Err(VisualPacketError::CollectionTooLarge {
                        what: "properties",
                        cap: MAX_PROPERTIES,
                    });
                }
            }
        }
        Ok(())
    }

    /// Encodes the packet body (no packet id, no frame) into `dst`.
    pub fn encode(&self, dst: &mut impl BufMut) -> Result<(), VisualPacketError> {
        self.validate()?;
        let mask = self.mask()?;
        dst.put_u8(mask.bits());
        write_varint(self.entries.len() as i32, dst);
        for entry in &self.entries {
            write_uuid_wire(dst, entry.uuid);
        }
        for entry in &self.entries {
            // payload order is fixed: strictly by mask bit, lowest first
            let entry_mask = entry.action_mask();
            if mask.bits() & ACTION_ADD_PLAYER != 0 {
                let add = entry.add.as_ref().ok_or(VisualPacketError::MaskMismatch(
                    entry_mask.bits(),
                    mask.bits(),
                ))?;
                add.encode(dst)?;
            }
            if mask.bits() & ACTION_UPDATE_GAME_MODE != 0 {
                let mode = entry
                    .update_game_mode
                    .ok_or(VisualPacketError::MaskMismatch(
                        entry_mask.bits(),
                        mask.bits(),
                    ))?;
                write_varint(mode, dst);
            }
            if mask.bits() & ACTION_UPDATE_LISTED != 0 {
                let listed = entry.update_listed.ok_or(VisualPacketError::MaskMismatch(
                    entry_mask.bits(),
                    mask.bits(),
                ))?;
                write_bool(dst, listed);
            }
            if mask.bits() & ACTION_UPDATE_LATENCY != 0 {
                let latency = entry.update_latency.ok_or(VisualPacketError::MaskMismatch(
                    entry_mask.bits(),
                    mask.bits(),
                ))?;
                write_varint(latency, dst);
            }
            if mask.bits() & ACTION_UPDATE_DISPLAY_NAME != 0 {
                let display_name =
                    entry
                        .update_display_name
                        .as_ref()
                        .ok_or(VisualPacketError::MaskMismatch(
                            entry_mask.bits(),
                            mask.bits(),
                        ))?;
                match display_name {
                    Some(component) => {
                        write_bool(dst, true);
                        write_component(dst, component)?;
                    }
                    None => write_bool(dst, false),
                }
            }
            if mask.bits() & ACTION_UPDATE_LIST_PRIORITY != 0 {
                let priority =
                    entry
                        .update_list_priority
                        .ok_or(VisualPacketError::MaskMismatch(
                            entry_mask.bits(),
                            mask.bits(),
                        ))?;
                write_varint(priority, dst);
            }
            if mask.bits() & ACTION_UPDATE_HAT != 0 {
                let hat = entry.update_hat.ok_or(VisualPacketError::MaskMismatch(
                    entry_mask.bits(),
                    mask.bits(),
                ))?;
                write_bool(dst, hat);
            }
        }
        Ok(())
    }

    /// Encodes the packet body into a fresh `Vec`.
    pub fn to_vec(&self) -> Result<Vec<u8>, VisualPacketError> {
        let mut buf = bytes::BytesMut::new();
        self.encode(&mut buf)?;
        Ok(buf.to_vec())
    }

    /// Decodes a packet body (the bytes after the packet id).
    ///
    /// Fails on a negative or oversized player count, on the
    /// `Initialize Chat` bit (unsupported), on inconsistent masks, or on
    /// leftover bytes.
    pub fn decode(src: &mut impl Buf) -> Result<Self, VisualPacketError> {
        let mask = read_u8(src)?;
        if mask & ACTION_INITIALIZE_CHAT != 0 {
            return Err(VisualPacketError::UnsupportedInitializeChat);
        }
        if mask == 0 {
            return Err(VisualPacketError::EmptyActions);
        }
        let count = read_varint(src)?;
        let count = checked_count(count, "players", MAX_ENTRIES)?;
        if count == 0 {
            // an empty update packet would decode into a value that can
            // never re-encode; reject it strictly like the remove packet
            return Err(VisualPacketError::EmptyCollection {
                what: "player info update entries",
            });
        }
        let mut uuids = Vec::with_capacity(count);
        for _ in 0..count {
            uuids.push(read_uuid_wire(src)?);
        }
        let mut entries = Vec::with_capacity(count);
        for uuid in uuids {
            let mut entry = PlayerInfoEntry::new(uuid);
            if mask & ACTION_ADD_PLAYER != 0 {
                entry.add = Some(AddPlayerData::decode(src)?);
            }
            if mask & ACTION_UPDATE_GAME_MODE != 0 {
                entry.update_game_mode = Some(read_varint(src)?);
            }
            if mask & ACTION_UPDATE_LISTED != 0 {
                entry.update_listed = Some(read_bool(src)?);
            }
            if mask & ACTION_UPDATE_LATENCY != 0 {
                entry.update_latency = Some(read_varint(src)?);
            }
            if mask & ACTION_UPDATE_DISPLAY_NAME != 0 {
                entry.update_display_name = Some(if read_bool(src)? {
                    Some(read_component(src)?)
                } else {
                    None
                });
            }
            if mask & ACTION_UPDATE_LIST_PRIORITY != 0 {
                entry.update_list_priority = Some(read_varint(src)?);
            }
            if mask & ACTION_UPDATE_HAT != 0 {
                entry.update_hat = Some(read_bool(src)?);
            }
            entries.push(entry);
        }
        ensure_exhausted(src)?;
        Ok(Self { entries })
    }

    /// Decodes a packet body from a byte slice, fully consuming it.
    pub fn from_slice(src: &[u8]) -> Result<Self, VisualPacketError> {
        let mut src = src;
        Self::decode(&mut src)
    }
}

impl std::fmt::Display for PlayerInfoUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mask = self
            .mask()
            .map(|m| m.to_string())
            .unwrap_or_else(|_| "<invalid>".to_string());
        write!(
            f,
            "PlayerInfoUpdate(players={}, actions={mask})",
            self.entries.len()
        )
    }
}

/// The `Player Info Remove` packet body: the UUIDs whose tab list entries
/// are dropped.
///
/// # Wire format
///
/// A `Prefixed Array` of UUIDs. There is no packet id here — the proxy
/// writes it, resolved from its per-version registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerInfoRemove {
    /// The UUIDs to remove, at least 1 and at most 4096.
    pub uuids: Vec<Uuid>,
}

impl PlayerInfoRemove {
    /// Creates a remove packet from the UUIDs. Fails if the list is empty
    /// or exceeds the 4096-entry cap.
    pub fn new(uuids: Vec<Uuid>) -> Result<Self, VisualPacketError> {
        let remove = Self { uuids };
        remove.validate()?;
        Ok(remove)
    }

    /// Validates the packet for encoding.
    pub fn validate(&self) -> Result<(), VisualPacketError> {
        if self.uuids.is_empty() {
            return Err(VisualPacketError::EmptyCollection {
                what: "player info remove uuids",
            });
        }
        if self.uuids.len() > MAX_ENTRIES {
            return Err(VisualPacketError::CollectionTooLarge {
                what: "player info remove uuids",
                cap: MAX_ENTRIES,
            });
        }
        Ok(())
    }

    /// Encodes the packet body (no packet id, no frame) into `dst`.
    pub fn encode(&self, dst: &mut impl BufMut) -> Result<(), VisualPacketError> {
        self.validate()?;
        write_varint(self.uuids.len() as i32, dst);
        for uuid in &self.uuids {
            write_uuid_wire(dst, *uuid);
        }
        Ok(())
    }

    /// Encodes the packet body into a fresh `Vec`.
    pub fn to_vec(&self) -> Result<Vec<u8>, VisualPacketError> {
        let mut buf = bytes::BytesMut::new();
        self.encode(&mut buf)?;
        Ok(buf.to_vec())
    }

    /// Decodes a packet body (the bytes after the packet id).
    pub fn decode(src: &mut impl Buf) -> Result<Self, VisualPacketError> {
        let count = read_varint(src)?;
        let count = checked_count(count, "players", MAX_ENTRIES)?;
        if count == 0 {
            return Err(VisualPacketError::EmptyCollection {
                what: "player info remove uuids",
            });
        }
        let mut uuids = Vec::with_capacity(count);
        for _ in 0..count {
            uuids.push(read_uuid_wire(src)?);
        }
        ensure_exhausted(src)?;
        Ok(Self { uuids })
    }

    /// Decodes a packet body from a byte slice, fully consuming it.
    pub fn from_slice(src: &[u8]) -> Result<Self, VisualPacketError> {
        let mut src = src;
        Self::decode(&mut src)
    }
}

impl std::fmt::Display for PlayerInfoRemove {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlayerInfoRemove(players={})", self.uuids.len())
    }
}

/// Validates a decoded `Prefixed Array` count.
fn checked_count(count: i32, what: &'static str, cap: usize) -> Result<usize, VisualPacketError> {
    if count < 0 {
        return Err(VisualPacketError::InvalidArrayCount { what, count });
    }
    let count = count as usize;
    if count > cap {
        return Err(VisualPacketError::CollectionTooLarge { what, cap });
    }
    Ok(count)
}
