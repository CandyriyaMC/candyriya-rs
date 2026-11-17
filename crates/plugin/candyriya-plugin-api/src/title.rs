//! The title packet bodies: the splash across the screen, the subtitle
//! line under it, the action bar above the hotbar, the timer controls and
//! the reset.
//!
//! Since Minecraft 1.17 the title effect packets (previously a single
//! packet with an action byte distinguishing title, subtitle, action
//! bar, clear, reset and times) were split into one packet per effect.
//! This module models the modern split: every [`TitleKind`] corresponds
//! to a separate packet on 1.17+ clients. The proxy picks the packet id
//! for the negotiated protocol version from its own registry — this
//! crate never writes packet ids, only the fields after them.
//!
//! Modern packet bodies (verified against the wiki, 2026-11-17):
//!
//! - `Title` / `Subtitle` / `ActionBar`: one `Text Component` field.
//! - `TitleTimes`: three big-endian `Int` fields — fade in, stay, fade
//!   out — all in **ticks** (20 ticks = 1 second). They are `Int`, not
//!   milliseconds and not `VarInt`.
//! - `Clear` / `Reset`: a single `Boolean` field, `false` for clear
//!   (forget the currently shown titles) and `true` for reset (also
//!   restore the default timings and title state). Both map onto the
//!   modern `clear_titles` packet.
//!
//! A note on JSON text components: since Minecraft 1.20.3 parts of the
//! protocol carry components as NBT instead of JSON strings. The proxy is
//! responsible for that translation when talking to such clients; this
//! crate always writes the JSON form, matching the `Text Component` field
//! as documented for the string encoding.
//!
//! Everything here is plain data — `Serde` derives are provided so the
//! same types can travel across the plugin ABI as JSON. The wire format
//! itself is built by [`TitlePacket::encode`] and consumed by
//! [`TitlePacket::decode`], both of which enforce the field presence rules
//! strictly (a `Title` kind without text fails, a `TitleTimes` kind with
//! text fails, and so on).

use bytes::{Buf, BufMut};
use lobsterchat::component::Component;
use serde::{Deserialize, Serialize};

use crate::wire::{
    VisualPacketError, ensure_exhausted, read_component, read_int, read_u8, write_bool,
    write_component, write_int,
};

/// Which title packet a [`TitlePacket`] describes.
///
/// The kind selects both the packet id the proxy must write (per version)
/// and the body layout:
///
/// | Kind | Body | Notes |
/// |---|---|---|
/// | `Title` | component | Big title text |
/// | `Subtitle` | component | Small text under the title |
/// | `ActionBar` | component | Line above the hotbar |
/// | `Clear` | `false` bool | Drop the currently displayed titles |
/// | `Reset` | `true` bool | Clear **and** restore default timings |
/// | `TitleTimes` | 3 × `Int` | Fade in / stay / fade out, in ticks |
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleKind {
    /// The big title text.
    Title,
    /// The smaller line underneath the title.
    Subtitle,
    /// The line above the hotbar.
    ActionBar,
    /// Clear the currently displayed titles (`false` reset flag).
    Clear,
    /// Clear the titles and restore the default state (`true` reset flag).
    Reset,
    /// Update the display timings of all titles.
    TitleTimes,
}

impl TitleKind {
    /// The stable wire name of the kind (`"title"`, `"subtitle"`,
    /// `"action_bar"`, `"clear"`, `"reset"`, `"title_times"`), matching the
    /// serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Subtitle => "subtitle",
            Self::ActionBar => "action_bar",
            Self::Clear => "clear",
            Self::Reset => "reset",
            Self::TitleTimes => "title_times",
        }
    }
}

impl std::fmt::Display for TitleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The display timings of the title animation.
///
/// All fields are big-endian `Int`s on the wire, counted in **ticks**
/// (20 ticks = 1 second), matching the protocol spec for the titles
/// animation packet. Millisecond helpers are provided for callers that
/// think in wall-clock terms: [`TitleTimes::to_millis`] converts ticks to
/// milliseconds (1 tick = 50 ms) and [`TitleTimes::from_millis`] converts
/// back with rounding to the nearest tick.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleTimes {
    /// Ticks to spend fading the title in.
    pub fade_in_ticks: i32,
    /// Ticks to keep the title fully displayed.
    pub stay_ticks: i32,
    /// Ticks to spend fading the title out.
    pub fade_out_ticks: i32,
}

impl TitleTimes {
    /// Ticks per second in the game.
    pub const TICKS_PER_SECOND: i32 = 20;
    /// Milliseconds per tick.
    pub const MILLIS_PER_TICK: i32 = 50;

    /// Creates the timings from tick values.
    pub fn new(fade_in_ticks: i32, stay_ticks: i32, fade_out_ticks: i32) -> Self {
        Self {
            fade_in_ticks,
            stay_ticks,
            fade_out_ticks,
        }
    }

    /// Creates the timings from millisecond values, rounding each to the
    /// nearest tick.
    pub fn from_millis(fade_in_ms: i32, stay_ms: i32, fade_out_ms: i32) -> Self {
        fn to_ticks(ms: i32) -> i32 {
            // round half away from zero, computed in i64 and clamped to
            // i32 so extreme inputs saturate instead of wrapping
            let ms = i64::from(ms);
            let rounded = if ms >= 0 {
                (ms + 25) / 50
            } else {
                -((-ms + 25) / 50)
            };
            rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        }
        Self {
            fade_in_ticks: to_ticks(fade_in_ms),
            stay_ticks: to_ticks(stay_ms),
            fade_out_ticks: to_ticks(fade_out_ms),
        }
    }

    /// The timings in milliseconds `(fade_in, stay, fade_out)`.
    ///
    /// Computed in `i64`; the products cannot overflow.
    pub fn to_millis(&self) -> (i64, i64, i64) {
        (
            i64::from(self.fade_in_ticks) * i64::from(Self::MILLIS_PER_TICK),
            i64::from(self.stay_ticks) * i64::from(Self::MILLIS_PER_TICK),
            i64::from(self.fade_out_ticks) * i64::from(Self::MILLIS_PER_TICK),
        )
    }

    /// The total time the title occupies the screen, in ticks.
    ///
    /// Computed in `i64`; the sum cannot overflow.
    pub fn total_ticks(&self) -> i64 {
        i64::from(self.fade_in_ticks) + i64::from(self.stay_ticks) + i64::from(self.fade_out_ticks)
    }

    /// The total time the title occupies the screen, in milliseconds.
    pub fn total_millis(&self) -> i64 {
        self.total_ticks() * i64::from(Self::MILLIS_PER_TICK)
    }
}

impl Default for TitleTimes {
    /// The vanilla default timings: 10 ticks to fade in, 70 to stay,
    /// 20 to fade out.
    fn default() -> Self {
        Self::new(10, 70, 20)
    }
}

impl std::fmt::Display for TitleTimes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Times(fade_in={}t, stay={}t, fade_out={}t)",
            self.fade_in_ticks, self.stay_ticks, self.fade_out_ticks
        )
    }
}

/// A title packet body, selected by [`TitleKind`].
///
/// Only the fields the kind carries may be set; the encoder enforces
/// this strictly, so a half-filled value cannot cross the ABI silently.
/// Use the constructor helpers ([`TitlePacket::title`],
/// [`TitlePacket::subtitle`], [`TitlePacket::action_bar`],
/// [`TitlePacket::clear`], [`TitlePacket::reset`],
/// [`TitlePacket::times`]) to build a valid value in one call.
///
/// # Wire format
///
/// See the module docs for the per-kind layouts. The packet id is **not**
/// part of this type — the proxy writes it, resolved from its per-version
/// registry. The pre-1.17 single `Clientbound Title` packet (action byte
/// `0..=5` inside one packet) is intentionally not modelled; this crate is
/// modern-only, matching the proxy's passthrough stance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitlePacket {
    /// Which packet this describes.
    pub kind: TitleKind,
    /// The text component for the `Title` / `Subtitle` / `ActionBar`
    /// kinds; must be `None` for the other kinds. Serialized only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<Component>,
    /// The timings for the `TitleTimes` kind; must be `None` for the
    /// other kinds. Serialized only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub times: Option<TitleTimes>,
}

impl TitlePacket {
    /// Checks that the field combination is valid for the kind.
    ///
    /// Returns the empty `Ok(())` for a constructible packet, or a
    /// [`VisualPacketError`] describing exactly what is off. This runs
    /// implicitly in [`TitlePacket::encode`] and [`TitlePacket::decode`];
    /// call it separately when validating ABI input before doing work.
    pub fn validate(&self) -> Result<(), VisualPacketError> {
        match self.kind {
            TitleKind::Title | TitleKind::Subtitle | TitleKind::ActionBar => {
                if self.text.is_none() {
                    return Err(VisualPacketError::MissingText { kind: self.kind });
                }
                if self.times.is_some() {
                    return Err(VisualPacketError::UnexpectedTimes { kind: self.kind });
                }
            }
            TitleKind::Clear | TitleKind::Reset => {
                if self.text.is_some() {
                    return Err(VisualPacketError::UnexpectedText { kind: self.kind });
                }
                if self.times.is_some() {
                    return Err(VisualPacketError::UnexpectedTimes { kind: self.kind });
                }
            }
            TitleKind::TitleTimes => {
                if self.times.is_none() {
                    return Err(VisualPacketError::MissingTimes { kind: self.kind });
                }
                if self.text.is_some() {
                    return Err(VisualPacketError::UnexpectedText { kind: self.kind });
                }
            }
        }
        Ok(())
    }

    /// Creates a `Title` packet with the given component.
    pub fn title(text: Component) -> Self {
        Self {
            kind: TitleKind::Title,
            text: Some(text),
            times: None,
        }
    }

    /// Creates a `Subtitle` packet with the given component.
    pub fn subtitle(text: Component) -> Self {
        Self {
            kind: TitleKind::Subtitle,
            text: Some(text),
            times: None,
        }
    }

    /// Creates an `ActionBar` packet with the given component.
    pub fn action_bar(text: Component) -> Self {
        Self {
            kind: TitleKind::ActionBar,
            text: Some(text),
            times: None,
        }
    }

    /// Creates a `Clear` packet (forget the currently shown titles, keep
    /// the timings).
    pub fn clear() -> Self {
        Self {
            kind: TitleKind::Clear,
            text: None,
            times: None,
        }
    }

    /// Creates a `Reset` packet (clear the titles and restore the default
    /// timings).
    pub fn reset() -> Self {
        Self {
            kind: TitleKind::Reset,
            text: None,
            times: None,
        }
    }

    /// Creates a `TitleTimes` packet with the given timings.
    pub fn times(times: TitleTimes) -> Self {
        Self {
            kind: TitleKind::TitleTimes,
            text: None,
            times: Some(times),
        }
    }

    /// Encodes the packet body (no packet id, no frame) into `dst`.
    ///
    /// Runs [`TitlePacket::validate`] first, so invalid combinations are
    /// rejected before a single byte is written.
    pub fn encode(&self, dst: &mut impl BufMut) -> Result<(), VisualPacketError> {
        self.validate()?;
        match self.kind {
            TitleKind::Title | TitleKind::Subtitle | TitleKind::ActionBar => {
                match &self.text {
                    Some(text) => write_component(dst, text),
                    // unreachable after validate(); reported, never panicked
                    None => Err(VisualPacketError::MissingText { kind: self.kind }),
                }
            }
            TitleKind::Clear => {
                write_bool(dst, false);
                Ok(())
            }
            TitleKind::Reset => {
                write_bool(dst, true);
                Ok(())
            }
            TitleKind::TitleTimes => match &self.times {
                Some(times) => {
                    write_int(dst, times.fade_in_ticks);
                    write_int(dst, times.stay_ticks);
                    write_int(dst, times.fade_out_ticks);
                    Ok(())
                }
                None => Err(VisualPacketError::MissingTimes { kind: self.kind }),
            },
        }
    }

    /// Encodes the packet body (no packet id, no frame) into a fresh `Vec`.
    pub fn to_vec(&self) -> Result<Vec<u8>, VisualPacketError> {
        let mut buf = bytes::BytesMut::new();
        self.encode(&mut buf)?;
        Ok(buf.to_vec())
    }

    /// Decodes a packet body (the bytes after the packet id) into a
    /// [`TitlePacket`] of the given `kind`.
    ///
    /// The kind comes from the packet id the proxy resolved for the
    /// negotiated protocol version; the body itself does not carry it.
    /// Fails if the body is malformed, if fewer or more bytes than
    /// expected remain, or — for the `Clear` / `Reset` kinds — if the
    /// reset flag does not match the requested kind.
    pub fn decode(kind: TitleKind, src: &mut impl Buf) -> Result<Self, VisualPacketError> {
        let packet = match kind {
            TitleKind::Title | TitleKind::Subtitle | TitleKind::ActionBar => {
                let text = read_component(src)?;
                Self {
                    kind,
                    text: Some(text),
                    times: None,
                }
            }
            TitleKind::Clear | TitleKind::Reset => {
                let found = read_u8(src)?;
                let expected = matches!(kind, TitleKind::Reset);
                match found {
                    0x00 | 0x01 => {
                        if found != u8::from(expected) {
                            return Err(VisualPacketError::ResetFlagMismatch { kind, found });
                        }
                    }
                    other => return Err(VisualPacketError::InvalidBoolean(other)),
                }
                Self {
                    kind,
                    text: None,
                    times: None,
                }
            }
            TitleKind::TitleTimes => {
                let fade_in_ticks = read_int(src)?;
                let stay_ticks = read_int(src)?;
                let fade_out_ticks = read_int(src)?;
                Self {
                    kind,
                    text: None,
                    times: Some(TitleTimes::new(fade_in_ticks, stay_ticks, fade_out_ticks)),
                }
            }
        };
        ensure_exhausted(src)?;
        Ok(packet)
    }

    /// Decodes a packet body from a byte slice, fully consuming it.
    ///
    /// Convenience wrapper around [`TitlePacket::decode`] for slices;
    /// fails with [`VisualPacketError::TrailingBytes`] if `src` holds more
    /// bytes than the kind needs.
    pub fn from_slice(kind: TitleKind, src: &[u8]) -> Result<Self, VisualPacketError> {
        let mut src = src;
        Self::decode(kind, &mut src)
    }
}

impl std::fmt::Display for TitlePacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            TitleKind::Title | TitleKind::Subtitle | TitleKind::ActionBar => match &self.text {
                Some(text) => {
                    let json_len = serde_json::to_string(text)
                        .map(|json| json.len())
                        .unwrap_or(0);
                    write!(f, "Title({}, text_json_bytes={json_len})", self.kind)
                }
                None => write!(f, "Title({}, text=<missing>)", self.kind),
            },
            TitleKind::Clear => f.write_str("Title(clear)"),
            TitleKind::Reset => f.write_str("Title(reset)"),
            TitleKind::TitleTimes => match &self.times {
                Some(times) => write!(f, "Title({times})"),
                None => f.write_str("Title(times=<missing>)"),
            },
        }
    }
}
