//! Events the host dispatches into plugins.
//!
//! The flow is strictly one-directional: something happens in the proxy
//! (a connection was accepted, a player joined, ...), the host builds an
//! [`EventPayload`] and hands it to subscribed plugins. Plugins react
//! through the host functions the runtime provides; there is no plugin to
//! plugin dispatch here.
//!
//! ## Wire format
//!
//! The plugin ABI transports payloads as bytes, and the convention for
//! structured payloads is JSON (via extism-convert on the plugin side).
//! Every type in this module therefore round-trips through a compact,
//! self-describing JSON document:
//!
//! ```json
//! {
//!   "kind": "switch_server",
//!   "player": { "uuid": "069a79f4-44e9-4726-a5be-fca90e38aaf5", "name": "SomePlayer" },
//!   "server": "lobby"
//! }
//! ```
//!
//! [`EventKind`] serializes as a plain snake_case string; unknown kinds
//! introduced by newer hosts survive a round-trip through
//! [`EventKind::Unknown`] instead of failing deserialization, so plugins
//! built against an older ABI keep working — they simply cannot match on
//! events they do not know.

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerRef {
    /// The player's UUID, hyphenated canonical form.
    pub uuid: String,
    /// The player's current game profile name.
    pub name: String,
}

/// The kind of event the host is dispatching.
///
/// Serializes as a snake_case string. `Unknown` catches kinds this ABI
/// version does not know about; see the module docs for why. The wire
/// format is pinned by manual `Serialize`/`Deserialize` impls because
/// serde's untagged unit variants would go out as `null`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// The proxy decided the connection is allowed and handed it to the
    /// login pipeline.
    ConnectionAllowed,
    /// The connection was rejected or kicked before/while logging in.
    ConnectionKicked,
    /// A player completed login and entered the proxy.
    Login,
    /// A player was moved from one backend server to another.
    SwitchServer,
    /// A player disconnected (kick, quit or transport failure).
    Disconnect,
    /// Any kind this ABI version does not know. Preserves forward compat.
    Unknown(String),
}

impl EventKind {
    /// The stable wire name of the event kind, e.g. `"switch_server"`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::ConnectionAllowed => "connection_allowed",
            Self::ConnectionKicked => "connection_kicked",
            Self::Login => "login",
            Self::SwitchServer => "switch_server",
            Self::Disconnect => "disconnect",
            Self::Unknown(name) => name,
        }
    }
}

impl Serialize for EventKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = String::deserialize(deserializer)?;
        Ok(match wire.as_str() {
            "connection_allowed" => Self::ConnectionAllowed,
            "connection_kicked" => Self::ConnectionKicked,
            "login" => Self::Login,
            "switch_server" => Self::SwitchServer,
            "disconnect" => Self::Disconnect,
            _ => Self::Unknown(wire),
        })
    }
}

/// The envelope the host dispatches into plugins.
///
/// Only the fields that make sense for the event kind are populated;
/// the rest are `None` and omitted from the wire form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPayload {
    /// What happened.
    pub kind: EventKind,
    /// The player the event is about, when one is involved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player: Option<PlayerRef>,
    /// The backend server the event is about, when one is involved
    /// (e.g. the target of a `switch_server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// A human-readable, already-localized reason string, when applicable
    /// (kick reason, disconnect reason). Never contains internals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl EventPayload {
    /// Creates an empty envelope of the given kind; fields are filled in
    /// by the host as it understands the context.
    pub fn new(kind: EventKind) -> Self {
        Self {
            kind,
            player: None,
            server: None,
            reason: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_wire_names_match_as_str() {
        let cases = [
            (EventKind::ConnectionAllowed, "connection_allowed"),
            (EventKind::ConnectionKicked, "connection_kicked"),
            (EventKind::Login, "login"),
            (EventKind::SwitchServer, "switch_server"),
            (EventKind::Disconnect, "disconnect"),
        ];
        for (kind, wire) in cases {
            assert_eq!(kind.as_str(), wire);
            let json = serde_json::to_string(&kind).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""));
            let back: EventKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn unknown_kind_roundtrips() {
        let json = r#""server_ping""#;
        let kind: EventKind = serde_json::from_str(json).expect("deserialize");
        assert_eq!(kind, EventKind::Unknown("server_ping".to_owned()));
        assert_eq!(kind.as_str(), "server_ping");
        let back = serde_json::to_string(&kind).expect("serialize");
        assert_eq!(back, json);
    }
}
