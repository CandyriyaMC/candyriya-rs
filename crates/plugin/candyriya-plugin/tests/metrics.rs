//! Per-plugin metrics: load/unload lifecycle, call outcomes, duration,
//! busy rejections. The Prometheus recorder is one-shot per process, so
//! this file installs it once (first caller wins) and every test scrapes
//! the shared registry for its own uniquely-named plugins. Series values
//! for other plugins in the same binary are not asserted on.
//!
//! Fixtures are real extism plugins compiled from WAT at test time.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::OnceLock;
use std::time::Duration;

use candyriya_metrics::{MetricsError, install};
use candyriya_plugin::{LibraryPlugin, PluginLoader, PluginSet, ProxyEventDispatcher};
use candyriya_plugin_api::{EventEnvelope, EventKind, ProxyEvent};

use common::Fixture;

fn login_event() -> ProxyEvent {
    ProxyEvent::new(EventEnvelope::new(
        1,
        EventKind::Login {
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
            name: "Notch".to_owned(),
            protocol_version: 769,
        },
        1_730_000_000_000,
    ))
}

fn ensure_installed() -> SocketAddr {
    static ADDR: OnceLock<SocketAddr> = OnceLock::new();
    *ADDR.get_or_init(|| {
        // same trick as candyriya-metrics' test: bind :0, drop, reuse
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match install(Some(addr)) {
            Ok(_handle) => addr,
            Err(MetricsError::AlreadyInstalled) => {
                unreachable!("this test binary is the only installer in its process")
            }
            Err(e) => panic!("metrics install failed: {e}"),
        }
    })
}

fn load_plugins(fx: &Fixture, plugins: &[(&str, Vec<u8>)]) -> Vec<LibraryPlugin> {
    for (id, wasm) in plugins {
        common::write_plugin(&fx.plugins_dir, id, wasm);
    }
    let report =
        PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
    assert!(
        report.issues.is_empty(),
        "unexpected issues: {:?}",
        report.issues
    );
    report.plugins
}

/// The value of the single exposition line carrying all `labels`, or
/// `None` when the series is absent.
fn series_value(body: &str, metric: &str, labels: &[&str]) -> Option<f64> {
    body.lines()
        .find(|line| {
            line.starts_with(metric)
                && line.contains('{')
                && labels.iter().all(|label| line.contains(*label))
        })
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

fn scrape(addr: SocketAddr) -> String {
    let mut last_err = std::io::Error::other("no attempt made");
    for _ in 0..50 {
        match fetch(addr) {
            Ok(body) => return body,
            Err(e) => {
                last_err = e;
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    panic!("metrics endpoint at {addr} never served: {last_err}");
}

fn fetch(addr: SocketAddr) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let raw = String::from_utf8_lossy(&raw);
    Ok(raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string())
}

#[test]
fn loaded_gauge_tracks_instance_lifetime() {
    let addr = ensure_installed();
    let fx = Fixture::new();
    let mut plugins = load_plugins(
        &fx,
        &[(
            "metrics-loaded-a",
            common::response_plugin_wasm("{\"cancelled\":false}"),
        )],
    );
    assert_eq!(plugins.len(), 1);

    let body = scrape(addr);
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_loaded",
            &["plugin=\"metrics-loaded-a\""]
        ),
        Some(1.0),
        "a live instance must read 1, got:\n{body}"
    );

    plugins.pop().expect("one plugin").unload().expect("unload");

    let body = scrape(addr);
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_loaded",
            &["plugin=\"metrics-loaded-a\""]
        ),
        Some(0.0),
        "a dropped instance must read 0, got:\n{body}"
    );
}

#[test]
fn ok_call_records_outcome_and_duration() {
    let addr = ensure_installed();
    let fx = Fixture::new();
    let plugins = load_plugins(
        &fx,
        &[(
            "metrics-ok-a",
            common::response_plugin_wasm("{\"cancelled\":false}"),
        )],
    );
    let set = PluginSet::from_vec(plugins);
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());
    assert!(!result.cancelled);

    let body = scrape(addr);
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_calls_total",
            &["plugin=\"metrics-ok-a\"", "outcome=\"ok\""]
        ),
        Some(1.0),
        "one dispatch must count one ok call, got:\n{body}"
    );
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_call_duration_seconds_count",
            &["plugin=\"metrics-ok-a\""],
        ),
        Some(1.0),
        "one dispatch must observe one call duration, got:\n{body}"
    );
    assert!(
        body.contains("# HELP candyriya_plugin_calls_total"),
        "the describe calls at load must surface HELP lines, got:\n{body}"
    );
    assert!(
        series_value(
            &body,
            "candyriya_plugin_thread_time_seconds_total",
            &["plugin=\"metrics-ok-a\""]
        )
        .is_some(),
        "the worker must account wasm thread time, got:\n{body}"
    );
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_active_calls",
            &["plugin=\"metrics-ok-a\""]
        ),
        Some(0.0),
        "no calls in flight after the dispatch returns, got:\n{body}"
    );
}

#[test]
fn timeout_call_records_timeout_outcome() {
    let addr = ensure_installed();
    let fx = Fixture::new();
    let plugins = load_plugins(&fx, &[("metrics-timeout-a", common::slow_plugin_wasm())]);
    let set = PluginSet::from_vec(plugins);
    // the infinite wasm loop never answers; the deadline ends the wait
    let dispatcher = ProxyEventDispatcher::with_timeout(set, Duration::from_millis(100));

    let result = dispatcher.dispatch_cancellable(&login_event());
    assert_eq!(
        result.outcome_for("metrics-timeout-a"),
        Some(&candyriya_plugin::Outcome::Timeout)
    );

    let body = scrape(addr);
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_calls_total",
            &["plugin=\"metrics-timeout-a\"", "outcome=\"timeout\""]
        ),
        Some(1.0),
        "the timed-out dispatch must count one timeout call, got:\n{body}"
    );
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_call_duration_seconds_count",
            &["plugin=\"metrics-timeout-a\""],
        ),
        Some(1.0),
        "the timed-out dispatch still awaited the deadline, got:\n{body}"
    );
}

#[test]
fn cancelling_plugin_counts_cancelled_not_ok() {
    let addr = ensure_installed();
    let fx = Fixture::new();
    let plugins = load_plugins(
        &fx,
        &[(
            "metrics-cancel-a",
            common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"nope\"}"),
        )],
    );
    let set = PluginSet::from_vec(plugins);
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());
    assert!(result.cancelled);

    let body = scrape(addr);
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_calls_total",
            &["plugin=\"metrics-cancel-a\"", "outcome=\"cancelled\""]
        ),
        Some(1.0),
        "the cancelling dispatch must count one cancelled call, got:\n{body}"
    );
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_calls_total",
            &["plugin=\"metrics-cancel-a\"", "outcome=\"ok\""]
        ),
        None,
        "a cancelled call must not also count as ok, got:\n{body}"
    );
}

#[test]
fn undecodable_answer_counts_failed() {
    let addr = ensure_installed();
    let fx = Fixture::new();
    let plugins = load_plugins(
        &fx,
        &[(
            "metrics-broken-a",
            common::response_plugin_wasm("this is definitely not json"),
        )],
    );
    let set = PluginSet::from_vec(plugins);
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());
    assert!(!result.cancelled);

    let body = scrape(addr);
    assert_eq!(
        series_value(
            &body,
            "candyriya_plugin_calls_total",
            &["plugin=\"metrics-broken-a\"", "outcome=\"failed\""]
        ),
        Some(1.0),
        "an undecodable answer must count one failed call, got:\n{body}"
    );
}

#[test]
fn full_mailbox_counts_busy_not_calls() {
    let addr = ensure_installed();
    let fx = Fixture::new();
    let plugins = load_plugins(&fx, &[("metrics-busy-a", common::slow_plugin_wasm())]);
    let instance = plugins
        .first()
        .and_then(LibraryPlugin::instance)
        .expect("the slow plugin must have an instance");

    // the worker takes one call and never returns; the mailbox holds 16;
    // 24 callers at once must see at least a few mailbox-full rejections
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..24 {
            handles.push(
                scope.spawn(|| {
                    instance.call_with_timeout("on_event", b"", Duration::from_millis(50))
                }),
            );
        }
        for handle in handles {
            let _ = handle.join();
        }
    });

    let body = scrape(addr);
    let busy = series_value(
        &body,
        "candyriya_plugin_busy_total",
        &["plugin=\"metrics-busy-a\""],
    );
    assert!(
        busy.is_some_and(|v| v >= 1.0),
        "a full mailbox must count at least one busy rejection, got:\n{body}"
    );
}
