//! Typed game events the host dispatches into plugins.
//!
//! The flow is strictly one-directional: something happens in the proxy
//! (a connection was accepted, a player completed login, ...), the host
//! builds an [`EventEnvelope`], and fires it into the subscribed plugins.
//! Plugins react through the host functions the runtime provides and hand
//! back an [`OutcomeReport`] (or a compact [`EventCancellation`]); there
//! is no plugin to plugin dispatch here.
//!
//! ## Why structured kinds
//!
//! The event model mirrors the CancelEvent pattern: every dispatchable
//! event carries enough information for a plugin to act without asking
//! the host for more, and every dispatch can be cancelled. The kinds are
//! plain struct variants instead of opaque strings so that:
//!
//! - the payload *is* the kind; there is no stringly-typed `server`/`reason`
//!   side channel to populate by convention,
//! - the WASM boundary (and extism-convert on the plugin side) sees one
//!   flat, self-describing JSON document per event,
//! - matching on an event in plugin code is a real `match`, not a string
//!   comparison with a typo hazard.
//!
//! ## Wire format
//!
//! Structured payloads are JSON. A login envelope on the wire:
//!
//! ```json
//! {"id":1,"kind":"login","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","protocol_version":769,"occurred_at_unix_ms":1730000000000}
//! ```
//!
//! `kind` is the tag and is always present. Unknown tags survive a
//! round-trip through [`EventKind::Unknown`] instead of failing
//! deserialization, so plugins built against an older ABI keep working -
//! they simply cannot match on events they do not know. Fields this ABI
//! version does not know are ignored on the way in and never re-appear
//! on the way out.
//!
//! [`EventKind::Disconnect`] carries `reason` as a plain `String`, **not**
//! a chat component. The reason is carrier-free at the WASM edge (it is
//! often a bare backend-provided string); the proxy renders it as a
//! component of its choosing before showing it to a client.
//!
//! ## Cancellation model
//!
//! - The host mints monotonic event ids from an [`EventIdRegistry`]
//!   (ids start at 1 and increase by 1 per event; plugins never mint ids).
//! - A plugin answers a dispatch by sending back an [`OutcomeReport`]
//!   (`cancelled` + optional human-readable `cancel_reason`) or the more
//!   compact [`EventCancellation`] (`uuid` + `event_id` + `cancelled`).
//! - [`ProxyEvent`] is the host-side accumulator for one dispatch: it
//!   knows its id, its kind and whether a plugin cancelled it.
//!
//! [`MinecraftEvent`] ties the input and output of a dispatch together
//! for generic host code; it is a pure marker with associated types -
//! everything crossing the boundary stays a flat enum/struct from this
//! module, which is what keeps the WASM edge cheap.
//!
//! Allocation note: these types are passed by value in WASM context.
//! `EventKind` is deliberately not `Copy` (it owns strings), but no event
//! type borrows, so moving one across a call boundary costs exactly one
//! move plus the strings it owns.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

/// Convenience alias with the plugin-facing name of the envelope.
///
/// `PluginEvent` is what the host "dispatches into a plugin"; it is the
/// same type as [`EventPayload`], kept as an alias so both spellings in
/// the documentation refer to one shape.
pub type PluginEvent = EventPayload;

/// A reference to a player, small enough to hand across the ABI by value.
///
/// Deliberately a snapshot, not a live handle: plugins get the identity
/// at the time the event fired and must ask the host for anything newer.
/// Event kinds carry `uuid`/`name` directly since schema 0.1; this type
/// remains for host code that needs a lightweight player reference without
/// a full event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerRef {
    /// The player's UUID, hyphenated canonical form.
    pub uuid: String,
    /// The player's current game profile name.
    pub name: String,
}

/// The kind of event the host is dispatching.
///
/// The payload travels inside the variant: matching on `EventKind` gives
/// plugin code both the discriminant and the data. Serializes as one flat
/// JSON object whose `kind` key is the stable snake_case wire name; the
/// remaining keys are the variant's fields. The wire format is pinned by
/// manual `Serialize`/`Deserialize` impls so the tag is a single key and
/// unknown variants degrade into [`EventKind::Unknown`] - see the module
/// docs for why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// The proxy accepted the connection and handed it to the login
    /// pipeline.
    ConnectionAllowed {
        /// The connecting account's UUID, hyphenated canonical form.
        uuid: String,
        /// The account's game profile name.
        name: String,
        /// The protocol version the client announced in the handshake.
        protocol: u32,
        /// The client address as seen by the proxy (`ip:port`), after
        /// any forwarding resolution the proxy performs.
        address: String,
    },
    /// The connection was rejected or kicked before or during login.
    ConnectionKicked {
        /// The connecting account's UUID, hyphenated canonical form.
        uuid: String,
        /// The account's game profile name.
        name: String,
        /// A human-readable, already-localized reason. Never contains
        /// proxy internals.
        reason: String,
    },
    /// A player completed login and entered the proxy.
    Login {
        /// The player's UUID, hyphenated canonical form.
        uuid: String,
        /// The player's game profile name.
        name: String,
        /// The protocol version negotiated with the client.
        protocol_version: u32,
    },
    /// A player was moved from one backend server to another.
    SwitchServer {
        /// The player's UUID, hyphenated canonical form.
        uuid: String,
        /// The player's game profile name.
        name: String,
        /// The backend the player is leaving.
        from_server: String,
        /// The backend the player is joining.
        to_server: String,
    },
    /// A player disconnected (kick, quit or transport failure).
    ///
    /// Note: `reason` is a plain carrier-free string, not a chat
    /// component; see the module docs for why.
    Disconnect {
        /// The player's UUID, hyphenated canonical form.
        uuid: String,
        /// The player's game profile name.
        name: String,
        /// The backend the player was on when they disconnected.
        server: String,
        /// A human-readable, already-localized reason. Never contains
        /// proxy internals.
        reason: String,
    },
    /// Any kind this ABI version does not know. Preserves forward compat.
    Unknown(String),
}

impl EventKind {
    /// The stable wire name of the event kind, e.g. `"switch_server"`.
    ///
    /// For `Unknown` this is the unrecognized tag, verbatim.
    pub fn as_str(&self) -> &str {
        match self {
            Self::ConnectionAllowed { .. } => "connection_allowed",
            Self::ConnectionKicked { .. } => "connection_kicked",
            Self::Login { .. } => "login",
            Self::SwitchServer { .. } => "switch_server",
            Self::Disconnect { .. } => "disconnect",
            Self::Unknown(name) => name,
        }
    }

    /// The UUID of the player this event is about, when one is involved.
    ///
    /// Every structured variant carries a UUID; `Unknown` does not.
    pub fn uuid(&self) -> Option<&str> {
        match self {
            Self::ConnectionAllowed { uuid, .. }
            | Self::ConnectionKicked { uuid, .. }
            | Self::Login { uuid, .. }
            | Self::SwitchServer { uuid, .. }
            | Self::Disconnect { uuid, .. } => Some(uuid),
            Self::Unknown(_) => None,
        }
    }
}

// The wire tag is a single `kind` key next to the variant's fields,
// e.g. {"kind":"login","uuid":"...","name":"...","protocol_version":769}.
// Unknown tags round-trip losslessly as `Unknown(tag)`; unknown *fields*
// are ignored on deserialize and never re-serialized.
impl Serialize for EventKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::ConnectionAllowed {
                uuid,
                name,
                protocol,
                address,
            } => {
                let mut map = serializer.serialize_map(Some(5))?;
                map.serialize_entry("kind", "connection_allowed")?;
                map.serialize_entry("uuid", uuid)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("protocol", protocol)?;
                map.serialize_entry("address", address)?;
                map.end()
            }
            Self::ConnectionKicked { uuid, name, reason } => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("kind", "connection_kicked")?;
                map.serialize_entry("uuid", uuid)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("reason", reason)?;
                map.end()
            }
            Self::Login {
                uuid,
                name,
                protocol_version,
            } => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("kind", "login")?;
                map.serialize_entry("uuid", uuid)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("protocol_version", protocol_version)?;
                map.end()
            }
            Self::SwitchServer {
                uuid,
                name,
                from_server,
                to_server,
            } => {
                let mut map = serializer.serialize_map(Some(5))?;
                map.serialize_entry("kind", "switch_server")?;
                map.serialize_entry("uuid", uuid)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("from_server", from_server)?;
                map.serialize_entry("to_server", to_server)?;
                map.end()
            }
            Self::Disconnect {
                uuid,
                name,
                server,
                reason,
            } => {
                let mut map = serializer.serialize_map(Some(5))?;
                map.serialize_entry("kind", "disconnect")?;
                map.serialize_entry("uuid", uuid)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("server", server)?;
                map.serialize_entry("reason", reason)?;
                map.end()
            }
            Self::Unknown(name) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("kind", name)?;
                map.end()
            }
        }
    }
}

// one JSON value, kept exactly as it arrived; we only pick the
// fields this schema knows and drop everything else
enum WireValue {
    Text(String),
    Unsigned(u64),
    Signed(i64),
}

impl<'de> Deserialize<'de> for WireValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct WireValueVisitor;

        impl serde::de::Visitor<'_> for WireValueVisitor {
            type Value = WireValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string or an unsigned integer")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(WireValue::Text(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(WireValue::Text(value))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(WireValue::Unsigned(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(WireValue::Signed(value))
            }
        }

        deserializer.deserialize_any(WireValueVisitor)
    }
}

#[derive(Default)]
struct LooseFields {
    uuid: Option<WireValue>,
    name: Option<WireValue>,
    reason: Option<WireValue>,
    server: Option<WireValue>,
    from_server: Option<WireValue>,
    to_server: Option<WireValue>,
    address: Option<WireValue>,
    protocol: Option<WireValue>,
    protocol_version: Option<WireValue>,
}

impl LooseFields {
    fn take_text(slot: &mut Option<WireValue>, field: &str) -> Result<String, String> {
        match slot.take() {
            Some(WireValue::Text(s)) => Ok(s),
            Some(_) => Err(format!("field `{field}` must be a string")),
            None => Err(format!("missing field `{field}`")),
        }
    }

    fn take_u32(slot: &mut Option<WireValue>, field: &str) -> Result<u32, String> {
        let raw = match slot.take() {
            Some(WireValue::Unsigned(n)) => n,
            Some(WireValue::Signed(n)) => u64::try_from(n)
                .map_err(|_| format!("field `{field}` must be an unsigned integer"))?,
            Some(WireValue::Text(_)) => {
                return Err(format!("field `{field}` must be an unsigned integer"));
            }
            None => return Err(format!("missing field `{field}`")),
        };
        u32::try_from(raw).map_err(|_| format!("field `{field}` is out of range"))
    }
}

struct KindVisitor;

impl<'de> serde::de::Visitor<'de> for KindVisitor {
    type Value = EventKind;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON object with a `kind` tag")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut tag: Option<String> = None;
        let mut fields = LooseFields::default();

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "kind" => tag = Some(map.next_value::<String>()?),
                "uuid" => fields.uuid = Some(map.next_value()?),
                "name" => fields.name = Some(map.next_value()?),
                "reason" => fields.reason = Some(map.next_value()?),
                "server" => fields.server = Some(map.next_value()?),
                "from_server" => fields.from_server = Some(map.next_value()?),
                "to_server" => fields.to_server = Some(map.next_value()?),
                "address" => fields.address = Some(map.next_value()?),
                "protocol" => fields.protocol = Some(map.next_value()?),
                "protocol_version" => fields.protocol_version = Some(map.next_value()?),
                // unknown fields are tolerated and dropped, whatever shape they take
                _ => {
                    let _: serde::de::IgnoredAny = map.next_value()?;
                }
            }
        }

        let tag = tag.ok_or_else(|| serde::de::Error::missing_field("kind"))?;
        let err = serde::de::Error::custom;

        match tag.as_str() {
            "connection_allowed" => Ok(EventKind::ConnectionAllowed {
                uuid: LooseFields::take_text(&mut fields.uuid, "uuid").map_err(err)?,
                name: LooseFields::take_text(&mut fields.name, "name").map_err(err)?,
                protocol: LooseFields::take_u32(&mut fields.protocol, "protocol").map_err(err)?,
                address: LooseFields::take_text(&mut fields.address, "address").map_err(err)?,
            }),
            "connection_kicked" => Ok(EventKind::ConnectionKicked {
                uuid: LooseFields::take_text(&mut fields.uuid, "uuid").map_err(err)?,
                name: LooseFields::take_text(&mut fields.name, "name").map_err(err)?,
                reason: LooseFields::take_text(&mut fields.reason, "reason").map_err(err)?,
            }),
            "login" => Ok(EventKind::Login {
                uuid: LooseFields::take_text(&mut fields.uuid, "uuid").map_err(err)?,
                name: LooseFields::take_text(&mut fields.name, "name").map_err(err)?,
                protocol_version: LooseFields::take_u32(
                    &mut fields.protocol_version,
                    "protocol_version",
                )
                .map_err(err)?,
            }),
            "switch_server" => Ok(EventKind::SwitchServer {
                uuid: LooseFields::take_text(&mut fields.uuid, "uuid").map_err(err)?,
                name: LooseFields::take_text(&mut fields.name, "name").map_err(err)?,
                from_server: LooseFields::take_text(&mut fields.from_server, "from_server")
                    .map_err(err)?,
                to_server: LooseFields::take_text(&mut fields.to_server, "to_server")
                    .map_err(err)?,
            }),
            "disconnect" => Ok(EventKind::Disconnect {
                uuid: LooseFields::take_text(&mut fields.uuid, "uuid").map_err(err)?,
                name: LooseFields::take_text(&mut fields.name, "name").map_err(err)?,
                server: LooseFields::take_text(&mut fields.server, "server").map_err(err)?,
                reason: LooseFields::take_text(&mut fields.reason, "reason").map_err(err)?,
            }),
            _ => Ok(EventKind::Unknown(tag)),
        }
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(KindVisitor)
    }
}

/// The flat wire container the host dispatches into plugins.
///
/// Kept for ABI name stability; identical on the wire to [`EventKind`].
/// For dispatches that need registry bookkeeping (monotonic ids,
/// timestamps), the host wraps the kind in an [`EventEnvelope`] - and
/// [`ProxyEvent`] when it wants a host-side accumulator for the outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPayload {
    /// What happened; the payload travels inside the variant.
    #[serde(flatten)]
    pub kind: EventKind,
}

impl EventPayload {
    /// Creates a payload of the given kind.
    pub fn new(kind: EventKind) -> Self {
        Self { kind }
    }
}

/// Allocates monotonic event ids for the host side of the ABI.
///
/// Ids start at 1 and increase by exactly 1 per [`EventEnvelope`] the
/// registry mints; ordering of ids reflects the order events were
/// dispatched. Plugins never mint event ids - they only see the ids the
/// host hands out. This is a thin atomic counter, not a queue: it keeps
/// dispatch code lock-free.
#[derive(Debug)]
pub struct EventIdRegistry {
    next: AtomicU64,
}

impl EventIdRegistry {
    /// Creates an empty registry; the first id it issues is 1.
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    /// Returns the next monotonic id.
    pub fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for EventIdRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The envelope the host dispatches into plugins.
///
/// `id` comes from an [`EventIdRegistry`] and is monotonic;
/// `occurred_at_unix_ms` is the wall-clock time the host observed the
/// event, milliseconds since the Unix epoch. Both are stamped by the
/// host - plugins treat them as opaque facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Monotonic id from the host-side registry, starting at 1.
    pub id: u64,
    /// What happened; the payload travels inside the variant.
    #[serde(flatten)]
    pub kind: EventKind,
    /// Milliseconds since the Unix epoch when the host observed the event.
    pub occurred_at_unix_ms: u64,
}

impl EventEnvelope {
    /// Creates an envelope. The host is responsible for minting `id`
    /// (e.g. via [`EventIdRegistry::next_id`]) and stamping the clock.
    pub fn new(id: u64, kind: EventKind, occurred_at_unix_ms: u64) -> Self {
        Self {
            id,
            kind,
            occurred_at_unix_ms,
        }
    }
}

/// A compact cancellation signal a plugin sends back to the host.
///
/// `uuid` names the player the cancellation applies to (all structured
/// event kinds carry one), `event_id` ties it to the [`EventEnvelope`]
/// the host dispatched. `cancelled` is the signal itself; the wire key
/// is exactly `"cancelled"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCancellation {
    /// The player the cancellation applies to.
    pub uuid: String,
    /// The id of the [`EventEnvelope`] that was dispatched.
    pub event_id: u64,
    /// Whether the plugin cancels the event.
    pub cancelled: bool,
}

/// The full outcome of a dispatch, as read back by the host.
///
/// `cancel_reason` is optional and meant for humans (server logs, kick
/// screens); it is never required for the cancellation itself to take
/// effect. The wire form elides `cancel_reason` entirely when unset, so
/// `{"cancelled":true}` and `{"cancelled":true,"cancel_reason":"..."}`
/// are both valid plugin answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeReport {
    /// Whether the plugin cancels the event.
    pub cancelled: bool,
    /// Optional human-readable reason for the cancellation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
}

impl OutcomeReport {
    /// An outcome that lets the event proceed.
    pub fn allowed() -> Self {
        Self {
            cancelled: false,
            cancel_reason: None,
        }
    }

    /// An outcome that cancels the event, with an optional reason.
    pub fn cancelled(cancel_reason: Option<String>) -> Self {
        Self {
            cancelled: true,
            cancel_reason,
        }
    }
}

/// The host-side accumulator for one dispatch.
///
/// Wraps the [`EventEnvelope`] that went out and remembers how the
/// plugin answered: [`ProxyEvent::cancel`] flips the event into the
/// cancelled state, and [`ProxyEvent::outcome`] / [`ProxyEvent::cancellation`]
/// produce the two wire shapes the host can hand back to its own logic.
///
/// Serializes flat like the envelope, with `cancelled` / `cancel_reason`
/// (the latter elided when unset) appended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyEvent {
    #[serde(flatten)]
    envelope: EventEnvelope,
    cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cancel_reason: Option<String>,
}

impl ProxyEvent {
    /// Wraps a dispatched envelope in an un-cancelled accumulator.
    pub fn new(envelope: EventEnvelope) -> Self {
        Self {
            envelope,
            cancelled: false,
            cancel_reason: None,
        }
    }

    /// The envelope this event was built from.
    pub fn envelope(&self) -> &EventEnvelope {
        &self.envelope
    }

    /// Consumes the accumulator and returns the envelope.
    pub fn into_envelope(self) -> EventEnvelope {
        self.envelope
    }

    /// The kind that was dispatched.
    pub fn kind(&self) -> &EventKind {
        &self.envelope.kind
    }

    /// The UUID of the player this event is about, when one is involved.
    pub fn uuid(&self) -> Option<&str> {
        self.envelope.kind.uuid()
    }

    /// The monotonic id of the dispatched envelope.
    pub fn event_id(&self) -> u64 {
        self.envelope.id
    }

    /// Milliseconds since the Unix epoch when the host stamped the event.
    pub fn occurred_at_unix_ms(&self) -> u64 {
        self.envelope.occurred_at_unix_ms
    }

    /// Whether a plugin cancelled this event.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// The reason given for the cancellation, if any.
    pub fn cancel_reason(&self) -> Option<&str> {
        self.cancel_reason.as_deref()
    }

    /// Marks the event cancelled, remembering the optional reason.
    pub fn cancel(&mut self, reason: Option<String>) {
        self.cancelled = true;
        self.cancel_reason = reason;
    }

    /// The [`OutcomeReport`] form of this event's state.
    pub fn outcome(&self) -> OutcomeReport {
        OutcomeReport {
            cancelled: self.cancelled,
            cancel_reason: self.cancel_reason.clone(),
        }
    }

    /// The compact [`EventCancellation`] form of this event's state.
    ///
    /// Returns `None` when the event's kind carries no player UUID
    /// (i.e. [`EventKind::Unknown`]).
    pub fn cancellation(&self) -> Option<EventCancellation> {
        let uuid = self.uuid()?.to_owned();
        Some(EventCancellation {
            uuid,
            event_id: self.event_id(),
            cancelled: self.cancelled,
        })
    }
}

/// An event that can be dispatched across the plugin ABI.
///
/// Pure marker trait with associated types for the two directions of a
/// dispatch: `Input` is what the host passes into the plugin, `Output`
/// is what the proxy reads back. The trait exists so generic host code
/// (dispatch loops, metrics) can talk about "an event" without naming
/// the concrete enum; everything behind it stays the flat
/// enum/struct types from this module, which is what keeps the WASM
/// boundary cheap.
pub trait MinecraftEvent {
    /// The payload the host hands to the plugin.
    type Input: Serialize + DeserializeOwned;
    /// The report the proxy reads back from the plugin.
    type Output: Serialize + DeserializeOwned;
}

impl MinecraftEvent for ProxyEvent {
    type Input = EventEnvelope;
    type Output = OutcomeReport;
}

/// The envelope the host fires when a player completed login.
///
/// The dispatched kind is [`EventKind::Login`]. This is a type alias,
/// not a new type - plugin handlers name the alias for readability while
/// the ABI carries one common shape.
pub type LoginEvent = ProxyEvent;

/// The envelope the host fires when a connection is kicked or rejected.
///
/// The dispatched kind is [`EventKind::ConnectionKicked`].
pub type KickEvent = ProxyEvent;

/// The envelope the host fires when a player disconnected.
///
/// The dispatched kind is [`EventKind::Disconnect`]; its `reason` is a
/// carrier-free string, not a chat component.
pub type DisconnectEvent = ProxyEvent;

/// The envelope the host fires when a connection was accepted.
///
/// The dispatched kind is [`EventKind::ConnectionAllowed`].
pub type ConnectAllowedEvent = ProxyEvent;

/// The envelope the host fires when a player moves backends.
///
/// The dispatched kind is [`EventKind::SwitchServer`].
pub type SwitchServerEvent = ProxyEvent;
