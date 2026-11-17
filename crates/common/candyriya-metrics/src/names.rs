//! Metric names shared by the candyriya proxy.
//!
//! Every constant in this module is the exact string the Prometheus exporter
//! will put on the wire, so dashboards and alert rules can reference these
//! names directly and `grep` stays trustworthy both ways.
//!
//! Conventions used throughout:
//!
//! - Counters carry the `_total` suffix in the constant itself. Recommended
//!   naming is NOT enabled in the exporter, so what you see in the const is
//!   what gets exported, with no transformations applied.
//! - Histograms carry the unit in the name (`_seconds`) and are rendered as
//!   true Prometheus histograms with [`crate::DEFAULT_HISTOGRAM_BUCKETS`].
//! - Names are plain ASCII `snake_case`; no dots, no double underscores.
//!
//! Label conventions are documented on each constant. The proxy adds one
//! global label `service="candyriya"` to every metric, on top of whatever the
//! call site sets.
//!
//! This module holds constants only. Any logic lives in the crate root.

/// Gauge. Players currently connected to the proxy, in any session state
/// (right after the TCP accept, during handshake, logged in, playing).
///
/// No labels.
pub const NAME_PLAYERS_CONNECTED: &str = "candyriya_players_connected";

/// Counter. Inbound TCP connections accepted by the accept loop. This fires
/// before any handshake validation, so it counts pings, scrapes against the
/// proxy port and garbage floods, not just players.
///
/// No labels.
pub const NAME_CONNECTIONS_ACCEPTED_TOTAL: &str = "candyriya_connections_accepted_total";

/// Counter. Handshakes rejected before status or login could proceed.
///
/// Labels:
///
/// - `reason` (required): what killed the handshake. Well-known values used
///   by the proxy are `invalid_protocol`, `invalid_intent`,
///   `malformed_packet`, `timeout` and `unsupported_version`, but the label
///   is effectively free-form — keep the set of values bounded and
///   human-curated, never raw error strings, to avoid label cardinality
///   explosions against untrusted clients.
pub const NAME_HANDSHAKE_FAILURES_TOTAL: &str = "candyriya_handshake_failures_total";

/// Counter. Login sequences that ran to completion.
///
/// Labels:
///
/// - `mode` (required): `online` or `offline`, matching the proxy's
///   authentication mode for the session.
pub const NAME_LOGINS_TOTAL: &str = "candyriya_logins_total";

/// Histogram. Wall-clock time from the first login packet until the login
/// outcome is decided (success or failure), in seconds.
///
/// No labels. Rendered as a true Prometheus histogram with
/// [`crate::DEFAULT_HISTOGRAM_BUCKETS`] as the bucket bounds (login involves
/// a sessionserver roundtrip in online mode, so the upper buckets matter).
pub const NAME_LOGIN_DURATION_SECONDS: &str = "candyriya_login_duration_seconds";

/// Counter. Minecraft packets forwarded through the proxy in either
/// direction. Increments per packet, not per byte.
///
/// Labels:
///
/// - `direction` (required): `client_to_server` or `server_to_client`.
pub const NAME_PACKETS_FORWARDED_TOTAL: &str = "candyriya_packets_forwarded_total";

/// Counter. Bytes forwarded through the proxy in either direction, counting
/// only application payload, excluding anything the transport itself added.
///
/// Labels:
///
/// - `direction` (required): `client_to_server` or `server_to_client`.
pub const NAME_BYTES_FORWARDED_TOTAL: &str = "candyriya_bytes_forwarded_total";

/// Gauge. Backends in the registry whose last health check succeeded.
/// Sampled from the backend registry state, not from a live probe at scrape
/// time.
///
/// No labels.
pub const NAME_BACKENDS_ONLINE: &str = "candyriya_backends_online";

/// Gauge. Duration of the most recent health-check status ping to a backend,
/// in seconds. Scrapes see the value left by the last check; this is a
/// gauge (last observation), not a latency summary, by design — one series
/// per backend, bounded by the size of the backend registry.
///
/// Labels:
///
/// - `backend` (required): the backend's name as declared in the config's
///   server registry. Not the address — names are stable across restarts
///   and address changes.
pub const NAME_BACKEND_PING_SECONDS: &str = "candyriya_backend_ping_seconds";

/// Counter. Sessions that ended: client disconnects, backend disconnects,
/// admin kicks, timeouts and auth failures all land here.
///
/// Labels:
///
/// - `reason` (required): well-known values used by the proxy are
///   `client_disconnect`, `backend_disconnect`, `kick`, `timeout`,
///   `auth_failure` and `other`, but the label is effectively free-form —
///   keep the set of values bounded and human-curated, never raw error
///   strings, to avoid label cardinality explosions against untrusted
///   peers.
pub const NAME_DISCONNECTS_TOTAL: &str = "candyriya_disconnects_total";

/// Counter. Plugin calls, counted once per call attempt, labelled by the
/// outcome the call settled on.
///
/// Labels:
///
/// - `plugin` (required): the plugin id from the manifest. Bounded by the
///   number of plugins the operator loads.
/// - `outcome` (required): one of `ok`, `timeout`, `failed`,
///   `fuel_exhausted` or `cancelled`. `cancelled` is only produced by the
///   event dispatcher, which knows the plugin cancelled the event; the
///   instance call path knows only the mechanical result (`ok`, `timeout`,
///   `failed`, `fuel_exhausted`). Calls rejected before the plugin ever ran
///   (input too large, rate limited) are recorded as `failed`; mailbox-full
///   rejections are NOT counted here — see [`NAME_PLUGIN_BUSY_TOTAL`].
pub const NAME_PLUGIN_CALLS_TOTAL: &str = "candyriya_plugin_calls_total";

/// Histogram. Wall-clock time for one plugin call, from the moment the
/// caller starts waiting for the plugin until the answer (or the timeout)
/// comes back, in seconds.
///
/// Labels:
///
/// - `plugin` (required): the plugin id from the manifest.
///
/// Rendered as a true Prometheus histogram with
/// [`crate::DEFAULT_HISTOGRAM_BUCKETS`] as the bucket bounds. A shared
/// bucket ladder is fine here: an `on_event` call that takes seconds is the
/// signal, and the existing bounds bracket it. Calls rejected before the
/// plugin ran are not observed (there is no call to time).
pub const NAME_PLUGIN_CALL_DURATION_SECONDS: &str = "candyriya_plugin_call_duration_seconds";

/// Gauge. Cumulative seconds the plugin's dedicated worker thread spent
/// inside wasm calls. Monotonically increasing while the plugin lives.
///
/// Labels:
///
/// - `plugin` (required): the plugin id from the manifest.
///
/// The `metrics` facade has no float counters (`Counter::increment` is
/// integer-only), so the cumulative seconds ride on a gauge; it only ever
/// goes up. It measures the wall-clock span of the wasm call on the worker
/// thread, not CPU time — true per-thread CPU time is not obtainable in
/// safe Rust. A call that never returns (infinite wasm loop) is not
/// accounted until the runtime interrupts it (fuel exhaustion or the
/// manifest timeout).
pub const NAME_PLUGIN_THREAD_TIME_SECONDS_TOTAL: &str =
    "candyriya_plugin_thread_time_seconds_total";

/// Gauge. Plugin calls currently being executed or awaited: incremented
/// when a call is sent into the plugin's mailbox and decremented when the
/// answer (or the timeout) comes back. A hung plugin whose callers all gave
/// up keeps this at zero; a plugin mid-wasm shows the in-flight count.
///
/// Labels:
///
/// - `plugin` (required): the plugin id from the manifest.
pub const NAME_PLUGIN_ACTIVE_CALLS: &str = "candyriya_plugin_active_calls";

/// Counter. Calls rejected because the plugin's mailbox was full — the
/// plugin is busy with earlier work and there is nowhere to queue. This is
/// the `PluginError::Busy` path; anything else that rejects a call is a
/// plain call failure, not busyness.
///
/// Labels:
///
/// - `plugin` (required): the plugin id from the manifest.
pub const NAME_PLUGIN_BUSY_TOTAL: &str = "candyriya_plugin_busy_total";

/// Gauge. Number of live wasm instances for this plugin — `1` while the
/// plugin is loaded, `0` once the instance is gone (unload, reload swap,
/// failed `plugin_main` check or a broken worker), briefly `2` while a
/// hot-reload replacement and the outgoing instance coexist. Kept with the
/// instance rather than removed on unload: series persist, which keeps
/// dashboards and alert rules stable across unload/reload cycles.
///
/// Labels:
///
/// - `plugin` (required): the plugin id from the manifest.
pub const NAME_PLUGIN_LOADED: &str = "candyriya_plugin_loaded";

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_NAMES: &[&str] = &[
        NAME_PLAYERS_CONNECTED,
        NAME_CONNECTIONS_ACCEPTED_TOTAL,
        NAME_HANDSHAKE_FAILURES_TOTAL,
        NAME_LOGINS_TOTAL,
        NAME_LOGIN_DURATION_SECONDS,
        NAME_PACKETS_FORWARDED_TOTAL,
        NAME_BYTES_FORWARDED_TOTAL,
        NAME_BACKENDS_ONLINE,
        NAME_BACKEND_PING_SECONDS,
        NAME_DISCONNECTS_TOTAL,
        NAME_PLUGIN_CALLS_TOTAL,
        NAME_PLUGIN_CALL_DURATION_SECONDS,
        NAME_PLUGIN_THREAD_TIME_SECONDS_TOTAL,
        NAME_PLUGIN_ACTIVE_CALLS,
        NAME_PLUGIN_BUSY_TOTAL,
        NAME_PLUGIN_LOADED,
    ];

    // prometheus data model: metric names must match [a-zA-Z_:][a-zA-Z0-9_:]*
    fn is_valid_prometheus_name(name: &str) -> bool {
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first.is_ascii_alphabetic() || first == '_' || first == ':') {
            return false;
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    }

    #[test]
    fn every_const_is_a_valid_prometheus_name() {
        for name in ALL_NAMES {
            assert!(!name.is_empty(), "metric name must not be empty");
            assert!(name.is_ascii(), "metric name must be ASCII: {name}");
            assert!(
                is_valid_prometheus_name(name),
                "metric name must match [a-zA-Z_:][a-zA-Z0-9_:]*: {name}"
            );
        }
    }

    #[test]
    fn metric_names_are_unique() {
        let mut sorted = ALL_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ALL_NAMES.len(),
            "duplicate metric names declared"
        );
    }
}
