//! The boss bar packet body: the bar many servers use to show progress,
//! health pools and announcements.
//!
//! One packet carries one [`BossBarPacket`]: the bar's UUID and one
//! [`BossBarAction`]. The action is encoded as a `VarInt` discriminant
//! followed by that action's payload (verified against the wiki,
//! 2026-11-17):
//!
//! | Id | Action | Payload |
//! |---|---|---|
//! | `0` | [`BossBarAction::Add`] | title [`Component`], health `Float`, color `VarInt`, division `VarInt`, flags `u8` |
//! | `1` | [`BossBarAction::Remove`] | — (no fields) |
//! | `2` | [`BossBarAction::UpdateHealth`] | health `Float` |
//! | `3` | [`BossBarAction::UpdateTitle`] | title [`Component`] |
//! | `4` | [`BossBarAction::UpdateStyle`] | color `VarInt`, division `VarInt` |
//! | `5` | [`BossBarAction::UpdateFlags`] | flags `u8` |
//!
//! ### Health
//!
//! The wire type is a 32-bit IEEE float in the inclusive `0.0..=1.0`
//! range. Encoding enforces the range (and rejects non-finite values);
//! decoding rejects `NaN`/infinite values but passes finite out-of-range
//! values through untouched — the vanilla client renders those without
//! crashing (part of a second bar appears), so dropping them silently on
//! decode would lose information the caller may legitimately want.
//!
//! ### Flags
//!
//! Three bits are defined, matching the names used by Adventure/Paper:
//!
//! - `0x01` [`BossBarFlag::DarkenSky`] — darken the world sky with the bar
//! - `0x02` [`BossBarFlag::PlayBossMusic`] — play the dragon boss music
//! - `0x04` [`BossBarFlag::CreateFog`] — create fog
//!
//! Note on history: before 1.13 the fog and dragon music were both
//! controlled by `0x02`; they are split since 1.13 (`0x02` = dragon bar,
//! `0x04` = fog). The crate documents the split behavior only.
//!
//! Everything here is plain data with serde derives, so the same types can
//! travel across the plugin ABI as JSON. This module never writes packet
//! ids — the proxy picks the id for the negotiated protocol version from
//! its registry.

use bytes::{Buf, BufMut};
use lobsterchat::component::Component;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::wire::{
    VisualPacketError, ensure_exhausted, read_component, read_u8, read_uuid_wire, read_varint,
    write_component, write_uuid_wire,
};
use candyriya_protocol::write_varint;

/// The color of the bar, encoded as a `VarInt` discriminant.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BossBarColor {
    /// Pink (id `0`).
    Pink,
    /// Blue (id `1`).
    Blue,
    /// Red (id `2`).
    Red,
    /// Green (id `3`).
    Green,
    /// Yellow (id `4`).
    Yellow,
    /// Purple (id `5`).
    Purple,
    /// White (id `6`).
    White,
}

impl BossBarColor {
    /// The wire discriminant of the color.
    pub fn id(&self) -> i32 {
        match self {
            Self::Pink => 0,
            Self::Blue => 1,
            Self::Red => 2,
            Self::Green => 3,
            Self::Yellow => 4,
            Self::Purple => 5,
            Self::White => 6,
        }
    }

    /// Parses a wire discriminant.
    pub fn from_id(id: i32) -> Result<Self, VisualPacketError> {
        Ok(match id {
            0 => Self::Pink,
            1 => Self::Blue,
            2 => Self::Red,
            3 => Self::Green,
            4 => Self::Yellow,
            5 => Self::Purple,
            6 => Self::White,
            other => {
                return Err(VisualPacketError::UnknownDiscriminant {
                    what: "boss bar color",
                    found: other,
                });
            }
        })
    }

    /// The stable wire name of the color, matching the serde
    /// representation (`"pink"`, `"blue"`, ...).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pink => "pink",
            Self::Blue => "blue",
            Self::Red => "red",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Purple => "purple",
            Self::White => "white",
        }
    }
}

impl std::fmt::Display for BossBarColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the bar is divided into notches, encoded as a `VarInt` discriminant.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BossBarDivision {
    /// No division, one smooth bar (id `0`).
    NoDivision,
    /// 6 notches (id `1`).
    SixNotches,
    /// 10 notches (id `2`).
    TenNotches,
    /// 12 notches (id `3`).
    TwelveNotches,
    /// 20 notches (id `4`).
    TwentyNotches,
}

impl BossBarDivision {
    /// The wire discriminant of the division.
    pub fn id(&self) -> i32 {
        match self {
            Self::NoDivision => 0,
            Self::SixNotches => 1,
            Self::TenNotches => 2,
            Self::TwelveNotches => 3,
            Self::TwentyNotches => 4,
        }
    }

    /// Parses a wire discriminant.
    pub fn from_id(id: i32) -> Result<Self, VisualPacketError> {
        Ok(match id {
            0 => Self::NoDivision,
            1 => Self::SixNotches,
            2 => Self::TenNotches,
            3 => Self::TwelveNotches,
            4 => Self::TwentyNotches,
            other => {
                return Err(VisualPacketError::UnknownDiscriminant {
                    what: "boss bar division",
                    found: other,
                });
            }
        })
    }

    /// The stable wire name of the division, matching the serde
    /// representation (`"no_division"`, `"six_notches"`, ...).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoDivision => "no_division",
            Self::SixNotches => "six_notches",
            Self::TenNotches => "ten_notches",
            Self::TwelveNotches => "twelve_notches",
            Self::TwentyNotches => "twenty_notches",
        }
    }
}

impl std::fmt::Display for BossBarDivision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One bit of the boss bar flags byte.
///
/// The serde names match the identifiers used by Adventure/Paper
/// (`darken_sky`, `play_boss_music`, `create_fog`) so the ABI round-trips
/// cleanly against those ecosystems.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BossBarFlag {
    /// `0x01` — darken the world sky while the bar is visible.
    DarkenSky,
    /// `0x02` — mark this as a dragon bar (plays the dragon boss music).
    PlayBossMusic,
    /// `0x04` — create fog while the bar is visible.
    CreateFog,
}

impl BossBarFlag {
    /// The bit of the flag in the flags byte.
    pub const fn bit(&self) -> u8 {
        match self {
            Self::DarkenSky => 0x01,
            Self::PlayBossMusic => 0x02,
            Self::CreateFog => 0x04,
        }
    }

    /// Decodes one bit position into a flag, if known.
    pub fn from_bit(bit: u8) -> Option<Self> {
        match bit {
            0x01 => Some(Self::DarkenSky),
            0x02 => Some(Self::PlayBossMusic),
            0x04 => Some(Self::CreateFog),
            _ => None,
        }
    }

    /// The stable wire name of the flag, matching the serde
    /// representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DarkenSky => "darken_sky",
            Self::PlayBossMusic => "play_boss_music",
            Self::CreateFog => "create_fog",
        }
    }
}

impl std::fmt::Display for BossBarFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The flags byte of a boss bar: a `u8` bit mask of [`BossBarFlag`]s.
///
/// Serializes transparently as the raw byte, matching the wire exactly.
/// Unknown bits round-trip untouched; [`BossBarFlags::flags`] only yields
/// the known ones.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BossBarFlags(u8);

impl BossBarFlags {
    /// No flags set.
    pub const NONE: Self = Self(0);
    /// The `darken_sky` bit.
    pub const DARKEN_SKY: Self = Self(BossBarFlag::DarkenSky.bit());
    /// The `play_boss_music` bit.
    pub const PLAY_BOSS_MUSIC: Self = Self(BossBarFlag::PlayBossMusic.bit());
    /// The `create_fog` bit.
    pub const CREATE_FOG: Self = Self(BossBarFlag::CreateFog.bit());

    /// Creates flags from a raw byte. Bits other than the known ones are
    /// preserved untouched.
    pub fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The raw byte for the wire.
    pub fn bits(&self) -> u8 {
        self.0
    }

    /// Whether `flag` is set.
    pub fn contains(&self, flag: BossBarFlag) -> bool {
        self.0 & flag.bit() != 0
    }

    /// Sets `flag`.
    pub fn insert(&mut self, flag: BossBarFlag) {
        self.0 |= flag.bit();
    }

    /// Clears `flag`.
    pub fn remove(&mut self, flag: BossBarFlag) {
        self.0 &= !flag.bit();
    }

    /// Whether no flags are set.
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Iterates the known flags that are set.
    pub fn flags(&self) -> impl Iterator<Item = BossBarFlag> {
        [
            BossBarFlag::DarkenSky,
            BossBarFlag::PlayBossMusic,
            BossBarFlag::CreateFog,
        ]
        .into_iter()
        .filter(move |flag| self.contains(*flag))
    }
}

impl From<BossBarFlag> for BossBarFlags {
    fn from(flag: BossBarFlag) -> Self {
        Self(flag.bit())
    }
}

impl std::fmt::Display for BossBarFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        for flag in self.flags() {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(flag.as_str());
        }
        let unknown = self.0 & !0b111;
        if unknown != 0 {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(&format!("0x{unknown:02X}"));
        }
        if out.is_empty() {
            out.push('-');
        }
        f.write_str(&out)
    }
}

/// What one boss bar packet tells the client to do.
///
/// The discriminant ids match the protocol exactly (`add = 0`,
/// `remove = 1`, `update_health = 2`, `update_title = 3`,
/// `update_style = 4`, `update_flags = 5`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BossBarAction {
    /// Create a new bar (id `0`).
    Add {
        /// The text shown on the bar.
        title: Component,
        /// The fill fraction, `0.0..=1.0`. See the module docs.
        health: f32,
        /// The bar color.
        color: BossBarColor,
        /// How the bar is divided into notches.
        division: BossBarDivision,
        /// The option flags (darken sky / dragon bar / fog).
        flags: BossBarFlags,
    },
    /// Remove the bar (id `1`). No fields.
    Remove,
    /// Update the fill fraction (id `2`).
    UpdateHealth {
        /// The fill fraction, `0.0..=1.0`. See the module docs.
        health: f32,
    },
    /// Update the text shown on the bar (id `3`).
    UpdateTitle {
        /// The new text component.
        title: Component,
    },
    /// Update the bar color and notch division (id `4`).
    UpdateStyle {
        /// The new color.
        color: BossBarColor,
        /// The new division.
        division: BossBarDivision,
    },
    /// Update the option flags (id `5`).
    UpdateFlags {
        /// The new flags.
        flags: BossBarFlags,
    },
}

impl BossBarAction {
    /// The wire discriminant of the action.
    pub fn id(&self) -> i32 {
        match self {
            Self::Add { .. } => 0,
            Self::Remove => 1,
            Self::UpdateHealth { .. } => 2,
            Self::UpdateTitle { .. } => 3,
            Self::UpdateStyle { .. } => 4,
            Self::UpdateFlags { .. } => 5,
        }
    }

    /// The stable wire name of the action, matching the serde
    /// representation (`"add"`, `"remove"`, `"update_health"`, ...).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add",
            Self::Remove => "remove",
            Self::UpdateHealth { .. } => "update_health",
            Self::UpdateTitle { .. } => "update_title",
            Self::UpdateStyle { .. } => "update_style",
            Self::UpdateFlags { .. } => "update_flags",
        }
    }

    /// Checks that the action is encodable.
    ///
    /// The only failure mode is an out-of-range or non-finite health:
    /// `Add` and `UpdateHealth` require `0.0..=1.0` inclusive.
    pub fn validate(&self) -> Result<(), VisualPacketError> {
        match self {
            Self::Add { health, .. } | Self::UpdateHealth { health } => check_health(*health),
            _ => Ok(()),
        }
    }
}

impl std::fmt::Display for BossBarAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Add {
                health,
                color,
                division,
                flags,
                ..
            } => write!(
                f,
                "add(health={health:.2}, color={color}, division={division}, flags={flags})"
            ),
            Self::Remove => f.write_str("remove"),
            Self::UpdateHealth { health } => write!(f, "update_health(health={health:.2})"),
            Self::UpdateTitle { .. } => f.write_str("update_title(..)"),
            Self::UpdateStyle { color, division } => {
                write!(f, "update_style(color={color}, division={division})")
            }
            Self::UpdateFlags { flags } => write!(f, "update_flags(flags={flags})"),
        }
    }
}

/// A boss bar packet body: the bar's UUID plus one action.
///
/// # Wire format
///
/// `UUID` (two big-endian `i64`s) then the action discriminant as a
/// `VarInt`, then the action payload. There is no packet id here — the
/// proxy writes it, resolved from its per-version registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BossBarPacket {
    /// The UUID identifying the bar. The same UUID addresses the bar for
    /// every action after `Add`.
    pub uuid: Uuid,
    /// The action to apply.
    pub action: BossBarAction,
}

impl BossBarPacket {
    /// Checks that the packet is encodable. Runs the action's own
    /// validation.
    pub fn validate(&self) -> Result<(), VisualPacketError> {
        self.action.validate()
    }

    /// Encodes the packet body (no packet id, no frame) into `dst`.
    pub fn encode(&self, dst: &mut impl BufMut) -> Result<(), VisualPacketError> {
        self.validate()?;
        write_uuid_wire(dst, self.uuid);
        write_varint(self.action.id(), dst);
        match &self.action {
            BossBarAction::Add {
                title,
                health,
                color,
                division,
                flags,
            } => {
                write_component(dst, title)?;
                dst.put_f32(*health);
                write_varint(color.id(), dst);
                write_varint(division.id(), dst);
                dst.put_u8(flags.bits());
            }
            BossBarAction::Remove => {}
            BossBarAction::UpdateHealth { health } => {
                dst.put_f32(*health);
            }
            BossBarAction::UpdateTitle { title } => {
                write_component(dst, title)?;
            }
            BossBarAction::UpdateStyle { color, division } => {
                write_varint(color.id(), dst);
                write_varint(division.id(), dst);
            }
            BossBarAction::UpdateFlags { flags } => {
                dst.put_u8(flags.bits());
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
    /// Fails on malformed discriminants, non-finite health, or leftover
    /// bytes.
    pub fn decode(src: &mut impl Buf) -> Result<Self, VisualPacketError> {
        let uuid = read_uuid_wire(src)?;
        let action_id = read_varint(src)?;
        let action = match action_id {
            0 => {
                let title = read_component(src)?;
                let health = read_health(src)?;
                let color = BossBarColor::from_id(read_varint(src)?)?;
                let division = BossBarDivision::from_id(read_varint(src)?)?;
                let flags = BossBarFlags::from_bits(read_u8(src)?);
                BossBarAction::Add {
                    title,
                    health,
                    color,
                    division,
                    flags,
                }
            }
            1 => BossBarAction::Remove,
            2 => BossBarAction::UpdateHealth {
                health: read_health(src)?,
            },
            3 => BossBarAction::UpdateTitle {
                title: read_component(src)?,
            },
            4 => BossBarAction::UpdateStyle {
                color: BossBarColor::from_id(read_varint(src)?)?,
                division: BossBarDivision::from_id(read_varint(src)?)?,
            },
            5 => BossBarAction::UpdateFlags {
                flags: BossBarFlags::from_bits(read_u8(src)?),
            },
            other => {
                return Err(VisualPacketError::UnknownDiscriminant {
                    what: "boss bar action",
                    found: other,
                });
            }
        };
        ensure_exhausted(src)?;
        Ok(Self { uuid, action })
    }

    /// Decodes a packet body from a byte slice, fully consuming it.
    pub fn from_slice(src: &[u8]) -> Result<Self, VisualPacketError> {
        let mut src = src;
        Self::decode(&mut src)
    }
}

impl std::fmt::Display for BossBarPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BossBar({}, {})", self.uuid, self.action)
    }
}

/// Encapsulates the health validation shared by encode and decode.
fn check_health(health: f32) -> Result<(), VisualPacketError> {
    if !health.is_finite() || !(0.0..=1.0).contains(&health) {
        return Err(VisualPacketError::HealthOutOfRange(health));
    }
    Ok(())
}

/// Reads a health float, rejecting non-finite values but passing
/// finite out-of-range values through (see the module docs).
fn read_health(src: &mut impl Buf) -> Result<f32, VisualPacketError> {
    if src.remaining() < 4 {
        return Err(candyriya_protocol::ProtocolError::Truncated.into());
    }
    let health = src.get_f32();
    if health.is_nan() || health.is_infinite() {
        return Err(VisualPacketError::HealthNotFinite);
    }
    Ok(health)
}
