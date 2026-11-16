//! Integration tests for `candyriya-metrics`.
//!
//! The global recorder can only be installed once per process, and these two
//! tests live in one process. `disabled_install_is_ok_and_repeatable` never
//! installs a recorder, so it cannot race
//! `enabled_install_serves_metrics_and_is_one_shot`, which consumes the
//! one-shot slot. Do not add more enabled-install tests to this binary —
//! put them in their own integration test file if you ever need them.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use candyriya_metrics::{MetricsError, install, names};

/// Grabs an ephemeral port by binding to `:0` and releasing it.
///
/// There is an inherent race here: the port may be taken by someone else
/// between this call and the exporter's bind. That is a documented, accepted
/// trade-off of testing a listener without a fixed port; if it ever bites,
/// rerun the test.
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

#[test]
fn disabled_install_is_ok_and_repeatable() {
    let first = install(None);
    assert!(first.is_ok(), "disabled install must succeed");
    let second = install(None);
    assert!(
        second.is_ok(),
        "disabled install must not consume the one-shot slot"
    );

    let first = first.expect("checked above");
    let second = second.expect("checked above");
    assert!(!first.is_prometheus_enabled());
    assert!(!second.is_prometheus_enabled());

    first.shutdown();
    drop(second);
}

#[test]
fn enabled_install_serves_metrics_and_is_one_shot() {
    let port = free_port();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    let handle = install(Some(addr)).expect("first enabled install must succeed");
    assert!(handle.is_prometheus_enabled());

    // the facade increments land in the exporter's registry
    metrics::counter!(names::NAME_CONNECTIONS_ACCEPTED_TOTAL).increment(1);

    let body = get_metrics_body(addr);
    assert!(
        body.contains(names::NAME_CONNECTIONS_ACCEPTED_TOTAL),
        "scrape body must contain the const name, got:\n{body}"
    );
    assert!(
        body.contains("service=\"candyriya\""),
        "scrape body must carry the global service label, got:\n{body}"
    );

    // a second enabled install is one-shot and must report AlreadyInstalled,
    // even if the address differs (the flag is checked before any bind)
    let other = std::net::SocketAddr::from(([127, 0, 0, 1], free_port()));
    let second = install(Some(other));
    assert!(
        matches!(second, Err(MetricsError::AlreadyInstalled)),
        "second enabled install must fail with AlreadyInstalled, got: {second:?}"
    );

    handle.shutdown();
}

/// Polls the scrape endpoint until the exporter thread serves it.
///
/// The bind happens synchronously inside `install`, so the socket is already
/// listening and a connect essentially cannot fail — but the serve loop lives
/// on another thread whose startup is not sequenced against our writes, so we
/// retry the whole request a few times with a read timeout instead of
/// assuming.
fn get_metrics_body(addr: std::net::SocketAddr) -> String {
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

fn fetch(addr: std::net::SocketAddr) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let raw = String::from_utf8_lossy(&raw);
    // split the response at the header/body boundary
    Ok(raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string())
}
