//! Host functions registered on every plugin's extism builder.
//!
//! These are the `candyriya:*` functions a plugin can call from WASM. The
//! surface is deliberately small and read/signal-only: no host function
//! mutates proxy or server state directly. The cross-subsystem ones
//! (`callApi`, `listServers`, `getPlayerRoster`, and the roster check
//! inside `pingPlayer`) reach the supervised `host` unit over the
//! capability IPC plane, through the shared [`HostIpc`] link every
//! plugin instance carries; when the link is missing or the call fails,
//! each answers with its documented honest error instead of a fallback
//! it cannot back. Plugin-owned services keep flowing through the
//! [`ServiceRegistry`], unchanged.
//!
//! # Design rules
//!
//! - The plugin's identity comes from the [`HostState`] the instance was
//!   built with — never from anything in the input bytes. An optional
//!   `plugin_id` field is honoured *only* as a self-check: when present
//!   it must equal the registered id, otherwise the call is rejected
//!   (a plugin cannot claim to be another plugin).
//! - Wrong or missing input is answered with an error JSON document
//!   (`{"ok": false, "error": "..."}`), never a trap: a buggy plugin
//!   gets a parseable answer instead of a blown-up call.
//! - Host functions hold no event-dispatch state of their own; they
//!   delegate to the `host` unit or to the pending-outcome slot the
//!   dispatcher reads afterwards.
//! - `switchPlayer` has no backend behind it yet, so it answers with
//!   `{"ok": false, "error": "not implemented yet"}` — an honest stub
//!   error handed downstream to the plugin author, not a fake success.
//! - `i18nT` resolves keys through a plugin-scoped chain — the plugin's
//!   own locale file (active, then `en`), then the shared `common`
//!   tables (active, then `en`), then the key itself. Only the plugin's
//!   `plugin.<id>.*` keys plus `common.*` keys resolve; out-of-scope
//!   keys answer with the key itself, same as a missing key.
//!
//! # The host link and its failure vocabulary
//!
//! [`PLAYERS_PING_SINK_KEY`] — a [`PlayerPingSink`] delivering the ping
//! (optional; without it `pingPlayer` reports failure instead of
//! pretending to have delivered). The sink stays in the
//! [`ServiceRegistry`] on purpose: the `host` unit deliberately serves
//! no ping message because the play state has no client-bound
//! packet-injection path yet, so there is nothing honest to route it to.
//!
//! When the [`HostIpc`] link cannot answer — never installed, unit down,
//! full mailbox, timeout — the answers are:
//!
//! - `callApi` — `CallApiResponse::err` with the reason prefixed
//!   `callApi: host dispatcher unavailable (...)`, the same shape a
//!   missing dispatcher used to answer with.
//! - `listServers` — `["local"]`, the documented stand-in for "no known
//!   backends" (a boot with no `[servers]`, a disabled routing reload,
//!   or an unreachable host unit all degrade the same way). A healthy
//!   route unit serving zero backends answers `["local"]` for the same
//!   reason, so the plugin never has to know which case it is.
//! - `getPlayerRoster` — an empty list; the host API carries no error
//!   channel, so silence is impossible and fabrication is ruled out.
//! - `pingPlayer` — `player roster unavailable (...)`; skipping the
//!   online check and delivering anyway would claim a delivery that did
//!   not happen.

use std::sync::Arc;

use extism::{CurrentPlugin, UserData, Val};
use serde::de::DeserializeOwned;
use serde_json::Value;

use candyriya_plugin_api::{
    CallApiRequest, CallApiResponse, CancelEventRequest, CandyriyaHostApi, HostOk, I18nTRequest,
    LogLevel, LogMessage, OutcomeReport, PlayerNameRequest, PlayerRef, SwitchPlayerRequest,
};

use crate::host_state::HostState;
use crate::host_wire::{HostMsg, HostResp};

/// The namespace every candyriya host function is registered under.
pub(crate) const HOST_NAMESPACE: &str = "candyriya";

/// Service key for the player-ping delivery channel ([`PlayerPingSink`]).
pub const PLAYERS_PING_SINK_KEY: &str = "players.ping_sink";

/// Delivery channel for `candyriya:pingPlayer`.
///
/// The composition root registers it under [`PLAYERS_PING_SINK_KEY`] when a
/// chat-ping path to players exists; until then `candyriya:pingPlayer`
/// answers with an error rather than silently dropping the ping.
#[derive(Clone)]
pub struct PlayerPingSink(pub Arc<dyn Fn(&str) + Send + Sync>);

impl PlayerPingSink {
    /// Wrap a `Send + Sync` closure that receives the target player's name.
    pub fn new(f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
}

impl std::fmt::Debug for PlayerPingSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayerPingSink").finish_non_exhaustive()
    }
}

/// Handle bound to the proxy's data layout: register every host function
/// on `builder`, closing over `host_state`.
pub(crate) fn register_host_functions(
    builder: extism::PluginBuilder<'_>,
    host_state: HostState,
) -> extism::PluginBuilder<'_> {
    let state = || host_state.clone();
    builder
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "helloWorld",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            hello_world,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "getLocale",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            get_locale,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "i18nT",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            i18n_t,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "i18nLocale",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            i18n_locale,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "callApi",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            call_api,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "logMessage",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            log_message,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "listServers",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            list_servers,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "getPlayerRoster",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            get_player_roster,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "pingPlayer",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            ping_player,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "switchPlayer",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            switch_player,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "cancelEvent",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            cancel_event,
        )
        .with_function_in_namespace(
            HOST_NAMESPACE,
            "subscribeToEvents",
            [extism::PTR],
            [extism::PTR],
            UserData::new(state()),
            subscribe_to_events,
        )
}

impl CandyriyaHostApi for HostState {
    fn hello_world(&self, name: &str) -> String {
        format!("hello, {name}")
    }

    fn get_locale(&self) -> String {
        self.i18n_locale.lock().clone()
    }

    fn i18n_t(&self, request: &I18nTRequest) -> String {
        let key = &request.key;
        // scope gate first: a plugin only ever sees its own namespace
        // plus the shared common table, never core or peer plugins
        if !key_in_plugin_scope(key, &self.plugin_id) {
            tracing::warn!(
                plugin = %self.plugin_id,
                key = %key,
                "i18n key out of scope; returning the key itself",
            );
            return key.clone();
        }
        let Some(locales) = &self.locales else {
            tracing::warn!(
                plugin = %self.plugin_id,
                key = %key,
                "no i18n machine available; returning the key itself",
            );
            return key.clone();
        };
        let args: Vec<(String, String)> = request
            .args
            .iter()
            .flat_map(serde_json::Map::iter)
            .map(|(name, value)| (name.clone(), json_value_to_arg_string(value)))
            .collect();
        let refs: Vec<(&str, &str)> = args
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let active = self.i18n_locale.lock().clone();
        locales
            .pick(&active)
            .t_for_plugin(&self.plugin_id, key, &refs)
    }

    fn i18n_locale(&self) -> String {
        self.i18n_locale.lock().clone()
    }

    fn call_api(&self, request: &CallApiRequest) -> CallApiResponse {
        let args = match serde_json::to_vec(&request.args) {
            Ok(args) => args,
            Err(error) => {
                return CallApiResponse::err(format!("args are not serializable: {error}"));
            }
        };
        let message = HostMsg::CallApi {
            target_plugin: request.target_plugin.clone(),
            function: request.function.clone(),
            args,
            caller: Some(self.plugin_id.clone()),
        };
        match self.host_ipc.call(message) {
            Ok(HostResp::CallApi(outcome)) => match outcome {
                Ok(output) => CallApiResponse::ok(String::from_utf8_lossy(&output)),
                // the dispatcher's own diagnosis, verbatim to the plugin
                Err(message) => CallApiResponse::err(message),
            },
            Ok(_) => {
                tracing::error!(
                    plugin = %self.plugin_id,
                    "host unit answered with an unexpected reply for callApi",
                );
                CallApiResponse::err("host answered with an unexpected reply type")
            }
            Err(error) => {
                tracing::error!(
                    plugin = %self.plugin_id,
                    %error,
                    "callApi could not reach the host dispatcher",
                );
                CallApiResponse::err(format!("callApi: host dispatcher unavailable ({error})"))
            }
        }
    }

    fn log_message(&self, message: &LogMessage) -> HostOk {
        // keep one plugin from flooding the proxy log; the full line is
        // truncated rather than dropped so the tail stays visible
        const MAX_MSG_BYTES: usize = 1024;
        let msg = utf8_truncate(&message.msg, MAX_MSG_BYTES);
        match message.level {
            LogLevel::Error => tracing::error!(plugin = %self.plugin_id, "{msg}"),
            LogLevel::Warn => tracing::warn!(plugin = %self.plugin_id, "{msg}"),
            LogLevel::Info => tracing::info!(plugin = %self.plugin_id, "{msg}"),
            LogLevel::Debug => tracing::debug!(plugin = %self.plugin_id, "{msg}"),
        }
        HostOk::ok()
    }

    fn list_servers(&self) -> Vec<String> {
        match self.host_ipc.call(HostMsg::Names) {
            // a healthy route unit serving zero backends is the empty
            // answer for "routing is disabled" — same degrade the
            // documented `["local"]` stand-in covers
            Ok(HostResp::Names(names)) if !names.is_empty() => names,
            Ok(HostResp::Names(_)) => vec!["local".to_owned()],
            Ok(_) => {
                tracing::error!(
                    plugin = %self.plugin_id,
                    "host unit answered with an unexpected reply for listServers",
                );
                vec!["local".to_owned()]
            }
            Err(error) => {
                tracing::error!(
                    plugin = %self.plugin_id,
                    %error,
                    "backend list unavailable; answering the documented stand-in",
                );
                vec!["local".to_owned()]
            }
        }
    }

    fn get_player_roster(&self) -> Vec<PlayerRef> {
        match self.host_ipc.call(HostMsg::Roster) {
            Ok(HostResp::Roster(roster)) => roster,
            Ok(_) => {
                tracing::error!(
                    plugin = %self.plugin_id,
                    "host unit answered with an unexpected reply for getPlayerRoster",
                );
                Vec::new()
            }
            Err(error) => {
                tracing::error!(
                    plugin = %self.plugin_id,
                    %error,
                    "player roster unavailable; answering an empty list",
                );
                Vec::new()
            }
        }
    }

    fn ping_player(&self, request: &PlayerNameRequest) -> HostOk {
        match self.host_ipc.call(HostMsg::Roster) {
            Ok(HostResp::Roster(roster)) => {
                if !roster.iter().any(|player| player.name == request.name) {
                    return HostOk::err(format!("player `{}` is not online", request.name));
                }
            }
            Ok(_) => {
                return HostOk::err("host answered with an unexpected reply type");
            }
            Err(error) => {
                tracing::error!(
                    plugin = %self.plugin_id,
                    %error,
                    "player roster unavailable; refusing the ping",
                );
                return HostOk::err(format!("player roster unavailable ({error})"));
            }
        }
        match self
            .services
            .get_service::<PlayerPingSink>(PLAYERS_PING_SINK_KEY)
        {
            Some(sink) => {
                (sink.0)(&request.name);
                HostOk::ok()
            }
            None => HostOk::err("player messaging is not implemented yet"),
        }
    }

    fn switch_player(&self, _request: &SwitchPlayerRequest) -> HostOk {
        // no switch backend exists yet; the error is the honest answer
        // sent down to the plugin author
        HostOk::err("not implemented yet")
    }

    fn cancel_event(&self, request: &CancelEventRequest) -> HostOk {
        *self.pending_outcome.lock() = Some(OutcomeReport {
            cancelled: request.cancel,
            cancel_reason: request.reason.clone(),
        });
        HostOk::ok()
    }

    fn subscribe_to_events(&self, kinds: &[String]) -> HostOk {
        let mut subscribers = self.subscribers.lock();
        subscribers.clear();
        subscribers.extend(kinds.iter().cloned());
        HostOk::ok()
    }
}

/// Is `key` inside the namespace a plugin may address?
///
/// Plugins resolve their own `plugin.<id>.*` keys and the shared
/// `common.*` keys; everything else (core `candyriya.*`, other plugins'
/// `plugin.<other>.*`) answers with the key itself, like a missing key.
fn key_in_plugin_scope(key: &str, plugin_id: &str) -> bool {
    if key.starts_with("common.") {
        return true;
    }
    let own = format!("plugin.{plugin_id}.");
    key.starts_with(&own)
}

/// Render one JSON arg value as an interpolation string.
///
/// Strings pass through untouched; everything else is its compact JSON
/// form (`42`, `true`, `null`, `[1,2]`). A plugin that wants the raw
/// JSON it sent would have to interpolate it back out itself.
fn json_value_to_arg_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Truncate `s` to at most `max` bytes, never splitting a UTF-8
/// codepoint — the host-function boundary must not be crossed by an
/// unpaired byte.
fn utf8_truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut boundary = max;
    while !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

// ---- extism binding helpers ----

/// Run `f` with the borrowed host state, transparently recovering from a
/// poisoned mutex. Refusing to serve a plugin because another one poisoned
/// a mutex helps no one.
fn with_state<R>(
    user_data: &UserData<HostState>,
    f: impl FnOnce(&HostState) -> R,
) -> Result<R, extism::Error> {
    let arc = user_data.get()?;
    let state = arc.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(f(&state))
}

fn input_bytes(plugin: &mut CurrentPlugin, inputs: &[Val]) -> Result<Vec<u8>, extism::Error> {
    match inputs.first() {
        Some(input) => plugin.memory_get_val::<Vec<u8>>(input),
        None => Err(extism::Error::msg("host function called without an input")),
    }
}

fn output_bytes(
    plugin: &mut CurrentPlugin,
    outputs: &mut [Val],
    bytes: &[u8],
) -> Result<(), extism::Error> {
    let handle = plugin.memory_new(bytes)?;
    if let Some(first) = outputs.first_mut() {
        *first = plugin.memory_to_val(handle);
    }
    Ok(())
}

fn output_json(
    plugin: &mut CurrentPlugin,
    outputs: &mut [Val],
    value: &impl serde::Serialize,
) -> Result<(), extism::Error> {
    match serde_json::to_vec(value) {
        Ok(bytes) => output_bytes(plugin, outputs, &bytes),
        Err(error) => {
            tracing::error!(%error, "failed to serialize host function output");
            output_bytes(
                plugin,
                outputs,
                br#"{"ok":false,"error":"host serialization failed"}"#,
            )
        }
    }
}

fn output_error_json(
    plugin: &mut CurrentPlugin,
    outputs: &mut [Val],
    reason: &str,
) -> Result<(), extism::Error> {
    output_json(
        plugin,
        outputs,
        &serde_json::json!({ "ok": false, "error": reason }),
    )
}

/// Check the optional `plugin_id` field against the registered id.
///
/// An absent field is fine; a mismatched one is rejected. Host functions
/// never authorize against anything else the plugin sends.
fn check_plugin_id(state: &HostState, value: &Value) -> Result<(), String> {
    match value.get("plugin_id") {
        None => Ok(()),
        Some(Value::String(claimed)) if claimed == &state.plugin_id => Ok(()),
        Some(Value::String(claimed)) => {
            tracing::warn!(
                plugin = %state.plugin_id,
                claimed = %claimed,
                "plugin tried to act as another plugin; rejecting the call",
            );
            Err(format!(
                "plugin_id mismatch: this instance is registered as `{}`",
                state.plugin_id
            ))
        }
        Some(_) => Err("plugin_id must be a string".to_owned()),
    }
}

/// Parse a JSON *object* input and enforce the caller self-check.
fn parse_object_input<T: DeserializeOwned>(state: &HostState, bytes: &[u8]) -> Result<T, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("input is not valid JSON: {error}"))?;
    if !value.is_object() {
        return Err("input must be a JSON object".to_owned());
    }
    check_plugin_id(state, &value)?;
    serde_json::from_value(value).map_err(|error| format!("input: {error}"))
}

// ---- the bindings ----

fn hello_world(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let input = input_bytes(plugin, inputs)?;
    let name = String::from_utf8_lossy(&input);
    let reply = with_state(&user_data, |state| state.hello_world(&name))?;
    output_bytes(plugin, outputs, reply.as_bytes())
}

fn i18n_t(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let bytes = input_bytes(plugin, inputs)?;
    let parsed = with_state(&user_data, |state| {
        parse_object_input::<I18nTRequest>(state, &bytes)
    })?;
    let request = match parsed {
        Ok(request) => request,
        Err(reason) => return output_error_json(plugin, outputs, &reason),
    };
    let reply = with_state(&user_data, |state| state.i18n_t(&request))?;
    output_bytes(plugin, outputs, reply.as_bytes())
}

fn i18n_locale(
    plugin: &mut CurrentPlugin,
    _inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let locale = with_state(&user_data, |state| state.i18n_locale())?;
    output_bytes(plugin, outputs, locale.as_bytes())
}

fn get_locale(
    plugin: &mut CurrentPlugin,
    _inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let locale = with_state(&user_data, |state| state.get_locale())?;
    output_bytes(plugin, outputs, locale.as_bytes())
}

fn call_api(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let bytes = input_bytes(plugin, inputs)?;
    let parsed = with_state(&user_data, |state| {
        parse_object_input::<CallApiRequest>(state, &bytes)
    })?;
    let request = match parsed {
        Ok(request) => request,
        Err(reason) => return output_error_json(plugin, outputs, &reason),
    };
    let response = with_state(&user_data, |state| state.call_api(&request))?;
    output_json(plugin, outputs, &response)
}

fn log_message(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let bytes = input_bytes(plugin, inputs)?;
    let parsed = with_state(&user_data, |state| {
        parse_object_input::<LogMessage>(state, &bytes)
    })?;
    let message = match parsed {
        Ok(message) => message,
        Err(reason) => return output_error_json(plugin, outputs, &reason),
    };
    let response = with_state(&user_data, |state| state.log_message(&message))?;
    output_json(plugin, outputs, &response)
}

fn list_servers(
    plugin: &mut CurrentPlugin,
    _inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let servers = with_state(&user_data, |state| state.list_servers())?;
    output_json(plugin, outputs, &servers)
}

fn get_player_roster(
    plugin: &mut CurrentPlugin,
    _inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let roster = with_state(&user_data, |state| state.get_player_roster())?;
    output_json(plugin, outputs, &roster)
}

fn ping_player(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let bytes = input_bytes(plugin, inputs)?;
    let parsed = with_state(&user_data, |state| {
        parse_object_input::<PlayerNameRequest>(state, &bytes)
    })?;
    let request = match parsed {
        Ok(request) => request,
        Err(reason) => return output_error_json(plugin, outputs, &reason),
    };
    let response = with_state(&user_data, |state| state.ping_player(&request))?;
    output_json(plugin, outputs, &response)
}

fn switch_player(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let bytes = input_bytes(plugin, inputs)?;
    let parsed = with_state(&user_data, |state| {
        parse_object_input::<SwitchPlayerRequest>(state, &bytes)
    })?;
    let request = match parsed {
        Ok(request) => request,
        Err(reason) => return output_error_json(plugin, outputs, &reason),
    };
    let response = with_state(&user_data, |state| state.switch_player(&request))?;
    output_json(plugin, outputs, &response)
}

fn cancel_event(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let bytes = input_bytes(plugin, inputs)?;
    let parsed = with_state(&user_data, |state| {
        parse_object_input::<CancelEventRequest>(state, &bytes)
    })?;
    let request = match parsed {
        Ok(request) => request,
        Err(reason) => return output_error_json(plugin, outputs, &reason),
    };
    let response = with_state(&user_data, |state| state.cancel_event(&request))?;
    output_json(plugin, outputs, &response)
}

fn subscribe_to_events(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let bytes = input_bytes(plugin, inputs)?;
    let kinds: Vec<String> = match serde_json::from_slice(&bytes) {
        Ok(kinds) => kinds,
        Err(error) => {
            return output_error_json(
                plugin,
                outputs,
                &format!("input is not a JSON array of event kind strings: {error}"),
            );
        }
    };
    let response = with_state(&user_data, |state| state.subscribe_to_events(&kinds))?;
    output_json(plugin, outputs, &response)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    // mirrors the byte cap in `log_message`
    const LOG_MSG_CAP: usize = 1024;

    #[test]
    fn plugin_scope_covers_own_namespace_and_common() {
        for key in ["plugin.my-plugin.line", "common.yes"] {
            assert!(key_in_plugin_scope(key, "my-plugin"), "{key}");
        }
        for key in [
            "plugin.other.line",
            "plugin.my-plugin",    // no sub-key: not a key anyone should resolve
            "candyriya.reload.ok", // core
            "common",              // bare prefix is not common.*
            "plugin_.my-plugin.x", // no dot after "plugin"
        ] {
            assert!(!key_in_plugin_scope(key, "my-plugin"), "{key}");
        }
    }

    #[test]
    fn json_args_render_into_placeholders() {
        assert_eq!(
            json_value_to_arg_string(&serde_json::json!("plain")),
            "plain"
        );
        assert_eq!(json_value_to_arg_string(&serde_json::json!(42)), "42");
        assert_eq!(json_value_to_arg_string(&serde_json::json!(true)), "true");
        assert_eq!(json_value_to_arg_string(&serde_json::json!(null)), "null");
        assert_eq!(
            json_value_to_arg_string(&serde_json::json!({"a": 1})),
            "{\"a\":1}"
        );
    }

    #[test]
    fn log_message_truncation_lands_on_a_utf8_boundary() {
        // '€' is 3 bytes (E2 82 AC): byte 1024 falls mid-codepoint
        // (char 341 occupies bytes 1023..1026)
        let msg = "€".repeat(400);
        assert!(msg.len() > LOG_MSG_CAP);

        let truncated = utf8_truncate(&msg, LOG_MSG_CAP);

        assert!(truncated.len() <= LOG_MSG_CAP);
        assert!(msg.is_char_boundary(truncated.len()));
        assert!(
            std::str::from_utf8(truncated.as_bytes()).is_ok(),
            "a truncated multibyte log line must stay valid UTF-8"
        );

        // and the host function itself must not panic on the same input
        let state = HostState::new(
            PathBuf::from("target/log-truncation-test"),
            "log-truncation-test",
        );
        let response = state.log_message(&LogMessage {
            level: LogLevel::Info,
            msg,
        });
        assert!(response.ok);
    }
}
