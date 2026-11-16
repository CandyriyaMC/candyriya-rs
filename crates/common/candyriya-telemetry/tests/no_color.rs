//! `NO_COLOR` (presence-based) must force colors off, overriding every
//! `Colors` mode — even `Always`.
//!
//! Note: in the 2024 edition `std::env::set_var`/`remove_var` are `unsafe`,
//! which the workspace forbids, so this test drives the environment through
//! `temp_env`, which encapsulates the unsafe calls and restores the variable
//! afterwards (even on panic) and serializes access with a mutex.

use candyriya_telemetry::{Colors, LogConfig, init};

#[test]
fn no_color_forces_colors_off() {
    temp_env::with_var("NO_COLOR", Some("1"), || {
        assert!(!LogConfig::new().colors(Colors::Auto).effective_ansi());
        assert!(!LogConfig::new().colors(Colors::Always).effective_ansi());
        assert!(!LogConfig::new().colors(Colors::Never).effective_ansi());
        // The full pipeline still initializes with NO_COLOR set.
        let guard = init(&LogConfig::default().without_console()).expect("init must succeed");
        drop(guard);
    });
}

#[test]
fn empty_no_color_also_disables_colors() {
    // Presence is enough; an empty value disables colors too (no-color.org).
    temp_env::with_var("NO_COLOR", Some(""), || {
        assert!(!LogConfig::new().colors(Colors::Always).effective_ansi());
    });
}

#[test]
fn no_color_absent_falls_back_to_mode() {
    temp_env::with_var_unset("NO_COLOR", || {
        assert!(LogConfig::new().colors(Colors::Always).effective_ansi());
        assert!(!LogConfig::new().colors(Colors::Never).effective_ansi());
        // Auto on a non-TTY stdout (the test harness captures it) disables colors.
        assert!(!LogConfig::new().colors(Colors::Auto).effective_ansi());
    });
}
