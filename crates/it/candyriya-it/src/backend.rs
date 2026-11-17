//! A real, loopback Minecraft server-side TCP backend.
//!
//! This is not a mock: it is an actual server that accepts TCP connections,
//! completes the handshaking dance and answers status and login traffic with
//! byte-exact packets produced through `candyriya-protocol`. It is bound to
//! loopback on an ephemeral port and never touches the network beyond that.
//!
//! The wire-behavior contract and the fixture lifecycle are documented on
//! [`BackendFixture`].

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use candyriya_protocol::{
    Handshake, Intent, Packet, parse_login_start_name, write_string, write_uuid, write_varint,
};
use candyriya_protocol_registry::{ConnectionState, Direction, PacketKind, PacketSlot, Registry};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::json::push_json_string;
use crate::uuid::uuid_from_name;
use crate::{HarnessError, protocol_id, resolve_version, wire};

/// Seed for a fake backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeBackendConfig {
    /// The protocol number the backend advertises and uses to resolve packet
    /// ids. Not validated against any list; arbitrary values (including
    /// negative ones) are accepted and echoed.
    pub protocol: i32,
    /// The MOTD served in the status response. Any UTF-8 string is accepted;
    /// it is escaped into the JSON document on emission.
    pub motd: String,
    /// The `max` player count advertised in the status response.
    pub max_players: u16,
}

/// 1x1 transparent PNG, base64. The status response embeds this as favicon;
/// [`FakeBackendConfig`] deliberately carries no favicon field, so this is
/// the one and only favicon the backend ever serves.
const ONE_PX_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

/// A point-in-time copy of the backend's counters.
///
/// Returned by [`BackendFixture::stats`] and [`BackendFixture::close`]; all
/// values are monotonic since the fixture started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackendStatsSnapshot {
    /// TCP connections accepted so far.
    pub connections_accepted: u64,
    /// Valid handshakes processed so far.
    pub handshakes: u64,
    /// Status-request packets answered so far.
    pub status_requests: u64,
    /// Status-ping packets echoed so far.
    pub pings: u64,
    /// Login-start packets answered with login success so far.
    pub logins: u64,
    /// Frames received in the play state and ignored (never replied to).
    pub discarded_frames: u64,
    /// Protocol-level problems: undecodable frames, unexpected packet ids,
    /// malformed handshakes, login starts that fail to parse.
    pub protocol_errors: u64,
    /// Packets that could not be written back (peer went away mid-response).
    pub write_errors: u64,
    /// Connections whose read loop ended naturally (EOF or rejected frame).
    /// Connections torn down by [`BackendFixture::close`]/`Drop` may not
    /// have been accounted for yet.
    pub connections_closed: u64,
}

#[derive(Default)]
struct BackendStats {
    connections_accepted: AtomicU64,
    handshakes: AtomicU64,
    status_requests: AtomicU64,
    pings: AtomicU64,
    logins: AtomicU64,
    discarded_frames: AtomicU64,
    protocol_errors: AtomicU64,
    write_errors: AtomicU64,
    connections_closed: AtomicU64,
}

impl BackendStats {
    fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> BackendStatsSnapshot {
        BackendStatsSnapshot {
            connections_accepted: self.connections_accepted.load(Ordering::Relaxed),
            handshakes: self.handshakes.load(Ordering::Relaxed),
            status_requests: self.status_requests.load(Ordering::Relaxed),
            pings: self.pings.load(Ordering::Relaxed),
            logins: self.logins.load(Ordering::Relaxed),
            discarded_frames: self.discarded_frames.load(Ordering::Relaxed),
            protocol_errors: self.protocol_errors.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
            connections_closed: self.connections_closed.load(Ordering::Relaxed),
        }
    }
}

struct BackendInner {
    config: FakeBackendConfig,
    stats: BackendStats,
    /// Handles of in-flight connection tasks. Used by `close()`/`Drop` to
    /// stop stragglers; finished handles are pruned on every accept.
    connections: Mutex<Vec<JoinHandle<()>>>,
    shutdown: watch::Receiver<bool>,
}

/// An ephemeral loopback Minecraft backend bound to `127.0.0.1:0`.
///
/// ## Wire behavior, exactly as implemented
///
/// * **Handshake.** A packet with id `0x00` whose payload decodes as a
///   [`Handshake`] is required; anything else closes the connection.
/// * **Intent.** `Intent::Status` switches to the status state, `Intent::Login`
///   and `Intent::Transfer` switch to the login state. Any other intent value
///   closes the connection without a response (no packet is written first).
/// * **Unknown protocol numbers are accepted.** The received protocol number
///   is never validated. Packet ids for status and login are resolved from
///   `candyriya-protocol-registry`; when the client's protocol is not in the
///   embedded table, the ids of the newest known release are used instead
///   (see `protocol_id` in the crate internals). The status response echoes
///   the client's protocol verbatim, with the release name resolved from the
///   registry when known, otherwise `protocol <n>`.
/// * **Status state.** `status request` is answered with a status response
///   `{"version":{"name":<n>,"protocol":p},"players":{"max":<m>,"online":0},
///   "description":"<motd>","favicon":"data:image/png;base64,..."}` (the MOTD
///   is escaped; the favicon is the embedded 1x1 transparent PNG).
///   `status ping` is echoed as `status pong` with the identical 8-byte
///   payload. Any other packet id in the status state closes the connection.
/// * **Login state.** The first packet must be a login start, answered with
///   a login success packet. The body is the current layout — a Game Profile
///   (UUID, username, zero properties) followed by a session id UUID; pre-26.2
///   clients read exactly the UUID and username and tolerate the remainder,
///   which is why one body is emitted for all protocol versions. The packet id
///   is resolved per protocol version from the registry. Compression and
///   encryption are never negotiated and no set-compression packet is sent;
///   framing stays in the uncompressed format for the whole connection.
/// * **Play state.** After login success the connection is held open until
///   the client drops it. Play-state frames are read and discarded; the
///   backend never replies in this state.
/// * **Close.** When the client disconnects, or a malformed packet arrives,
///   the read loop ends, the socket is dropped and the task finishes.
///
/// ## Lifecycle
///
/// [`BackendFixture::start`] binds `127.0.0.1:0` and reads back the assigned
/// port. [`BackendFixture::close`] signals shutdown, waits for the accept
/// loop, aborts and drains any still-running connection tasks and returns a
/// [`BackendStatsSnapshot`]. The [`Drop`](BackendFixture) implementation performs
/// the same steps synchronously (best effort, no waiting), so a fixture dropped
/// without `close()` never leaves an accept loop or a held socket behind.
/// Connection tasks are aborted at their next await point, which drops their
/// sockets; the client observes EOF shortly after teardown. Tests that need
/// to know *why* a connection died inspect [`BackendStatsSnapshot`] — every
/// rejection path increments exactly one counter.
pub struct BackendFixture {
    addr: std::net::SocketAddr,
    inner: Arc<BackendInner>,
    shutdown: watch::Sender<bool>,
    accept: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for BackendFixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendFixture")
            .field("addr", &self.addr)
            .finish()
    }
}

impl BackendFixture {
    /// Bind the backend to an ephemeral loopback port and start accepting.
    pub async fn start(config: FakeBackendConfig) -> Result<Self, HarnessError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let inner = Arc::new(BackendInner {
            config,
            stats: BackendStats::default(),
            connections: Mutex::new(Vec::new()),
            shutdown: shutdown_rx,
        });
        let accept = tokio::spawn(accept_loop(listener, Arc::clone(&inner)));
        Ok(Self {
            addr,
            inner,
            shutdown,
            accept: Some(accept),
        })
    }

    /// The address the backend is bound to.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// The configuration the backend was seeded with.
    pub fn config(&self) -> &FakeBackendConfig {
        &self.inner.config
    }

    /// A point-in-time copy of the backend counters.
    pub fn stats(&self) -> BackendStatsSnapshot {
        self.inner.stats.snapshot()
    }

    /// Shut the backend down deterministically.
    ///
    /// Sends the shutdown signal, waits for the accept loop to exit, then
    /// aborts and drains any still-running connection tasks. After this
    /// returns, no backend task is running and every connection socket has
    /// been dropped — clients observe EOF. Returns the final counters.
    pub async fn close(mut self) -> BackendStatsSnapshot {
        // signal first so the accept loop can break out of select! immediately
        let _ = self.shutdown.send(true);
        if let Some(accept) = self.accept.take() {
            // the accept loop selects on the shutdown signal; it exits promptly
            let _ = accept.await;
        }

        let handles: Vec<JoinHandle<()>> = match self.inner.connections.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => Vec::new(),
        };
        for handle in &handles {
            handle.abort();
        }
        for handle in handles {
            // abort() is terminal; this awaits the task's actual teardown
            let _ = handle.await;
        }
        self.inner.stats.snapshot()
    }
}

impl Drop for BackendFixture {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(accept) = self.accept.take() {
            accept.abort();
        }
        if let Ok(mut guard) = self.inner.connections.lock() {
            for handle in guard.drain(..) {
                handle.abort();
            }
        }
        // aborted tasks drop their sockets as the runtime unwinds them,
        // so clients observe EOF without waiting for anything here
    }
}

async fn accept_loop(listener: TcpListener, inner: Arc<BackendInner>) {
    let mut shutdown = inner.shutdown.clone();
    loop {
        let accepted = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _peer)) => {
                BackendStats::incr(&inner.stats.connections_accepted);
                let task_inner = Arc::clone(&inner);
                let handle = tokio::spawn(handle_connection(stream, task_inner));
                if let Ok(mut guard) = inner.connections.lock() {
                    // prune handles of connections that already finished
                    guard.retain(|h| !h.is_finished());
                    guard.push(handle);
                }
            }
            Err(_) => {
                // loopback accepts don't transiently fail in practice; if one
                // ever does, keep serving the rest instead of dying silently
            }
        }
    }
}

/// Build the status-response JSON document for the configured seed.
fn build_status_response(config: &FakeBackendConfig, registry: &Registry) -> String {
    let version_name = match registry.from_protocol(config.protocol) {
        Some(version) => version.name.to_owned(),
        None => format!("protocol {}", config.protocol),
    };

    let mut out = String::with_capacity(192 + config.motd.len());
    out.push_str("{\"version\":{\"name\":");
    push_json_string(&mut out, &version_name);
    out.push_str(",\"protocol\":");
    out.push_str(&config.protocol.to_string());
    out.push_str("},\"players\":{\"max\":");
    out.push_str(&config.max_players.to_string());
    out.push_str(",\"online\":0},\"description\":");
    push_json_string(&mut out, &config.motd);
    out.push_str(",\"favicon\":\"data:image/png;base64,");
    out.push_str(ONE_PX_PNG_BASE64);
    out.push_str("\"}");
    out
}

async fn handle_connection(stream: TcpStream, inner: Arc<BackendInner>) {
    let _ = stream.set_nodelay(true);
    let mut stream = stream;
    let mut buf = BytesMut::with_capacity(4096);

    // the registry parse is per-connection on purpose: it keeps BackendInner
    // free of a non-clonable Registry; the embedded snapshot is small
    let registry = match Registry::embedded() {
        Ok(registry) => registry,
        Err(_) => {
            BackendStats::incr(&inner.stats.protocol_errors);
            BackendStats::incr(&inner.stats.connections_closed);
            return;
        }
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Phase {
        Handshaking,
        Status,
        Login,
        Play,
    }

    let mut phase = Phase::Handshaking;
    let mut protocol = 0;
    let mut version = None;

    loop {
        let packet = match wire::read_packet(&mut stream, &mut buf).await {
            Ok(Some(packet)) => packet,
            Ok(None) => break, // clean EOF: client went away
            Err(_) => {
                BackendStats::incr(&inner.stats.protocol_errors);
                break;
            }
        };

        match phase {
            Phase::Handshaking => {
                if packet.id != Handshake::PACKET_ID {
                    BackendStats::incr(&inner.stats.protocol_errors);
                    break;
                }
                let handshake = match Handshake::decode_payload(&mut packet.payload.as_ref()) {
                    Ok(handshake) => handshake,
                    Err(_) => {
                        BackendStats::incr(&inner.stats.protocol_errors);
                        break;
                    }
                };
                BackendStats::incr(&inner.stats.handshakes);
                protocol = handshake.protocol;
                version = Some(resolve_version(&registry, protocol));
                phase = match handshake.intent {
                    Intent::Status => Phase::Status,
                    // transfer intent enters the login state, mirroring 1.20.5+
                    Intent::Login | Intent::Transfer => Phase::Login,
                    // documented: unknown intents are refused, no response is sent
                    Intent::Unknown(_) => {
                        BackendStats::incr(&inner.stats.protocol_errors);
                        break;
                    }
                };
            }
            Phase::Status => {
                let Some(version) = version else { break };
                let kind = registry.classify(
                    ConnectionState::Status,
                    Direction::Serverbound,
                    version,
                    packet.id,
                );
                match kind {
                    PacketKind::StatusRequest => {
                        let Some(response_id) =
                            protocol_id(&registry, protocol, PacketSlot::StatusClientboundResponse)
                                .ok()
                        else {
                            BackendStats::incr(&inner.stats.protocol_errors);
                            break;
                        };
                        let body = build_status_response(&inner.config, &registry);
                        let packet = Packet {
                            id: response_id,
                            payload: Bytes::from(body.into_bytes()),
                        };
                        if wire::write_packet(&mut stream, packet).await.is_err() {
                            BackendStats::incr(&inner.stats.write_errors);
                            break;
                        }
                        BackendStats::incr(&inner.stats.status_requests);
                    }
                    PacketKind::StatusPing => {
                        let Some(pong_id) =
                            protocol_id(&registry, protocol, PacketSlot::StatusClientboundPong)
                                .ok()
                        else {
                            BackendStats::incr(&inner.stats.protocol_errors);
                            break;
                        };
                        if packet.payload.len() != 8 {
                            BackendStats::incr(&inner.stats.protocol_errors);
                            break;
                        }
                        let packet = Packet {
                            id: pong_id,
                            payload: packet.payload.clone(),
                        };
                        if wire::write_packet(&mut stream, packet).await.is_err() {
                            BackendStats::incr(&inner.stats.write_errors);
                            break;
                        }
                        BackendStats::incr(&inner.stats.pings);
                    }
                    _ => {
                        BackendStats::incr(&inner.stats.protocol_errors);
                        break;
                    }
                }
            }
            Phase::Login => {
                let Some(version) = version else { break };
                let kind = registry.classify(
                    ConnectionState::Login,
                    Direction::Serverbound,
                    version,
                    packet.id,
                );
                if kind != PacketKind::LoginStart {
                    BackendStats::incr(&inner.stats.protocol_errors);
                    break;
                }
                // parse_login_start_name wants the whole frame body
                // (packet id + username + trailing); PacketCodec splits the
                // id off on decode, so rebuild the body for the partial parser
                let mut frame_body = BytesMut::with_capacity(packet.payload.len() + 5);
                write_varint(packet.id, &mut frame_body);
                frame_body.extend_from_slice(&packet.payload);
                let name = match parse_login_start_name(&frame_body) {
                    Ok(name) => name,
                    Err(_) => {
                        BackendStats::incr(&inner.stats.protocol_errors);
                        break;
                    }
                };
                let Some(success_id) = protocol_id(
                    &registry,
                    protocol,
                    PacketSlot::LoginClientboundLoginSuccess,
                )
                .ok() else {
                    BackendStats::incr(&inner.stats.protocol_errors);
                    break;
                };

                // current login success body: Game Profile (uuid, username,
                // zero properties) + session id uuid. Older clients read
                // exactly uuid+username and ignore the remainder; the frame
                // length delimits the rest of the stream.
                let (uuid_msb, uuid_lsb) = uuid_from_name(&name);
                let (session_msb, session_lsb) = uuid_from_name(&format!("session:{name}"));
                let mut payload = BytesMut::with_capacity(34 + name.len());
                write_uuid(uuid_msb, uuid_lsb, &mut payload);
                if write_string(&mut payload, &name, 16).is_err() {
                    // unreachable: the name came through the String(16)-capped parser
                    BackendStats::incr(&inner.stats.protocol_errors);
                    break;
                }
                write_varint(0, &mut payload); // properties: none
                write_uuid(session_msb, session_lsb, &mut payload);

                let packet = Packet {
                    id: success_id,
                    payload: payload.freeze(),
                };
                if wire::write_packet(&mut stream, packet).await.is_err() {
                    BackendStats::incr(&inner.stats.write_errors);
                    break;
                }
                BackendStats::incr(&inner.stats.logins);
                phase = Phase::Play;
            }
            Phase::Play => {
                // hold the connection open, drain and ignore play traffic;
                // the backend must never answer or parse it
                BackendStats::incr(&inner.stats.discarded_frames);
            }
        }
    }

    BackendStats::incr(&inner.stats.connections_closed);
}
