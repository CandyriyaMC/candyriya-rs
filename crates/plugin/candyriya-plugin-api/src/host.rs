//! The external mirror of the host-function surface (`candyriya:*`).
//!
//! A candyriya plugin runs as an Extism WASM module; the host exposes a
//! fixed set of host functions under the `candyriya` namespace
//! (`candyriya:helloWorld`, ...). Every function takes and returns one
//! `Ptr` argument crossing the ABI as Extism handles, and the payload
//! convention is plain bytes — JSON for everything structured, raw UTF-8
//! for the two stringly functions.
//!
//! This module is the *published contract* for that surface: the typed
//! request/response shapes, the exact wire names, and a typed trait
//! capturing the same surface for hosts and SDKs. Plugin authors depend
//! on this crate; the proxy implements the functions on top of the same
//! types, so the two sides cannot silently drift.
//!
//! # Wire format notes
//!
//! - All inputs are validated host-side. A missing or malformed input is
//!   answered with an error JSON document (`{"ok": false, "error": "..."}`)
//!   rather than a trap — a plugin calling with bad bytes can continue.
//! - Every JSON-object input accepts an optional `plugin_id` field. It
//!   is not sent by honest callers; when present it must match the
//!   *registered* plugin's id or the call is rejected (a plugin cannot
//!   claim to be another plugin). The array-shaped inputs carry no
//!   object to smuggle the field in.
//! - JSON key order is not significant on the wire.
//!
//! Each function below documents the exact input and output bytes with an
//! example, mirrored by the trait's documentation.

use serde::{Deserialize, Serialize};

use crate::events::PlayerRef;

/// The namespace every candyriya host function is registered under.
pub const HOST_NAMESPACE: &str = "candyriya";
/// `candyriya:helloWorld` — input is a raw UTF-8 name, not JSON.
pub const HELLO_WORLD_FN: &str = "candyriya:helloWorld";
/// `candyriya:getLocale` — input is ignored, output is the active locale id.
pub const GET_LOCALE_FN: &str = "candyriya:getLocale";
/// `candyriya:callApi` — input is a [`CallApiRequest`], output a [`CallApiResponse`].
pub const CALL_API_FN: &str = "candyriya:callApi";
/// `candyriya:logMessage` — input is a [`LogMessage`], output a [`HostOk`].
pub const LOG_MESSAGE_FN: &str = "candyriya:logMessage";
/// `candyriya:listServers` — input is ignored, output is a JSON array of server names.
pub const LIST_SERVERS_FN: &str = "candyriya:listServers";
/// `candyriya:getPlayerRoster` — input is ignored, output is a JSON array of [`PlayerRef`]s.
pub const GET_PLAYER_ROSTER_FN: &str = "candyriya:getPlayerRoster";
/// `candyriya:pingPlayer` — input is a [`PlayerNameRequest`], output a [`HostOk`].
pub const PING_PLAYER_FN: &str = "candyriya:pingPlayer";
/// `candyriya:switchPlayer` — input is a [`SwitchPlayerRequest`], output a [`HostOk`].
pub const SWITCH_PLAYER_FN: &str = "candyriya:switchPlayer";
/// `candyriya:cancelEvent` — input is a [`CancelEventRequest`], output a [`HostOk`].
pub const CANCEL_EVENT_FN: &str = "candyriya:cancelEvent";
/// `candyriya:subscribeToEvents` — input is a JSON array of event kind strings, output a [`HostOk`].
pub const SUBSCRIBE_TO_EVENTS_FN: &str = "candyriya:subscribeToEvents";

/// The severity of a [`LogMessage`] a plugin wants the host to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// The plugin reports an error; the host logs at `error` level.
    Error,
    /// The plugin reports a warning; the host logs at `warn` level.
    Warn,
    /// The plugin reports general information; the host logs at `info` level.
    Info,
    /// The plugin reports diagnostics; the host logs at `debug` level.
    Debug,
}

/// Input to `candyriya:logMessage` (`{"level": "error|warn|info|debug", "msg": "..."}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogMessage {
    /// How severe the line is.
    pub level: LogLevel,
    /// The message body. Never interpreted by the host.
    pub msg: String,
}

/// The shared `{"ok": true|false, "error"?: "..."}` answer.
///
/// This is the shape used by the mutating/signalling functions whose only
/// result is success or a human-readable failure reason. `error` is set
/// exactly when `ok` is `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostOk {
    /// Whether the host performed what was asked.
    pub ok: bool,
    /// Why it did not, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl HostOk {
    /// A successful answer without a reason.
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    /// A failed answer with a human-readable reason.
    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(reason.into()),
        }
    }
}

/// Input to `candyriya:callApi` (`{"target_plugin": "...", "fn": "...", "args": [...]}`).
///
/// Routes a call into another plugin's `[api].exports`. The target must be
/// loaded and must list `function` in its manifest; `args` is the raw JSON
/// array passed to the target's export as bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallApiRequest {
    /// The id of the plugin to call.
    pub target_plugin: String,
    /// The export to invoke on the target. Wire key is `fn`.
    #[serde(rename = "fn")]
    pub function: String,
    /// Arguments for the target, carried as JSON values and re-serialized
    /// to bytes for the call.
    #[serde(default)]
    pub args: Vec<serde_json::Value>,
}

/// Output of `candyriya:callApi` (`{"ok": true|false, "error"?: "...", "data"?: "..."}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallApiResponse {
    /// Whether the target answered without a host-side failure.
    pub ok: bool,
    /// Why it did not, when it did not (`plugin not loaded`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The target's output, as UTF-8 (lossy) text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

impl CallApiResponse {
    /// A successful response carrying the target's output.
    pub fn ok(data: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            data: Some(data.into()),
        }
    }

    /// A failed response with a human-readable reason.
    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(reason.into()),
            data: None,
        }
    }
}

/// Input to `candyriya:pingPlayer` and a couple of player-targeted functions
/// (`{"name": "Steve"}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerNameRequest {
    /// The player's current game profile name.
    pub name: String,
}

/// Input to `candyriya:switchPlayer` (`{"name": "Steve", "server": "local"}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchPlayerRequest {
    /// The player to move.
    pub name: String,
    /// The backend to move the player to.
    pub server: String,
}

/// Input to `candyriya:cancelEvent` (`{"cancel": true|false, "reason"?: "..."}`).
///
/// The cancellation applies to the event currently being dispatched to the
/// plugin; the host ORs it into the final [`crate::OutcomeReport`], so an
/// explicit `cancel: true` wins even if the export itself answers "allowed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelEventRequest {
    /// Whether the event is cancelled.
    pub cancel: bool,
    /// Optional human-readable cancellation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The typed view of the host-function surface.
///
/// Implemented by the host runtime (the proxy's plugin engine); plugin
/// authors do not implement this trait — they call the host functions
/// from WASM. It exists so hosts and SDKs share one typed contract and
/// so the wire format is checked against a single source of truth.
///
/// Every method documents the exact host function it mirrors, its input
/// bytes and its output bytes, with an example.
pub trait CandyriyaHostApi: Send + Sync {
    /// `candyriya:helloWorld` — test artifact for SDK authors.
    ///
    /// - Input: a raw UTF-8 string (not JSON), e.g. `Steve`.
    /// - Output: a raw UTF-8 string (not JSON), e.g. `hello, Steve`.
    fn hello_world(&self, name: &str) -> String;

    /// `candyriya:getLocale` — the active locale for this plugin.
    ///
    /// The locale is the host's per-plugin locale (falling back to the
    /// proxy's configured default), e.g. `en` or `ex_girl`. Hosts route
    /// plugin-side i18n fallback chains by plugin id; plugins see only
    /// the resolved id.
    ///
    /// - Input: ignored.
    /// - Output: a raw UTF-8 string, e.g. `en`.
    fn get_locale(&self) -> String;

    /// `candyriya:callApi` — call another plugin through the host.
    ///
    /// The target must be loaded and must list `function` in its
    /// `[api].exports`; the host sets the target's caller identity for the
    /// duration of the call. `error` is `"plugin not loaded"` when the
    /// target is missing or not in `Loaded` state.
    ///
    /// - Input: [`CallApiRequest`], e.g.
    ///   `{"target_plugin": "economy", "fn": "hello_world", "args": ["Steve"]}`.
    /// - Output: [`CallApiResponse`], e.g.
    ///   `{"ok": true, "data": "hello, Steve"}` or
    ///   `{"ok": false, "error": "plugin not loaded"}`.
    fn call_api(&self, request: &CallApiRequest) -> CallApiResponse;

    /// `candyriya:logMessage` — ask the host to log on the plugin's behalf.
    ///
    /// - Input: [`LogMessage`], e.g. `{"level": "info", "msg": "loaded"}`.
    /// - Output: [`HostOk`], e.g. `{"ok": true}`.
    fn log_message(&self, message: &LogMessage) -> HostOk;

    /// `candyriya:listServers` — the backends the proxy knows about.
    ///
    /// - Input: ignored.
    /// - Output: a JSON array of server names, e.g. `["local"]`.
    fn list_servers(&self) -> Vec<String>;

    /// `candyriya:getPlayerRoster` — the online players, by profile name.
    ///
    /// - Input: ignored.
    /// - Output: a JSON array of [`PlayerRef`] objects, e.g.
    ///   `[{"name": "Steve", "uuid": "5627dd98-e6be-3c21-b8a8-e92344183641"}]`.
    fn get_player_roster(&self) -> Vec<PlayerRef>;

    /// `candyriya:pingPlayer` — send a chat ping to an online player.
    ///
    /// Reports `{"ok": false, "error": "..."}` when the player is not
    /// known to be online or the host has no delivery channel for pings
    /// registered yet.
    ///
    /// - Input: [`PlayerNameRequest`], e.g. `{"name": "Steve"}`.
    /// - Output: [`HostOk`], e.g. `{"ok": true}`.
    fn ping_player(&self, request: &PlayerNameRequest) -> HostOk;

    /// `candyriya:switchPlayer` — move a player to another backend.
    ///
    /// The proxy has no switch backend registered yet; hosts currently
    /// answer with `{"ok": false, "error": "not implemented yet"}`.
    ///
    /// - Input: [`SwitchPlayerRequest`], e.g.
    ///   `{"name": "Steve", "server": "local"}`.
    /// - Output: [`HostOk`], e.g.
    ///   `{"ok": false, "error": "not implemented yet"}`.
    fn switch_player(&self, request: &SwitchPlayerRequest) -> HostOk;

    /// `candyriya:cancelEvent` — cancel the event being dispatched to this plugin.
    ///
    /// The host ORs the recorded cancellation into the final
    /// [`crate::OutcomeReport`] for the current dispatch.
    ///
    /// - Input: [`CancelEventRequest`], e.g. `{"cancel": true, "reason": "lobby is full"}`.
    /// - Output: [`HostOk`], e.g. `{"ok": true}`.
    fn cancel_event(&self, request: &CancelEventRequest) -> HostOk;

    /// `candyriya:subscribeToEvents` — opt this plugin into specific event kinds.
    ///
    /// The kinds are [`crate::EventKind::as_str`] wire names (`login`,
    /// `connection_kicked`, `connection_allowed`, `switch_server`,
    /// `disconnect`). Until the plugin subscribes, it receives every kind
    /// (the default is "all"). Passing an empty array also means "all" —
    /// there is no way to subscribe to nothing. A non-empty list is an
    /// exact allowlist: events outside it are not dispatched to the
    /// plugin at all, and the plugin is not listed in the per-plugin
    /// outcomes for them. Calling this replaces the previous allowlist.
    ///
    /// - Input: a JSON array of kind strings, e.g. `["login", "connection_kicked"]`.
    /// - Output: [`HostOk`], e.g. `{"ok": true}`.
    fn subscribe_to_events(&self, kinds: &[String]) -> HostOk;
}
