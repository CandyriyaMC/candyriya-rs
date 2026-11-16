//! The `PluginMessageEvent` model: the two results, the two possible
//! sources and the event that links channel, source, payload and result.
//!
//! Velocity fires exactly one event kind for plugin messages and lets the
//! [`ForwardResult`] attached to it decide what happens next — the proxy
//! waits for the event to finish firing, then either discards the message
//! (handled) or forwards it to the other side (forward). This module is a
//! faithful Rust model of that machinery; it performs no I/O and holds no
//! state beyond the event itself.
//!
//! # Leak prevention
//!
//! **If you do not intend the other side to participate, mark the event
//! [`handled`](PluginMessageEvent::handled).** A forwarded message travels on
//! as if the proxy were not present; a handled message is discarded.
//!
//! The exact pattern from the upstream documentation, which this crate
//! endorses and its examples follow:
//!
//! 1. Check the channel identifier first, no matter the source.
//! 2. If it matches what you care about, set the result to
//!    [`ForwardResult::Handled`] *before* parsing anything.
//! 3. Only then inspect the source and parse the data.
//!
//! Setting the result after parsing is a race-shaped habit: if your parsing
//! bails out early (wrong source, unknown subcommand, malformed bytes) the
//! event keeps whatever default it was created with and leaks the message to
//! the other side. Setting it between the identifier check and the source
//! check closes that window completely — see [Case 1](super#case-1) and
//! [Case 3](super#case-3) for the full pattern and the warning from the
//! upstream docs about clients impersonating the proxy.

use core::fmt;

use bytes::Bytes;
use candyriya_messaging::ChannelIdentifier;
use uuid::Uuid;

/// What happens to a plugin message once the proxy finishes firing
/// [`PluginMessageEvent`].
///
/// This is the Rust image of Velocity's `PluginMessageEvent.ForwardResult`.
/// The two variants are the only results that exist; like upstream, the
/// default for a freshly created event is [`Forward`](Self::Forward)
/// (inherited unchanged from Velocity — see [`Default`] and the warning
/// below).
///
/// # Leak-prevention warning
///
/// When the channel matches, set the result to [`Handled`](Self::Handled)
/// **before parsing** if you do not want the message to participate on the
/// other side. Only unhandled messages leak to the destination. Marking
/// handled too late — or not at all — lets a client impersonate the proxy to
/// backend servers, or a backend server inject messages to clients,
/// depending on direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForwardResult {
    /// The proxy consumed the message; it is discarded. This is what the
    /// upstream docs recommend setting as soon as the channel matches,
    /// unconditionally, so that stray or spoofed messages never travel on.
    Handled,
    /// The message travels on to its destination as if the proxy were not
    /// present. Use only for pass-through traffic; anything sensitive for
    /// the proxy itself must be [`Handled`](Self::Handled) instead.
    Forward,
}

impl ForwardResult {
    /// Static constructor mirroring the upstream `ForwardResult.handled()`.
    ///
    /// Returns a copy of the singleton; the enum is `Copy`, so repeated
    /// calls are free and never observe different values.
    pub const fn handled() -> Self {
        Self::Handled
    }

    /// Static constructor mirroring the upstream `ForwardResult.forward()`.
    ///
    /// Returns a copy of the singleton; the enum is `Copy`, so repeated
    /// calls are free and never observe different values.
    pub const fn forward() -> Self {
        Self::Forward
    }

    /// Whether the message is allowed to travel on to its destination.
    ///
    /// Mirrors the upstream `isAllowed()`: `true` only for
    /// [`Forward`](Self::Forward).
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Forward)
    }

    /// Whether the proxy consumed the message (shorthand for `!is_allowed`).
    pub const fn is_handled(self) -> bool {
        matches!(self, Self::Handled)
    }
}

impl Default for ForwardResult {
    /// The upstream default for a freshly created event: [`Forward`](Self::Forward).
    ///
    /// This matches Velocity exactly (`PluginMessageEvent` initializes its
    /// result to `ForwardResult.forward()`). Construct the event with
    /// [`PluginMessageEvent::new_for_known_channel`] or call
    /// [`PluginMessageEvent::handled`] when the leak-prevention default is
    /// what you want — see the module docs.
    fn default() -> Self {
        Self::Forward
    }
}

impl fmt::Display for ForwardResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handled => f.write_str("handled at proxy"),
            Self::Forward => f.write_str("forward to destination"),
        }
    }
}

/// The side the plugin message originated from.
///
/// Plugin messages arrive at the proxy from exactly two places: a connected
/// **player** (client) or a connected **backend server**. The variant split
/// mirrors the two concrete event sources in upstream Velocity (`Player`
/// and `ServerConnection`), folded into a single plain enum so this crate
/// stays free of proxy-runtime dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MessageSource {
    /// The message came from a player's client.
    ///
    /// `uuid` is optional because the proxy may fire the event before the
    /// player's identity is fully established on every code path (for
    /// example during configuration phase); downstream handlers that need a
    /// guaranteed UUID should treat `None` as "not available yet", not as an
    /// error.
    Player {
        /// The player's current in-game name.
        name: String,
        /// The player's UUID, when known.
        uuid: Option<Uuid>,
    },
    /// The message came from a backend server the proxy holds a connection to.
    ServerBackend {
        /// The backend's registered name (its key in the proxy's server registry).
        name: String,
    },
}

impl MessageSource {
    /// The display name of the source, for both variants.
    pub fn name(&self) -> &str {
        match self {
            Self::Player { name, .. } | Self::ServerBackend { name } => name,
        }
    }

    /// Whether the source is a player's client.
    pub fn is_player(&self) -> bool {
        matches!(self, Self::Player { .. })
    }

    /// Whether the source is a backend server.
    pub fn is_backend(&self) -> bool {
        matches!(self, Self::ServerBackend { .. })
    }
}

/// The single event kind plugin messages produce at the proxy.
///
/// Created when a plugin message arrives from a client or a backend, on a
/// channel the proxy registered with its [`ChannelRegistrar`](super::ChannelRegistrar);
/// the proxy waits for handlers to finish, then consults [`result`](Self::result)
/// to decide whether to discard or forward the message.
///
/// Fires only for **registered** channels: messages on unregistered channels
/// bypass the event entirely and are either passed through untouched or
/// dropped — handlers never see them.
///
/// # Construction
///
/// - [`new`](Self::new) mirrors upstream exactly: result defaults to
///   [`ForwardResult::Forward`].
/// - [`new_for_known_channel`](Self::new_for_known_channel) picks the
///   leak-prevention default ([`ForwardResult::Handled`]) for channels the
///   proxy registered and manages itself. Prefer it unless you are building
///   a pass-through tunnel on purpose.
/// - [`with_result`](Self::with_result) is the builder-style combinator for
///   either default, for tests and composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMessageEvent {
    identifier: ChannelIdentifier,
    source: MessageSource,
    data: Bytes,
    result: ForwardResult,
}

impl PluginMessageEvent {
    /// Creates an event with the upstream default result, [`ForwardResult::Forward`].
    ///
    /// This is exactly what upstream does when firing `PluginMessageEvent`:
    /// the message is forwarded to its destination unless a handler calls
    /// [`handled`](Self::handled) or [`set_result`](Self::set_result).
    ///
    /// **Warning:** with this default, messages on channels nobody handles
    /// leak across the proxy untouched. If you are constructing an event for
    /// a channel the proxy registered for a purpose, use
    /// [`new_for_known_channel`](Self::new_for_known_channel) instead.
    pub fn new(identifier: ChannelIdentifier, source: MessageSource, data: Bytes) -> Self {
        Self {
            identifier,
            source,
            data,
            result: ForwardResult::forward(),
        }
    }

    /// Creates an event whose result defaults to [`ForwardResult::Handled`].
    ///
    /// This is the leak-prevention default for **known channels** — channels
    /// the proxy itself registered with its
    /// [`ChannelRegistrar`](super::ChannelRegistrar) and is expected to
    /// consume. Handlers that match on the identifier and then on the source
    /// still work unchanged; if nothing parses the data, the message is
    /// swallowed instead of leaking to the client or the backend.
    ///
    /// To build a deliberate pass-through for a known channel, construct with
    /// [`new`](Self::new) (or [`with_result`](Self::with_result) with
    /// [`ForwardResult::Forward`]) instead — that is an explicit choice, not
    /// an accident.
    pub fn new_for_known_channel(
        identifier: ChannelIdentifier,
        source: MessageSource,
        data: Bytes,
    ) -> Self {
        Self {
            identifier,
            source,
            data,
            result: ForwardResult::handled(),
        }
    }

    /// Builder-style combinator: set the result at construction time.
    ///
    /// Overwrites whatever default the event was created with.
    pub fn with_result(mut self, result: ForwardResult) -> Self {
        self.result = result;
        self
    }

    /// The channel the message arrived on.
    pub fn identifier(&self) -> &ChannelIdentifier {
        &self.identifier
    }

    /// The side the message originated from.
    pub fn source(&self) -> &MessageSource {
        &self.source
    }

    /// The raw payload.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the event and returns the payload.
    pub fn into_data(self) -> Bytes {
        self.data
    }

    /// The current result.
    pub fn result(&self) -> ForwardResult {
        self.result
    }

    /// Sets the result, mirroring the upstream `setResult`.
    ///
    /// Call this (or [`handled`](Self::handled)) before parsing data if you
    /// do not want the message forwarded — see the module-level leak
    /// prevention section.
    pub fn set_result(&mut self, result: ForwardResult) {
        self.result = result;
    }

    /// Marks the message handled: the proxy consumes it, nothing travels on.
    ///
    /// Convenience for `set_result(ForwardResult::Handled)`. This is the
    /// recommended call to make *after checking the identifier and before
    /// checking the source or parsing the data* — see the module docs.
    pub fn handled(&mut self) {
        self.set_result(ForwardResult::handled());
    }

    /// Marks the message for forwarding: it travels on as if the proxy were
    /// not present.
    pub fn forward(&mut self) {
        self.set_result(ForwardResult::forward());
    }

    /// Whether the current result is [`ForwardResult::Handled`].
    pub fn is_handled(&self) -> bool {
        self.result.is_handled()
    }

    /// Whether the current result allows the message to travel on.
    pub fn is_forwarded(&self) -> bool {
        self.result.is_allowed()
    }
}
