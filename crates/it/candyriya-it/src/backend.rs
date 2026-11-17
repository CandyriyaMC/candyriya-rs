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
use candyriya_forwarding::verify_forwarding_data;
use candyriya_protocol::{
    DEFAULT_MAX_STRING_LENGTH, Handshake, Intent, Packet, PacketCodec, ProtocolError,
    decode_varint, parse_login_start_name, write_string, write_uuid, write_varint,
};
use candyriya_protocol_registry::{ConnectionState, Direction, PacketKind, PacketSlot, Registry};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::Decoder;

use crate::json::push_json_string;
use crate::uuid::uuid_from_name;
use crate::{HarnessError, protocol_id, resolve_version, wire};

/// How the backend handles the login phase.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BackendForwarding {
    /// Login start is answered with login success right away (the classic
    /// offline-mode backend behind a forwarding proxy).
    #[default]
    Off,
    /// Pull-model modern forwarding: on login start the backend sends a
    /// login plugin request on the `velocity:player_info` channel and waits
    /// for the proxy's plugin response. The sealed payload is verified with
    /// HMAC-SHA256 against `secret` (see `candyriya-forwarding`); only a
    /// payload that authenticates *and* carries the same username the client
    /// sent at login start earns a login success. Anything else is answered
    /// with a login disconnect. The `secret` is the raw key bytes, exactly
    /// as configured on both sides (here: as UTF-8 bytes of the string).
    Modern { secret: String },
    /// Answer any login start with a login disconnect carrying `reason`
    /// (e.g. a maintenance kick). Used to exercise the proxy's fallback.
    Disconnect { reason: String },
}

/// The plugin channel modern forwarding uses, i.e. `velocity:player_info`.
const PLAYER_INFO_CHANNEL: &str = "velocity:player_info";

/// Message id used for the `velocity:player_info` plugin request. The
/// fixture is single-purpose: one outstanding request per connection.
const PLAYER_INFO_MESSAGE_ID: i32 = 1;

/// Per-connection inbox depth for raw play-state bytes queued via
/// [`BackendFixture::send_play_bytes`]. Bounded on purpose: when the client
/// stops reading, the inbox fills and the sender back-pressures, which is
/// exactly what the hot-path pressure tests assert on.
const PLAY_INBOX_DEPTH: usize = 8;

/// Cap on the play-state bytes recorded for assertions. Bytes beyond it
/// are counted in `play_bytes_dropped` and not stored.
const PLAY_RECORD_CAP: usize = 8 * 1024 * 1024;

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
    /// How the backend behaves in the login phase. Use
    /// [`BackendForwarding::Off`] for the classic behavior.
    pub forwarding: BackendForwarding,
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
    /// `velocity:player_info` plugin requests sent by the backend so far
    /// (only in [`BackendForwarding::Modern`] mode).
    pub player_info_requests: u64,
    /// Plugin responses that failed verification so far (bad HMAC, bad id,
    /// wrong username, garbage): each one is answered with a login
    /// disconnect by the backend.
    pub forwarding_failures: u64,
    /// Login starts answered with a login disconnect in
    /// [`BackendForwarding::Disconnect`] mode.
    pub forced_disconnects: u64,
    /// Frames received in the play state and ignored (never replied to).
    pub discarded_frames: u64,
    /// Total play-state bytes read off the connection, including bytes
    /// dropped from the assertion record (see `play_bytes_dropped`).
    pub play_bytes_recorded: u64,
    /// Play-state bytes that arrived after the assertion record cap was
    /// reached (or the record lock poisoned) and were not stored.
    pub play_bytes_dropped: u64,
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
    player_info_requests: AtomicU64,
    forwarding_failures: AtomicU64,
    forced_disconnects: AtomicU64,
    discarded_frames: AtomicU64,
    play_bytes_recorded: AtomicU64,
    play_bytes_dropped: AtomicU64,
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
            player_info_requests: self.player_info_requests.load(Ordering::Relaxed),
            forwarding_failures: self.forwarding_failures.load(Ordering::Relaxed),
            forced_disconnects: self.forced_disconnects.load(Ordering::Relaxed),
            discarded_frames: self.discarded_frames.load(Ordering::Relaxed),
            play_bytes_recorded: self.play_bytes_recorded.load(Ordering::Relaxed),
            play_bytes_dropped: self.play_bytes_dropped.load(Ordering::Relaxed),
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
    /// Raw bytes received on play-state connections, for the hot-path
    /// assertions. Bounded by [`PLAY_RECORD_CAP`]; overflow lands in
    /// `play_bytes_dropped`. Every connection appends under this one lock —
    /// bytes from parallel connections interleave, but each connection's
    /// own byte order is preserved and every byte is recorded exactly once.
    play_received: Mutex<Vec<u8>>,
    /// Per-play-connection inboxes for raw bytes queued via
    /// [`BackendFixture::send_play_bytes`]. Keyed by a connection-local
    /// sequence number so dead connections can unregister themselves.
    play_writers: Mutex<Vec<(u64, mpsc::Sender<Bytes>)>>,
    /// Monotonic sequence for tagging play connections.
    play_conn_seq: AtomicU64,
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
/// * **Login state.** The first packet must be a login start. In
///   [`BackendForwarding::Off`] mode it is answered with a login success
///   packet built from the current layout - a Game Profile (UUID, username,
///   zero properties) followed by a session id UUID; pre-26.2 clients read
///   exactly the UUID and username and tolerate the remainder, which is why
///   one body is emitted for all protocol versions. In
///   [`BackendForwarding::Modern`] mode the backend first sends a login
///   plugin request on `velocity:player_info` (empty data) and waits for the
///   plugin response; the response is verified with HMAC-SHA256 against the
///   configured secret and against the username the client used at login
///   start, and only a valid response earns a login success (an invalid one
///   is answered with a login disconnect). In
///   [`BackendForwarding::Disconnect`] mode any login start is answered with
///   a login disconnect. The packet ids are resolved per protocol version
///   from the registry. Compression and encryption are never negotiated and
///   no set-compression packet is sent; framing stays in the uncompressed
///   format for the whole connection.
/// * **Play state.** After login success the connection is held open until
///   the client drops it. Play-state frames are read and discarded (counted
///   in `discarded_frames`); every byte the connection received is recorded
///   verbatim (bounded, see [`BackendFixture::take_play_bytes`]) so tests can
///   assert byte-exact copying. Tests can also inject raw bytes into the
///   connection(s) with [`BackendFixture::send_play_bytes`]; the bytes go
///   onto the wire verbatim.
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
            play_received: Mutex::new(Vec::new()),
            play_writers: Mutex::new(Vec::new()),
            play_conn_seq: AtomicU64::new(0),
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

    /// Send raw bytes to every play-state connection the fixture currently
    /// holds open (i.e. those that finished the login exchange and have not
    /// disconnected yet). Returns how many connections the bytes were
    /// queued on.
    ///
    /// The bytes are delivered verbatim — no framing is added or stripped,
    /// which is what the hot path promises end to end. The per-connection
    /// inbox is bounded (8 messages) and `send` awaits capacity, so this
    /// future back-pressures when the client stopped reading, exactly like
    /// the pump under test.
    pub async fn send_play_bytes(&self, bytes: &[u8]) -> usize {
        let senders: Vec<mpsc::Sender<Bytes>> = match self.inner.play_writers.lock() {
            Ok(mut guard) => {
                guard.retain(|(_, sender)| !sender.is_closed());
                guard.iter().map(|(_, sender)| sender.clone()).collect()
            }
            Err(_) => Vec::new(),
        };
        let mut queued = 0usize;
        for sender in senders {
            if sender.send(Bytes::copy_from_slice(bytes)).await.is_ok() {
                queued += 1;
            }
        }
        queued
    }

    /// Take the play-state bytes recorded on the connection(s), draining
    /// the buffer. Empty when nothing was received since the last call.
    pub fn take_play_bytes(&self) -> Vec<u8> {
        match self.inner.play_received.lock() {
            Ok(mut guard) => std::mem::take(&mut guard),
            Err(_) => Vec::new(),
        }
    }

    /// Total play-state bytes read off the connection(s), including bytes
    /// beyond the assertion record cap.
    pub fn play_bytes_recorded(&self) -> u64 {
        self.inner.stats.play_bytes_recorded.load(Ordering::Relaxed)
    }

    /// Play-state bytes that arrived after the assertion record cap was
    /// reached and were not stored.
    pub fn play_bytes_dropped(&self) -> u64 {
        self.inner.stats.play_bytes_dropped.load(Ordering::Relaxed)
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

/// The state the backend's login phase is in for this connection.
///
/// In [`BackendForwarding::Modern`] mode the backend asks the client for its
/// profile (`velocity:player_info`) and blocks on the answer; the pending
/// request is remembered here so exactly one outstanding request is ever in
/// flight per connection.
enum LoginStep {
    Idle,
    AwaitPlayerInfo { message_id: i32, name: String },
}

/// Build the login success packet for `profile`. The body is the current
/// layout — a Game Profile (UUID, username, zero properties) followed by a
/// session id UUID; pre-26.2 clients read exactly the UUID and username and
/// tolerate the remainder. In `Off` mode the fixture picks the profile
/// itself (FNV-based pseudo-UUID, documented); in `Modern` mode the
/// profile comes from the verified forwarding payload — like a real
/// forwarding-enabled backend, which trusts the proxy's sealed profile.
fn encode_login_success(
    registry: &Registry,
    protocol: i32,
    profile: &candyriya_player::GameProfile,
) -> Option<Packet> {
    let success_id =
        protocol_id(registry, protocol, PacketSlot::LoginClientboundLoginSuccess).ok()?;
    let (uuid_msb, uuid_lsb) = profile.id.as_u64_pair();
    let (session_msb, session_lsb) = uuid_from_name(&format!("session:{}", profile.name));
    let mut payload = BytesMut::with_capacity(34 + profile.name.len());
    write_uuid(uuid_msb as i64, uuid_lsb as i64, &mut payload);
    if write_string(&mut payload, &profile.name, 16).is_err() {
        // unreachable: the name came through the String(16)-capped parser
        return None;
    }
    write_varint(0, &mut payload); // properties: none
    write_uuid(session_msb, session_lsb, &mut payload);
    Some(Packet {
        id: success_id,
        payload: payload.freeze(),
    })
}

/// Build a login disconnect packet carrying `reason` as a JSON text
/// component. The reason travels inside a JSON string, so it is escaped
/// there by [`push_json_string`] rather than embedded raw.
fn encode_login_disconnect(registry: &Registry, protocol: i32, reason: &str) -> Option<Packet> {
    let disconnect_id =
        protocol_id(registry, protocol, PacketSlot::LoginClientboundDisconnect).ok()?;
    let mut json = String::with_capacity(reason.len() + 10);
    json.push_str("{\"text\":");
    push_json_string(&mut json, reason);
    json.push('}');
    let mut payload = BytesMut::with_capacity(json.len() + 5);
    write_string(&mut payload, &json, DEFAULT_MAX_STRING_LENGTH).ok()?;
    Some(Packet {
        id: disconnect_id,
        payload: payload.freeze(),
    })
}

/// Send a login disconnect, incrementing the given counter when the write
/// itself fails. The caller is expected to tear the connection down after
/// this returns.
async fn send_login_disconnect(
    stream: &mut TcpStream,
    registry: &Registry,
    protocol: i32,
    reason: &str,
) -> bool {
    let Some(packet) = encode_login_disconnect(registry, protocol, reason) else {
        return false;
    };
    wire::write_packet(stream, packet).await.is_ok()
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
    let mut login_step = LoginStep::Idle;

    loop {
        // the play state never reaches the frame-oriented reader below:
        // it runs its own select loop (raw byte recording + injected
        // writes) and leaves the function when both sides are done
        if matches!(phase, Phase::Play) {
            let play_id = inner.play_conn_seq.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = mpsc::channel::<Bytes>(PLAY_INBOX_DEPTH);
            if let Ok(mut writers) = inner.play_writers.lock() {
                writers.push((play_id, tx));
            }
            let mut inbox = Some(rx);
            loop {
                tokio::select! {
                    biased;
                    msg = async {
                        match &mut inbox {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending::<Option<Bytes>>().await,
                        }
                    } => {
                        match msg {
                            Some(bytes) => {
                                if stream.write_all(&bytes).await.is_err() {
                                    break;
                                }
                            }
                            // the sending half went away: keep draining
                            // reads only
                            None => inbox = None,
                        }
                    }
                    res = read_packet_recording(&mut stream, &mut buf, &inner) => {
                        match res {
                            Ok(Some(_)) => BackendStats::incr(&inner.stats.discarded_frames),
                            Ok(None) => break,
                            Err(_) => {
                                BackendStats::incr(&inner.stats.protocol_errors);
                                break;
                            }
                        }
                    }
                }
            }
            if let Ok(mut writers) = inner.play_writers.lock() {
                writers.retain(|(id, _)| *id != play_id);
            }
            break;
        }

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
                        // the vanilla wire format carries the document as a
                        // length-prefixed String, like a real server does
                        let mut payload = BytesMut::with_capacity(body.len() + 5);
                        if write_string(&mut payload, &body, DEFAULT_MAX_STRING_LENGTH).is_err() {
                            BackendStats::incr(&inner.stats.protocol_errors);
                            break;
                        }
                        let packet = Packet {
                            id: response_id,
                            payload: payload.freeze(),
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

                // a pending velocity:player_info request expects exactly one
                // packet: the plugin response. Anything else is a protocol
                // error and kills the connection.
                if let LoginStep::AwaitPlayerInfo { message_id, name } =
                    std::mem::replace(&mut login_step, LoginStep::Idle)
                {
                    let kind = registry.classify(
                        ConnectionState::Login,
                        Direction::Serverbound,
                        version,
                        packet.id,
                    );
                    let mut cursor: &[u8] = &packet.payload;
                    let message_ok =
                        matches!(decode_varint(&mut cursor), Ok(id) if id == message_id);
                    let success = match cursor.first() {
                        Some(&0x01) => {
                            cursor = &cursor[1..];
                            true
                        }
                        Some(&0x00) => {
                            cursor = &cursor[1..];
                            false
                        }
                        _ => {
                            BackendStats::incr(&inner.stats.protocol_errors);
                            break;
                        }
                    };
                    let secret = match &inner.config.forwarding {
                        BackendForwarding::Modern { secret } => secret.clone(),
                        _ => String::new(), // unreachable: AwaitPlayerInfo implies Modern
                    };
                    let verified =
                        if kind == PacketKind::LoginPluginResponse && message_ok && success {
                            verify_forwarding_data(secret.as_bytes(), cursor)
                                .ok()
                                .filter(|(_, profile)| profile.name == name)
                        } else {
                            None
                        };
                    match verified {
                        Some((_, profile)) => {
                            // a forwarding-enabled backend trusts the sealed
                            // profile: UUID and name come from it verbatim
                            let Some(packet) = encode_login_success(&registry, protocol, &profile)
                            else {
                                BackendStats::incr(&inner.stats.protocol_errors);
                                break;
                            };
                            if wire::write_packet(&mut stream, packet).await.is_err() {
                                BackendStats::incr(&inner.stats.write_errors);
                                break;
                            }
                            BackendStats::incr(&inner.stats.logins);
                            phase = Phase::Play;
                        }
                        None => {
                            BackendStats::incr(&inner.stats.forwarding_failures);
                            let _ = send_login_disconnect(
                                &mut stream,
                                &registry,
                                protocol,
                                "velocity:player_info verification failed",
                            )
                            .await;
                            break;
                        }
                    }
                    continue;
                }

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
                match &inner.config.forwarding {
                    BackendForwarding::Off => {
                        let (msb, lsb) = uuid_from_name(&name);
                        let profile = candyriya_player::GameProfile {
                            id: uuid::Uuid::from_u64_pair(msb as u64, lsb as u64),
                            name: name.clone(),
                            properties: Vec::new(),
                        };
                        let Some(packet) = encode_login_success(&registry, protocol, &profile)
                        else {
                            BackendStats::incr(&inner.stats.protocol_errors);
                            break;
                        };
                        if wire::write_packet(&mut stream, packet).await.is_err() {
                            BackendStats::incr(&inner.stats.write_errors);
                            break;
                        }
                        BackendStats::incr(&inner.stats.logins);
                        phase = Phase::Play;
                    }
                    BackendForwarding::Disconnect { reason } => {
                        BackendStats::incr(&inner.stats.forced_disconnects);
                        let _ =
                            send_login_disconnect(&mut stream, &registry, protocol, reason).await;
                        break;
                    }
                    BackendForwarding::Modern { .. } => {
                        let Some(request_id) = protocol_id(
                            &registry,
                            protocol,
                            PacketSlot::LoginClientboundPluginRequest,
                        )
                        .ok() else {
                            // modern forwarding does not exist before 1.13
                            BackendStats::incr(&inner.stats.protocol_errors);
                            break;
                        };
                        // the velocity:player_info request carries an empty
                        // data payload: the backend authenticates the answer
                        // with its own HMAC check, not with request data
                        let mut payload = BytesMut::with_capacity(32);
                        write_varint(PLAYER_INFO_MESSAGE_ID, &mut payload);
                        if write_string(
                            &mut payload,
                            PLAYER_INFO_CHANNEL,
                            DEFAULT_MAX_STRING_LENGTH,
                        )
                        .is_err()
                        {
                            BackendStats::incr(&inner.stats.protocol_errors);
                            break;
                        }
                        let packet = Packet {
                            id: request_id,
                            payload: payload.freeze(),
                        };
                        if wire::write_packet(&mut stream, packet).await.is_err() {
                            BackendStats::incr(&inner.stats.write_errors);
                            break;
                        }
                        BackendStats::incr(&inner.stats.player_info_requests);
                        login_step = LoginStep::AwaitPlayerInfo {
                            message_id: PLAYER_INFO_MESSAGE_ID,
                            name,
                        };
                    }
                }
            }
            Phase::Play => {
                // handled before the frame-oriented read at the top of the
                // loop; if this arm ever ran, closing beats looping
                break;
            }
        }
    }

    BackendStats::incr(&inner.stats.connections_closed);
}

/// Read one frame like [`wire::read_packet`], appending every byte read
/// from the socket to the fixture's play-state record (bounded by
/// [`PLAY_RECORD_CAP`]). Everything else — framing, EOF semantics, error
/// classification — matches the shared reader, so play traffic behaves
/// exactly like it does for the non-recording phases.
async fn read_packet_recording(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    inner: &BackendInner,
) -> Result<Option<Packet>, HarnessError> {
    let mut codec = PacketCodec::new();
    loop {
        match codec.decode(buf)? {
            Some(packet) => return Ok(Some(packet)),
            None => {
                let before = buf.len();
                if stream.read_buf(buf).await? == 0 {
                    return if before == 0 {
                        Ok(None)
                    } else {
                        Err(ProtocolError::Truncated.into())
                    };
                }
                record_play_bytes(inner, &buf[before..]);
            }
        }
    }
}

/// Append `bytes` to the play-state record, evicting nothing and counting
/// everything: bytes beyond the cap are tallied in `play_bytes_dropped`
/// instead of stored.
fn record_play_bytes(inner: &BackendInner, bytes: &[u8]) {
    inner
        .stats
        .play_bytes_recorded
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    match inner.play_received.lock() {
        Ok(mut record) => {
            let room = PLAY_RECORD_CAP.saturating_sub(record.len());
            let take = room.min(bytes.len());
            if take > 0 {
                record.extend_from_slice(&bytes[..take]);
            }
            if take < bytes.len() {
                inner
                    .stats
                    .play_bytes_dropped
                    .fetch_add((bytes.len() - take) as u64, Ordering::Relaxed);
            }
        }
        Err(_) => {
            inner
                .stats
                .play_bytes_dropped
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
    }
}
