//! Wire vocabulary for the controller <-> plugin-unit channel.
//!
//! These types are *internal* to the controller and its plugin units: they
//! travel through the supervision tree as type-erased envelopes (the tree's
//! capability IPC downcasts on the callee), never as serialized bytes.
//! There is deliberately no serde here and no wire format to version —
//! both sides are in-process and rebuilt from the same source.
//!
//! [`Generation`] is the only ordering primitive: every request carries a
//! generation minted for the unit's current incarnation, and acknowledges
//! echo it back. A plugin unit (or a stale ack from a previous one) is
//! rejected by the *caller* on generation mismatch — this crate only
//! provides the tag shape and its monotony.

use std::fmt;

/// Maximum size of an [`AckStatus::Failure`] message, in bytes.
///
/// Ack text travels into logs, metrics labels' siblings and ps; a plugin
/// that wants to leak a whole backtrace into the controller must not be
/// able to. Longer messages are truncated at a char boundary.
pub const MAX_ACK_MESSAGE_BYTES: usize = 1024;

/// Truncates to at most `max` bytes, never splitting a char.
fn truncate(message: &str, max: usize) -> &str {
    if message.len() <= max {
        return message;
    }
    let mut end = max;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

/// A monotonically increasing tag for unit incarnations.
///
/// Bumped whenever the unit (or a staged replacement for it) is spawned,
/// and echoed back on every acknowledgement. Monotonicity is the whole
/// point: an ack carrying an older generation belongs to a previous
/// incarnation and is dropped by the caller, never honored.
///
/// Overflow saturates at `u64::MAX` instead of wrapping — a wrapped tag
/// could compare *equal* to an older generation and defeat the staleness
/// check entirely. Reaching `u64::MAX` means 2^64 spawns happened; the
/// saturation is the honest outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Generation(u64);

impl Generation {
    /// The generation every fresh controller starts at.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw value. The caller owns the invariant that generations
    /// only move up.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw numeric value.
    pub const fn value(self) -> u64 {
        self.0
    }

    /// The next generation. Saturates at `u64::MAX`.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gen{}", self.0)
    }
}

/// A controller -> plugin-unit message.
///
/// One variant per lifecycle verb; the payload is a single opaque tag the
/// unit must echo on its acknowledgement. The set is closed — verbs are
/// added to this enum, never invented at call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginMsg {
    /// Mint the context, dlopen the library, run the ABI handshake.
    Load {
        /// The generation this incarnation was minted with.
        generation: Generation,
    },
    /// Run the plugin's init callback with the context.
    Init {
        /// The generation this incarnation was minted with.
        generation: Generation,
    },
    /// Run the plugin's enable callback.
    Enable {
        /// The generation this incarnation was minted with.
        generation: Generation,
    },
    /// Cooperative stop: pause, drain, run the disable callback.
    Disable {
        /// The generation this incarnation was minted with.
        generation: Generation,
    },
    /// Final callback, then teardown: mailbox close, bounded join, drop
    /// the context, close the mapping.
    Unload {
        /// The generation this incarnation was minted with.
        generation: Generation,
    },
}

impl PluginMsg {
    /// The tag this message carries, whatever the verb.
    pub const fn generation(&self) -> Generation {
        match self {
            Self::Load { generation }
            | Self::Init { generation }
            | Self::Enable { generation }
            | Self::Disable { generation }
            | Self::Unload { generation } => *generation,
        }
    }
}

/// How a plugin answered one verb.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AckStatus {
    /// The call ran to completion.
    Success,
    /// The call failed; carries a bounded human-readable message.
    Failure {
        /// Bounded to [`MAX_ACK_MESSAGE_BYTES`].
        message: String,
    },
}

impl AckStatus {
    /// Builds a failure with a message bounded to
    /// [`MAX_ACK_MESSAGE_BYTES`] at a char boundary.
    pub fn failure(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::Failure {
            message: truncate(&message, MAX_ACK_MESSAGE_BYTES).to_owned(),
        }
    }

    /// Whether the call succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

impl fmt::Display for AckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => f.write_str("ok"),
            Self::Failure { message } => write!(f, "failure({message})"),
        }
    }
}

/// A plugin-unit -> controller acknowledgement.
///
/// Mirrors [`PluginMsg`] one-to-one so an ack's verb is read off the
/// variant, never off a payload. Each carries the echoed generation, the
/// bounded status and the wall-clock duration the unit measured for the
/// call (the controller turns that into metrics).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PluginAck {
    /// Answer to [`PluginMsg::Load`].
    Load {
        /// The echoed tag.
        generation: Generation,
        /// Whether the load succeeded.
        status: AckStatus,
        /// How long the call took, as seen by the unit.
        duration_ms: u64,
    },
    /// Answer to [`PluginMsg::Init`].
    Init {
        /// The echoed tag.
        generation: Generation,
        /// Whether the init callback succeeded.
        status: AckStatus,
        /// How long the call took, as seen by the unit.
        duration_ms: u64,
    },
    /// Answer to [`PluginMsg::Enable`].
    Enable {
        /// The echoed tag.
        generation: Generation,
        /// Whether the enable callback succeeded.
        status: AckStatus,
        /// How long the call took, as seen by the unit.
        duration_ms: u64,
    },
    /// Answer to [`PluginMsg::Disable`].
    Disable {
        /// The echoed tag.
        generation: Generation,
        /// Whether the disable callback succeeded.
        status: AckStatus,
        /// How long the call took, as seen by the unit.
        duration_ms: u64,
    },
    /// Answer to [`PluginMsg::Unload`].
    Unload {
        /// The echoed tag.
        generation: Generation,
        /// Whether the teardown succeeded (a detached thread is a
        /// `Success` with a detached marker upstream, not a failure).
        status: AckStatus,
        /// How long the call took, as seen by the unit.
        duration_ms: u64,
    },
}

impl PluginAck {
    /// The echoed tag.
    pub const fn generation(&self) -> Generation {
        match self {
            Self::Load { generation, .. }
            | Self::Init { generation, .. }
            | Self::Enable { generation, .. }
            | Self::Disable { generation, .. }
            | Self::Unload { generation, .. } => *generation,
        }
    }

    /// How the unit judged its own call.
    pub const fn status(&self) -> &AckStatus {
        match self {
            Self::Load { status, .. }
            | Self::Init { status, .. }
            | Self::Enable { status, .. }
            | Self::Disable { status, .. }
            | Self::Unload { status, .. } => status,
        }
    }

    /// How long the call took, as seen by the unit.
    pub const fn duration_ms(&self) -> u64 {
        match self {
            Self::Load { duration_ms, .. }
            | Self::Init { duration_ms, .. }
            | Self::Enable { duration_ms, .. }
            | Self::Disable { duration_ms, .. }
            | Self::Unload { duration_ms, .. } => *duration_ms,
        }
    }

    /// Whether the unit judged its own call a success.
    pub fn is_success(&self) -> bool {
        self.status().is_success()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_monotonic() {
        let zero = Generation::ZERO;
        let one = zero.next();
        let two = one.next();
        assert_eq!(zero.value(), 0);
        assert_eq!(one.value(), 1);
        assert_eq!(two.value(), 2);
        assert!(zero < one);
        assert!(one < two);
        assert_eq!(Generation::new(9).next().value(), 10);
    }

    #[test]
    fn generation_saturates_instead_of_wrapping() {
        let max = Generation::new(u64::MAX);
        assert_eq!(max.next().value(), u64::MAX);
        assert_eq!(max.next(), max);
    }

    #[test]
    fn msg_echoes_its_tag() {
        let tag = Generation::new(7);
        let msgs = [
            PluginMsg::Load { generation: tag },
            PluginMsg::Init { generation: tag },
            PluginMsg::Enable { generation: tag },
            PluginMsg::Disable { generation: tag },
            PluginMsg::Unload { generation: tag },
        ];
        assert!(msgs.iter().all(|m| m.generation() == tag));
    }

    #[test]
    fn ack_carries_the_triple() {
        let ack = PluginAck::Enable {
            generation: Generation::new(3),
            status: AckStatus::Success,
            duration_ms: 17,
        };
        assert_eq!(ack.generation().value(), 3);
        assert!(ack.is_success());
        assert_eq!(ack.duration_ms(), 17);
        assert_eq!(ack.status(), &AckStatus::Success);
    }

    #[test]
    fn failure_message_is_bounded() {
        let huge = "x".repeat(MAX_ACK_MESSAGE_BYTES * 4);
        let AckStatus::Failure { message } = AckStatus::failure(huge) else {
            panic!("expected a failure status");
        };
        assert!(message.len() <= MAX_ACK_MESSAGE_BYTES);

        // multi-byte chars must never be split
        let cyrillic = "я".repeat(MAX_ACK_MESSAGE_BYTES);
        let AckStatus::Failure { message } = AckStatus::failure(cyrillic) else {
            panic!("expected a failure status");
        };
        assert!(message.len() <= MAX_ACK_MESSAGE_BYTES);
        assert!(message.chars().all(|c| c == 'я'));
    }

    #[test]
    fn failure_message_display() {
        let status = AckStatus::failure("nope");
        assert_eq!(status.to_string(), "failure(nope)");
        assert_eq!(AckStatus::Success.to_string(), "ok");
    }
}
