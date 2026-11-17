//! The plugin lifecycle events, fired from the proxy.
//!
//! This module is the only place the proxy builds a
//! [`ProxyEvent`](candyriya_plugin_api::ProxyEvent) and hands it to the
//! shared [`EventDispatcher`](candyriya_plugin_api::EventDispatcher)
//! handle. Every call returns
//! the aggregate answer so the call site can honor a plugin cancellation
//! (deny the action) — or, for the kick and disconnect kinds, exactly
//! ignore it, because a kick that was already decided cannot be un-kicked
//! and a TCP connection that is already closing cannot be un-closed.
//!
//! # Fire points and what cancellation means there
//!
//! | kind | fires | cancelled means |
//! |---|---|---|
//! | `connection_allowed` | in the login pipeline, after the profile is resolved, before any backend is contacted | the join is denied; the client gets a login disconnect carrying the plugin's reason (or a localized generic one when none) |
//! | `connection_kicked` | whenever the proxy decides to kick a connection the plugins know about | ignored; fired for observation |
//! | `login` | in the backend exchange, after the backend answers Login Success (the modern-forwarding ack has already happened) but **before the client is told** | the login is denied: no Login Success reaches the client, instead it gets a login disconnect with the plugin's reason and no backend is kept |
//! | `switch_server` | on a kick-fallback hop in the backend chain (one backend kicked/failed, the chain moves to the next candidate) | this hop is denied: the candidate is skipped without connecting, and the chain (or, when it runs out, the final disconnect) names the plugin's reason |
//! | `disconnect` | when the connection task ends, with the best-known identity | ignored; fired for observation |
//!
//! A connection the proxy never got as far as reading a Login Start for
//! (a handshake failure, a sneak client, a status ping) carries no account
//! identity, and none of these events fire for it — notifying plugins
//! about a player that never was one would be noise.
//!
//! # Identity before authentication
//!
//! Pre-authentication events (`connection_kicked` on an early kick) have no
//! verified account UUID yet. They carry the offline-mode derivation of
//! the announced name instead — stable, deterministic, and exactly what
//! offline mode would use. Once the profile is resolved, every event
//! carries the real UUID.
//!
//! # Unwired is silent
//!
//! When the composition root never registers a dispatcher (embedded
//! tooling, most proxy tests), [`fire`] is a no-op and returns `None`;
//! the proxy behaves byte-for-byte as it did before the wiring landed.
//!
//! # Kick plumbing
//!
//! The client-facing kick text for a plugin-denied action comes from
//! [`plugin_cancel_kick`]: the plugin's `cancel_reason` verbatim (it is
//! the plugin's own user-facing text, exactly like a backend's kick
//! reason passed through today), or the localized
//! `candyriya.kick.plugin_cancelled` string when the plugin cancelled
//! silently. The localized path parses through the strict MiniMessage
//! gate and falls back to a literal-text component on any failure, so a
//! broken locale override never leaks raw tags to the client.
//!
//! # What `ip:port` means here
//!
//! `ConnectionAllowed.address` is the peer address of the TCP connection,
//! `ip:port`. This proxy performs no client-address resolution of its own
//! (there is no HAProxy/forwarding-header parsing on the client link), so
//! the peer address is the address the client connected from, verbatim.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use candyriya_plugin_api::{DispatchResult, EventEnvelope, EventKind, Outcome, parse_minimessage};

use crate::accept::ProxyState;
use crate::login::MAX_KICK_REASON_CHARS;

/// The i18n key for the kick screen shown when a plugin denies an action
/// without giving a reason. Plain words; never shown raw to a client —
/// rendered through the component path in [`plugin_cancel_kick`].
const PLUGIN_CANCELLED_KEY: &str = "candyriya.kick.plugin_cancelled";

/// Used only when neither a wired [`candyriya_i18n::I18n`] handle nor a
/// working embedded `en` fallback is available — a build invariant, not
/// a runtime condition, but no client may ever see a bare i18n key.
const PLUGIN_CANCELLED_PLAIN: &str = "Kicked by a plugin.";

/// What plugins learn as the reason when one of them cancelled without
/// naming one. Static, not user-facing — plugins read it, players never
/// do (they get the localized component instead).
pub(crate) const PLUGIN_CANCELLED_EVENT_REASON: &str = "plugin cancelled the event";

/// Best-known identity and outcome of one connection.
///
/// Filled in as the login pipeline advances; read once at the end to fire
/// `disconnect`. Fields stay `None` until the corresponding fact exists —
/// a connection that died before the Login Start was read has no identity
/// and fires nothing.
#[derive(Debug, Default)]
pub(crate) struct SessionSummary {
    /// The account UUID, once the profile is resolved. Until then the
    /// offline-mode derivation of the announced name is used instead.
    pub(crate) uuid: Option<String>,
    /// The announced or resolved player name.
    pub(crate) name: Option<String>,
    /// The backend the player was on when the session ended. Empty when
    /// the connection never reached the play state.
    pub(crate) server: Option<String>,
    /// Why the session ended, when the proxy chose one (a kick). `None`
    /// for the silent-transport case — the disconnect event reports
    /// `connection closed` then.
    pub(crate) reason: Option<String>,
}

impl SessionSummary {
    /// The identity to report to plugins, best effort: the resolved UUID
    /// when there is one, otherwise the offline-mode derivation of the
    /// name. `None` when not even a name is known.
    pub(crate) fn event_identity(&self) -> Option<(String, &str)> {
        let name = self.name.as_deref()?;
        let uuid = self
            .uuid
            .clone()
            .unwrap_or_else(|| candyriya_player::offline_uuid(name).to_string());
        Some((uuid, name))
    }
}

/// Mint a fresh envelope for `kind` and fan it out.
///
/// Returns the aggregate answer, or `None` when no dispatcher is wired.
/// Per-plugin failures (broken export, undecodable answer) are logged
/// here; timeouts are logged by the dispatcher itself.
pub(crate) fn fire(state: &ProxyState, kind: EventKind) -> Option<DispatchResult> {
    let dispatcher = state.event_dispatcher.as_ref()?;
    let kind_name = kind.as_str().to_owned();
    let event = candyriya_plugin_api::ProxyEvent::new(EventEnvelope::new(
        state.event_ids.next_id(),
        kind,
        now_unix_ms(),
    ));
    let result = dispatcher.dispatch_event(&event);
    for (plugin, outcome) in &result.per_plugin_outcomes {
        if let Outcome::Failed { error } = outcome {
            tracing::warn!(
                plugin = %plugin,
                event = %kind_name,
                %error,
                "plugin failed the event dispatch",
            );
        }
    }
    Some(result)
}

/// Fire `connection_kicked`, when an identity is known.
///
/// `reason` is the plain carrier-free text naming the kick cause; it is
/// what plugins log or route on. Kicked before the Login Start was read
/// there is no account to name and the event does not fire.
pub(crate) fn fire_kicked(state: &ProxyState, session: &SessionSummary, reason: &str) {
    let Some((uuid, name)) = session.event_identity() else {
        return;
    };
    let kind = EventKind::ConnectionKicked {
        uuid,
        name: name.to_owned(),
        reason: reason.to_owned(),
    };
    let _ = fire(state, kind);
}

/// Fire `disconnect`, when an identity is known.
///
/// The reason is a static, carrier-free label for the transport case
/// (`connection closed`) or the plain reason the proxy chose for a kick.
/// A connection that never produced an account identity fires nothing.
pub(crate) fn fire_disconnect(state: &ProxyState, session: &SessionSummary) {
    let Some((uuid, name)) = session.event_identity() else {
        return;
    };
    let kind = EventKind::Disconnect {
        uuid,
        name: name.to_owned(),
        server: session.server.clone().unwrap_or_default(),
        reason: session
            .reason
            .clone()
            .unwrap_or_else(|| "connection closed".to_owned()),
    };
    let _ = fire(state, kind);
}

/// The kick text and wire payload for a plugin-denied action.
///
/// `event_reason` is what plugins and the kick/disconnect events see:
/// the cancelling plugin's `cancel_reason` verbatim — it is the plugin's
/// own user-facing text, passed through exactly like a backend's kick
/// reason is today — or a static marker when the plugin cancelled
/// silently. `packet_json` is the serialized Text Component the login
/// disconnect carries: the plugin's text as a literal-text component, or
/// the localized `candyriya.kick.plugin_cancelled` rendering.
pub(crate) struct KickMessage {
    /// Carrier-free reason for plugins and the kick events.
    pub(crate) event_reason: String,
    /// The Text Component JSON for the login disconnect packet.
    pub(crate) packet_json: String,
}

/// Build the kick message for a plugin-vetoed action.
pub(crate) fn plugin_cancel_kick(state: &ProxyState, reason: Option<&str>) -> KickMessage {
    match reason {
        // the plugin wrote its own words for the client; they go on the
        // screen verbatim, only clamped to something a packet can carry
        Some(reason) => {
            let text: String = reason.chars().take(MAX_KICK_REASON_CHARS).collect();
            KickMessage {
                event_reason: reason.to_owned(),
                packet_json: json!({ "text": text }).to_string(),
            }
        }
        None => {
            let text = match &state.event_i18n {
                Some(i18n) => i18n.t(PLUGIN_CANCELLED_KEY, &[]),
                None => match candyriya_i18n::I18n::new("en") {
                    Ok(i18n) => i18n.t(PLUGIN_CANCELLED_KEY, &[]),
                    // the embedded en table failing to build is a broken
                    // build; a plain fallback still beats a bare key
                    Err(_) => PLUGIN_CANCELLED_PLAIN.to_owned(),
                },
            };
            // locale values are MiniMessage; strictly gate them before
            // the client parses them, literal text on any failure
            let packet_json = match parse_minimessage(&text) {
                Ok(component) => serde_json::to_string(&component)
                    .unwrap_or_else(|_| json!({ "text": text }).to_string()),
                Err(_) => json!({ "text": text }).to_string(),
            };
            KickMessage {
                event_reason: PLUGIN_CANCELLED_EVENT_REASON.to_owned(),
                packet_json,
            }
        }
    }
}

/// Seconds since the Unix epoch in milliseconds, for the envelope stamp.
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
