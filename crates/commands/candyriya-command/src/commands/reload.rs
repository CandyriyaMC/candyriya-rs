//! `/candyriya reload` — hot-reload what can be reloaded without a restart.
//!
//! Two phases, selected by the (optional) trailing literal:
//!
//! - `config` — re-read `candyriya.toml` from the path the proxy was
//!   started with. A failure leaves the running configuration untouched;
//!   on success the backend registry is rebuilt and swapped atomically and
//!   the favicon / locales are refreshed. The listener, the session
//!   layer and the forwarding secret are never touched.
//! - `plugins` — reload every plugin from disk (existing records are
//!   swapped in place; newly discovered plugins are registered). The
//!   running set survives any failure; plugins whose files vanished are
//!   kept and only logged.
//! - no argument — both phases, config first.
//!
//! The phases run through the [`ReloadFn`](crate::ReloadFn) the composition
//! root wires in; this module owns parsing, permission context and
//! rendering — never the mechanism itself.

use candyriya_i18n::I18n;

use crate::effects::{ReloadFn, ReloadReport, ReloadScope};
use crate::source::CommandSource;
use crate::state::CommandState;
use crate::text::escape_minimessage;

use super::common::out;

fn render_config(
    t: &I18n,
    state: &CommandState,
    source: &CommandSource,
    outcome: &Result<String, String>,
) {
    match outcome {
        Ok(_) => out(state, source, t.t("candyriya.reload.config_ok", &[])),
        Err(reason) => {
            tracing::error!(%reason, "configuration reload failed; the running configuration is unchanged");
            out(
                state,
                source,
                t.t(
                    "candyriya.config.reload.failed",
                    &[("reason", escape_minimessage(reason).as_str())],
                ),
            );
        }
    }
}

fn render_plugins(
    t: &I18n,
    state: &CommandState,
    source: &CommandSource,
    phase: &crate::PluginPhaseReport,
) {
    let reloaded = phase.reloaded.to_string();
    let failed = phase.failed.to_string();
    if phase.failed == 0 {
        out(
            state,
            source,
            t.t(
                "candyriya.reload.plugins_summary",
                &[("reloaded", reloaded.as_str()), ("failed", failed.as_str())],
            ),
        );
    } else {
        tracing::error!(
            reloaded = phase.reloaded,
            failed = phase.failed,
            "plugin reload completed with failures; running instances were kept"
        );
        out(
            state,
            source,
            t.t(
                "candyriya.reload.plugins_summary_failed",
                &[("reloaded", reloaded.as_str()), ("failed", failed.as_str())],
            ),
        );
    }
    for detail in &phase.details {
        out(
            state,
            source,
            t.t(
                "candyriya.reload.detail",
                &[("detail", escape_minimessage(detail).as_str())],
            ),
        );
    }
}

/// The `/candyriya reload` handler. `reload` is `None` in builds that did
/// not wire one (tests, embedding); the command answers with a notice
/// instead of pretending.
pub(crate) fn handle_reload(
    state: &CommandState,
    source: &CommandSource,
    reload: &Option<ReloadFn>,
    scope: ReloadScope,
) -> i32 {
    let t = state.locales.pick(source);
    let Some(reload) = reload else {
        out(state, source, t.t("candyriya.reload.unavailable", &[]));
        return 0;
    };
    let ReloadReport { config, plugins } = reload(scope);
    let mut ok = true;
    // the scope selects the phases; a closure that reports a skipped
    // phase is not allowed to smuggle its output in
    if matches!(scope, ReloadScope::All | ReloadScope::Config)
        && let Some(config) = &config
    {
        if config.is_err() {
            ok = false;
        }
        render_config(&t, state, source, config);
    }
    if matches!(scope, ReloadScope::All | ReloadScope::Plugins)
        && let Some(plugins) = &plugins
    {
        if plugins.failed != 0 {
            ok = false;
        }
        render_plugins(&t, state, source, plugins);
    }
    if ok { 1 } else { 0 }
}
