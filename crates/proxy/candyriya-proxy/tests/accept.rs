//! Integration tests for the accept-loop slice of `candyriya-proxy`.
//!
//! All tests run on loopback only, no external network is touched, and
//! no test may panic (assert-failures panic by construction; the code
//! under test must not).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

use candyriya_config::Config;
use candyriya_scheduler::TaskService;

use candyriya_proxy::{ProxySocket, ProxyState, REUSE_PORT_SUPPORTED, SocketConfig, serve};

/// Loopback ephemeral bind address.
fn loopback_any() -> SocketAddr {
    "127.0.0.1:0".parse().expect("valid literal")
}

fn test_service() -> TaskService {
    TaskService::builder()
        .blocking_threads(2)
        .build()
        .expect("task service must build")
}

fn test_config() -> Arc<Config> {
    Arc::new(Config::default())
}

/// Poll `condition` until it holds or `timeout` elapses.
async fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return condition();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn bind_on_ephemeral_loopback_port_succeeds() {
    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default())
        .expect("bind on 127.0.0.1:0 must succeed");
    assert_ne!(
        socket.local_addr().port(),
        0,
        "ephemeral port must be assigned"
    );
    assert!(!socket.reuse_port_applied(), "reuse-port was not requested");
}

#[tokio::test]
async fn reuse_port_is_applied_where_supported_and_skipped_elsewhere() {
    let config = SocketConfig {
        enable_reuse_port: true,
        ..SocketConfig::default()
    };
    let socket = ProxySocket::bind(loopback_any(), &config)
        .expect("bind with reuse-port requested must still succeed");
    assert_eq!(
        socket.reuse_port_applied(),
        REUSE_PORT_SUPPORTED,
        "reuse-port outcome must match platform support"
    );
}

#[tokio::test]
async fn tcp_fast_open_request_does_not_fail_the_bind() {
    // enabling TCP_FASTOPEN has no safe API on this stack; the socket
    // must bind anyway and skip the option with a warning
    let config = SocketConfig {
        tcp_fast_open: true,
        ..SocketConfig::default()
    };
    let socket = ProxySocket::bind(loopback_any(), &config)
        .expect("bind with tcp-fast-open requested must succeed");
    assert_ne!(socket.local_addr().port(), 0);
}

#[tokio::test]
async fn per_ip_cap_closes_the_over_cap_connection() {
    let state = Arc::new(ProxyState::with_limits(test_config(), 1));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    // first connection occupies the single per-IP slot
    let mut first = TcpStream::connect(addr).await.expect("first connect");
    let held = wait_until(
        || state.limiter.current_connections() == 1,
        Duration::from_secs(2),
    )
    .await;
    assert!(held, "first connection must acquire the slot");

    // second connection from the same IP must be closed immediately
    let mut second = TcpStream::connect(addr).await.expect("second connect");
    let mut buf = [0_u8; 16];
    let result = tokio::time::timeout(Duration::from_secs(2), second.read(&mut buf)).await;
    match result {
        Ok(Ok(0)) => {}  // clean EOF: proxy closed the connection
        Ok(Err(_)) => {} // reset also counts as closed
        other => panic!("over-cap connection must be closed by the proxy, got {other:?}"),
    }
    assert_eq!(state.limiter.per_ip_connections(addr.ip()), 1);
    assert_eq!(state.limiter.current_connections(), 1);

    // the first connection is still usable and drains to EOF on close
    first
        .write_all(b"hello")
        .await
        .expect("write to first connection");
    drop(first);
    let drained = wait_until(
        || state.limiter.current_connections() == 0,
        Duration::from_secs(2),
    )
    .await;
    assert!(drained, "closing the peer must release the slot");

    shutdown_tx.send(true).expect("shutdown flip must work");
    let stopped = wait_until(|| service.stats().active == 0, Duration::from_secs(2)).await;
    assert!(stopped, "accept loop must stop on the shutdown signal");
    let report = service.drain(Duration::from_secs(2)).await;
    assert_eq!(report.timed_out, 0);
}

#[tokio::test]
async fn global_semaphore_caps_concurrent_connections() {
    let mut config = Config::default();
    config.advanced.max_connections = 1;
    let config = Arc::new(config);
    let state = Arc::new(ProxyState::new(config));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut first = TcpStream::connect(addr).await.expect("first connect");
    let held = wait_until(
        || state.limiter.current_connections() == 1,
        Duration::from_secs(2),
    )
    .await;
    assert!(held, "first connection must acquire the slot");

    let mut second = TcpStream::connect(addr).await.expect("second connect");
    let mut buf = [0_u8; 16];
    let result = tokio::time::timeout(Duration::from_secs(2), second.read(&mut buf)).await;
    match result {
        Ok(Ok(0)) => {}
        Ok(Err(_)) => {} // reset also counts as closed
        other => panic!("globally-capped connection must be closed by the proxy, got {other:?}"),
    }
    // the limiter slot for the rejected connection must have been
    // released when the semaphore rejected it: only the first holds one
    assert_eq!(state.limiter.per_ip_connections(addr.ip()), 1);

    first.write_all(b"x").await.expect("write");
    drop(first);
    let drained = wait_until(
        || state.limiter.current_connections() == 0,
        Duration::from_secs(2),
    )
    .await;
    assert!(drained, "closing the peer must release both slots");

    shutdown_tx.send(true).expect("shutdown flip must work");
    let stopped = wait_until(|| service.stats().active == 0, Duration::from_secs(2)).await;
    assert!(stopped, "accept loop must stop on the shutdown signal");
    let report = service.drain(Duration::from_secs(2)).await;
    assert_eq!(report.timed_out, 0);
}

#[tokio::test]
async fn accept_loop_drains_n_clients_and_releases_slots() {
    let state = Arc::new(ProxyState::new(test_config()));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    // five clients: connect, send a few bytes, wait for the slot, close
    for _ in 0..5 {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(b"ping").await.expect("write");
        stream.shutdown().await.expect("shutdown write");
        // dropping stream after shutdown closes the read side too
        drop(stream);
    }

    let drained = wait_until(
        || state.limiter.current_connections() == 0,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        drained,
        "all client slots must be released after peer close"
    );

    shutdown_tx.send(true).expect("shutdown flip must work");
    let stopped = wait_until(|| service.stats().active == 0, Duration::from_secs(2)).await;
    assert!(stopped, "accept loop must stop on the shutdown signal");
    let report = service.drain(Duration::from_secs(2)).await;
    assert_eq!(report.timed_out, 0);
}

#[tokio::test]
async fn accept_loop_terminates_on_shutdown_watch() {
    let state = Arc::new(ProxyState::new(test_config()));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    // the proxy is serving: a client can connect
    let _probe = TcpStream::connect(addr)
        .await
        .expect("connect before shutdown");
    drop(_probe);

    shutdown_tx
        .send(true)
        .expect("shutdown watch must accept true");

    let stopped = wait_until(|| service.stats().active == 0, Duration::from_secs(2)).await;
    assert!(stopped, "accept loop must stop after the shutdown signal");

    let report = service.drain(Duration::from_secs(2)).await;
    assert_eq!(report.timed_out, 0);
}

#[tokio::test]
async fn held_open_connection_survives_accept_loop_shutdown() {
    // shutdown stops accepting; an already-accepted connection keeps
    // running in its own task until the peer closes
    let state = Arc::new(ProxyState::new(test_config()));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let client = TcpStream::connect(addr).await.expect("connect");
    let held = wait_until(
        || state.limiter.current_connections() == 1,
        Duration::from_secs(2),
    )
    .await;
    assert!(held, "connection must be accepted before shutdown");

    shutdown_tx.send(true).expect("shutdown flip must work");

    // peer closes -> the connection task drains to EOF and releases the slot
    drop(client);
    let drained = wait_until(
        || state.limiter.current_connections() == 0,
        Duration::from_secs(2),
    )
    .await;
    assert!(drained, "peer close must release the slot");

    let stopped = wait_until(|| service.stats().active == 0, Duration::from_secs(2)).await;
    assert!(
        stopped,
        "all tasks must finish after shutdown and peer close"
    );

    let report = service.drain(Duration::from_secs(2)).await;
    assert_eq!(report.timed_out, 0);
}

#[tokio::test]
async fn handshake_style_bytes_flow_through_and_drain_completes() {
    // drain semantics: the proxy accepts a client that speaks first
    // (like a minecraft handshake) and tears the connection down cleanly
    let state = Arc::new(ProxyState::new(test_config()));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    // a minecraft-handshake-ish burst: length prefix + payload
    let mut bytes = vec![0x0a_u8, 0x00];
    bytes.extend_from_slice(&[0x00; 20]);
    stream
        .write_all(&bytes)
        .await
        .expect("write handshake bytes");
    stream.shutdown().await.expect("shutdown write");

    let drained = wait_until(
        || state.limiter.current_connections() == 0,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        drained,
        "connection must be fully drained after the peer closes"
    );

    shutdown_tx.send(true).expect("shutdown flip must work");
    let stopped = wait_until(|| service.stats().active == 0, Duration::from_secs(2)).await;
    assert!(stopped, "accept loop must stop on the shutdown signal");
    let report = service.drain(Duration::from_secs(2)).await;
    assert_eq!(report.timed_out, 0);
}

#[test]
fn config_advanced_maps_into_socket_config() {
    let advanced = candyriya_config::AdvancedConfig {
        enable_reuse_port: true,
        tcp_fast_open: true,
        ..candyriya_config::AdvancedConfig::default()
    };
    let config = candyriya_proxy::SocketConfig::from(&advanced);
    assert!(config.enable_reuse_port);
    assert!(config.tcp_fast_open);
}
