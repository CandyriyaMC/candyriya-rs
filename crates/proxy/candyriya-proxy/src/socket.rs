//! Listener socket construction via `socket2`.
//!
//! A Minecraft Java proxy is a TCP listener first and a protocol
//! implementation second, so the socket is set up deliberately rather
//! than by leaning on `TcpListener::bind`:
//!
//! - `SO_REUSEADDR` — set on Unix and Windows so the port can be
//!   rebound promptly after a restart (on Windows this also allows a
//!   socket to bind past a `TIME_WAIT` state, matching what operators
//!   expect when restarting a proxy quickly).
//! - `SO_REUSEPORT` — opt-in via config; honored on Linux/macOS,
//!   skipped with a warning everywhere else (see [`REUSE_PORT_SUPPORTED`]).
//! - `SO_KEEPALIVE` — always on; dead peers are detected at the OS
//!   layer instead of hanging a connection task forever.
//! - `TCP_NODELAY` — always on. Minecraft is a small-packet,
//!   latency-sensitive protocol; buffering writes behind Nagle's
//!   algorithm is pure harm here.
//! - `TCP_FASTOPEN` — config-gated but *experimental*: it is a Linux
//!   option that must be toggled with `setsockopt(2)`, which has no
//!   safe API in `socket2`/`tokio`/`std`. This crate is forbidden from
//!   using `unsafe`, so the option is accepted for forward
//!   compatibility and skipped with a warning. Setting it would need a
//!   vetted safe wrapper or a dedicated, justified `unsafe` site.
//!
//! The bound socket is wrapped into a non-blocking
//! [`tokio::net::TcpListener`] ready for the accept loop.

use std::net::SocketAddr;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::TcpListener;
use tracing::warn;

use crate::ProxyError;

/// Default listen backlog passed to `listen(2)`.
///
/// 1024 matches what most socket options on Linux document as their
/// effective `SOMAXCONN` ceiling, so the value is meaningful everywhere
/// and harmless where the kernel clamps it lower.
pub const DEFAULT_BACKLOG: u32 = 1024;

/// Whether this platform supports `SO_REUSEPORT`.
///
/// `true` on Linux and macOS (and other Unix targets where `socket2`
/// exposes it), `false` on Windows, where the option does not exist.
/// When a config asks for it on an unsupported platform, [`ProxySocket::bind`]
/// logs a warning and leaves it off instead of failing the bind.
#[cfg(all(
    unix,
    not(any(target_os = "solaris", target_os = "nuttx", target_os = "wasi"))
))]
pub const REUSE_PORT_SUPPORTED: bool = true;

/// Whether this platform supports `SO_REUSEPORT`. See the Unix definition.
#[cfg(not(all(
    unix,
    not(any(target_os = "solaris", target_os = "nuttx", target_os = "wasi"))
)))]
pub const REUSE_PORT_SUPPORTED: bool = false;

/// Socket-layer options for the proxy listener.
///
/// This is the socket-scoped view of the `[advanced]` config section
/// (the connection-count knobs live in the accept loop, not here).
/// Build it from [`candyriya_config::AdvancedConfig`] with
/// `SocketConfig::from(&config.advanced)` or construct it directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketConfig {
    /// Backlog passed to `listen(2)`. Values above the platform's
    /// `c_int` range are clamped to `i32::MAX` (the kernel clamps
    /// further anyway).
    pub backlog: u32,
    /// `SO_REUSEPORT`: let the kernel load-balance inbound connections
    /// across listener threads. Linux/macOS only; on other platforms
    /// the option is skipped with a warning. Defaults to `false`.
    pub enable_reuse_port: bool,
    /// `TCP_FASTOPEN` (Linux, experimental): start the handshake data
    /// in the opening SYN. Requesting it here logs a warning because
    /// enabling it requires a `setsockopt` call that has no safe API
    /// in this dependency stack; see the module docs. Defaults to
    /// `false`.
    pub tcp_fast_open: bool,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            backlog: DEFAULT_BACKLOG,
            enable_reuse_port: false,
            tcp_fast_open: false,
        }
    }
}

impl From<&candyriya_config::AdvancedConfig> for SocketConfig {
    fn from(advanced: &candyriya_config::AdvancedConfig) -> Self {
        Self {
            backlog: DEFAULT_BACKLOG,
            enable_reuse_port: advanced.enable_reuse_port,
            tcp_fast_open: advanced.tcp_fast_open,
        }
    }
}

/// A bound, listening TCP socket configured for the proxy.
///
/// Construction is [`ProxySocket::bind`]; the accept loop consumes the
/// socket with [`ProxySocket::into_listener`].
pub struct ProxySocket {
    listener: TcpListener,
    local_addr: SocketAddr,
    reuse_port_applied: bool,
}

impl core::fmt::Debug for ProxySocket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProxySocket")
            .field("local_addr", &self.local_addr)
            .field("reuse_port_applied", &self.reuse_port_applied)
            .finish_non_exhaustive()
    }
}

impl ProxySocket {
    /// Create, configure, bind and listen on `addr`.
    ///
    /// Options are applied in the order a cautious listener needs:
    /// `SO_REUSEADDR` and `SO_REUSEPORT` before `bind`, and
    /// `SO_KEEPALIVE`/`TCP_NODELAY` on the resulting socket. The socket is
    /// put into non-blocking mode and handed to tokio as a
    /// [`TcpListener`].
    ///
    /// Must be called from within a tokio runtime context: installing the
    /// listener requires a reactor (`tokio::net::TcpListener::from_std`).
    ///
    /// # Errors
    ///
    /// Returns [`ProxyError`] when the socket cannot be created, an option
    /// fails to be set, the bind fails, `listen(2)` fails, or the listener
    /// cannot be installed as a non-blocking tokio listener. An
    /// `SO_REUSEPORT` request on an unsupported platform is **not** an
    /// error — it is logged and skipped.
    pub fn bind(addr: SocketAddr, config: &SocketConfig) -> Result<Self, ProxyError> {
        let domain = if addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
            .map_err(ProxyError::SocketCreate)?;

        socket
            .set_reuse_address(true)
            .map_err(|source| ProxyError::SetOption {
                option: "SO_REUSEADDR",
                source,
            })?;

        let mut reuse_port_applied = false;
        if config.enable_reuse_port {
            if REUSE_PORT_SUPPORTED {
                #[cfg(all(
                    unix,
                    not(any(target_os = "solaris", target_os = "nuttx", target_os = "wasi"))
                ))]
                set_reuse_port(&socket)?;
                reuse_port_applied = true;
            } else {
                warn!(
                    %addr,
                    "enable-reuse-port is set in the config but SO_REUSEPORT does not exist on this platform; continuing without it"
                );
            }
        }

        if config.tcp_fast_open {
            warn!(
                %addr,
                "tcp-fast-open is set in the config but enabling it requires a setsockopt call with no safe API in this stack; continuing without it"
            );
        }

        socket
            .set_keepalive(true)
            .map_err(|source| ProxyError::SetOption {
                option: "SO_KEEPALIVE",
                source,
            })?;
        // Server-side sockets must not buffer behind Nagle; the option is
        // per-socket and NOT inherited from the listener on all platforms,
        // so the accept loop sets it again on every accepted stream.
        socket
            .set_tcp_nodelay(true)
            .map_err(|source| ProxyError::SetOption {
                option: "TCP_NODELAY",
                source,
            })?;
        // Tokio's from_std conversion requires a non-blocking socket.
        socket
            .set_nonblocking(true)
            .map_err(|source| ProxyError::SetOption {
                option: "O_NONBLOCK",
                source,
            })?;

        socket
            .bind(&SockAddr::from(addr))
            .map_err(|source| ProxyError::Bind { addr, source })?;

        // listen(2) takes a c_int. Clamp rather than truncate on absurd
        // backlogs; the kernel clamps to its own ceiling regardless.
        let backlog = core::ffi::c_int::try_from(config.backlog).unwrap_or(core::ffi::c_int::MAX);
        socket.listen(backlog).map_err(ProxyError::Listen)?;

        let std_listener: std::net::TcpListener = socket.into();
        let listener =
            TcpListener::from_std(std_listener).map_err(ProxyError::ListenerConversion)?;
        let local_addr = listener.local_addr().map_err(ProxyError::LocalAddr)?;

        Ok(Self {
            listener,
            local_addr,
            reuse_port_applied,
        })
    }

    /// The bound local address, with the real port for ephemeral binds.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether `SO_REUSEPORT` was actually applied.
    ///
    /// `false` when the config did not ask for it, or when it did but
    /// the platform does not support the option (logged at bind time).
    pub fn reuse_port_applied(&self) -> bool {
        self.reuse_port_applied
    }

    /// Consume the wrapper and return the tokio listener for the accept
    /// loop.
    pub fn into_listener(self) -> TcpListener {
        self.listener
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "solaris", target_os = "nuttx", target_os = "wasi"))
))]
fn set_reuse_port(socket: &Socket) -> Result<(), ProxyError> {
    socket
        .set_reuse_port(true)
        .map_err(|source| ProxyError::SetOption {
            option: "SO_REUSEPORT",
            source,
        })
}
