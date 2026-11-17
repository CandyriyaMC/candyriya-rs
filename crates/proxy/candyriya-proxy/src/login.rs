//! The login pipeline (intent = 2): from Login Start to Login Success
//! (or a kick), end to end.
//!
//! The flow this module owns, in order:
//!
//! 1. **Login Start.** Read from the client (byte-wise, 2 MiB cap), name
//!    taken from the first string field; trailing fields (the 1.19+ player
//!    UUID and chat-signature block) are ignored, so clients of every
//!    supported version work unchanged. The raw packet is kept around to
//!    forward verbatim to the backend.
//! 2. **Username validation.** [`validate_username`] enforces the legacy
//!    3-16 ASCII `[a-z0-9_]` rule before anything else happens.
//! 3. **Profile resolution.** Offline mode derives the UUID with
//!    [`offline_uuid`] (UUID v3 / MD5 of `OfflinePlayer:<name>`). Online
//!    mode runs the encryption handshake with the client first: the proxy
//!    sends **its own** Encryption Request with a per-process RSA-2048
//!    keypair, the client answers with the RSA-encrypted shared secret and
//!    verify token, both are decrypted, the verify token is compared, the
//!    client link switches to AES-128-CFB8 (key and IV are both the shared
//!    secret) and the profile is verified against the Mojang session server
//!    (`hasJoined`) with the matching `serverId` hash. Forged or missing
//!    sessions never reach a backend.
//! 4. **Routing.** The backend list is: forced-host matches for the
//!    handshake hostname (declaration order), otherwise the `[servers]`
//!    `try` chain. A forced host that matches wins over the `try` chain.
//! 5. **Backend handshake.** TCP connect with `[advanced] connection-timeout`
//!    (and the remaining overall login budget), then the Handshake (the
//!    client's protocol is forwarded verbatim) and the Login Start,
//!    forwarded raw. For legacy/BungeeGuard forwarding the handshake
//!    `server_address` becomes the forwarding address instead
//!    (`host\0ip\0undashed-uuid\0json`, BungeeGuard appends
//!    `bungeeguard-token` as the last property).
//! 6. **Backend login exchange.** Reading from the backend, under the same
//!    budget:
//!    * **Set Compression** switches the backend link (and the client
//!      link, with the same threshold) to the compressed layout from the
//!      next packet on — Set Compression itself stays in the plain layout.
//!    * **Login Plugin Request** on `velocity:player_info` is answered with
//!      the modern forwarding payload: `sig(HMAC-SHA256, secret, payload)`
//!      followed by `VarInt(1) | String(client ip, no port) | UUID(msb,
//!      lsb) | String(name) | VarInt(properties)`, authenticated by the
//!      HMAC the backend verifies with the same secret. Any other channel
//!      is answered with `success = false`, matching a vanilla client.
//!      Pre-1.13 backends never see this flow (the packet ids do not
//!      exist there).
//!    * **Encryption Request from the backend** means the backend is in
//!      online mode behind the proxy, which this proxy does not serve
//!      (double-auth is not supported). The backend is treated as not
//!      viable and the chain moves on.
//!    * **Login Disconnect** is a kick: the chain moves on to the next
//!      backend. The backend's reason is passed to the client if the whole
//!      chain is exhausted (it is the backend's own user-facing text, not
//!      a proxy internal).
//!    * **Login Success** is forwarded verbatim to the client (raw
//!      passthrough, version-agnostic). In **modern** forwarding mode a
//!      Login Success that arrived *without* the backend having asked for
//!      `velocity:player_info` is a security violation — a backend that
//!      does not check who the player is must not be served; the client is
//!      disconnected and the chain is NOT advanced.
//! 7. **Compression layering.** Everything above the link functions is
//!    packet-level; the zlib layer and the cipher are composed inside
//!    [`crate::wire`] as `[AES-CFB8 [compressed [packet]]]`, exactly the
//!    nesting the wire protocol defines.
//!
//! **Budgets.** Every blocking wait in the login phase is bounded: reads
//! that expect a packet from the peer (Login Start, the encryption
//! handshake, each backend attempt) each get one window of
//! `[advanced] connection-timeout` plus five seconds of headroom; the
//! backend chain as a whole gets one window per candidate plus a little
//! slack. Hitting an attempt's window closes the backend connection and
//! falls back to the next one; hitting the overall bound drops everything
//! and disconnects the client.
//!
//! **Sneak clients.** A client that sends ciphertext before the encryption
//! handshake is established (or anything else before the frame it is
//! expected to produce) stops the read: the budget expires and the
//! connection is dropped.
//!
//! **Session hand-off.** Once the Login Success has been written to the
//! client, the connection transitions to the play state: the
//! [`ConnectionSlot`](crate::ConnectionSlot) phase flips `login` -> `play`,
//! `conn=N transitioning to play state` is logged, and the sockets are
//! handed to the play-state byte pump (`crate::play`), which copies bytes
//! bidirectionally without parsing until both sides close. The function
//! returns only after that pump stops — the connection is never closed
//! early by the login slice itself.

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use candyriya_auth::{
    AuthError, MojangSessionValidator, OfflineSessionValidator, SessionValidator, validate_username,
};
use candyriya_compression::Compression;
use candyriya_config::{Config, ForwardingMode};
use candyriya_encryption::{AesCfb8, EncryptionKeypair, SharedSecret, server_id_hash};
use candyriya_forwarding::{
    clean_ipv6_scope, create_forwarding_data, create_legacy_forwarding_address,
};
use candyriya_player::GameProfile;
use candyriya_protocol::{
    Handshake, Intent, MAX_PACKET_SIZE, Packet, decode_varint, parse_login_start_name, read_string,
    write_string, write_varint,
};
use candyriya_protocol_registry::{ConnectionState, Direction, PacketKind, PacketSlot};
use candyriya_route::{Backend, BackendState};

use crate::ProxyError;
use crate::accept::ProxyState;
use crate::handshake::HandshakeResult;
use crate::limit::ConnectionSlot;
use crate::registry::embedded_registry;
use crate::wire::{self, Wire};

/// Headroom added on top of `[advanced] connection-timeout` for the whole
/// login exchange (both budget drains and slowest-backend retries).
const LOGIN_BUDGET_HEADROOM: Duration = Duration::from_secs(5);

/// The plugin channel name modern forwarding uses, verbatim from the
/// ecosystem (Paper and Velocity exchange bytes on this exact name).
const VELOCITY_PLAYER_INFO: &str = "velocity:player_info";

/// Protocol 1.20.5 (766) added the `shouldAuthenticate` boolean to the
/// Encryption Request packet (verified against prismarinejs minecraft-data,
/// `data/pc/*/protocol.json`: the 1.20.3 schema has no such field, the
/// 1.20.5 schema does). Sending one extra byte on older versions would
/// desync strict clients, so it is appended only for 766+.
const PROTOCOL_ENCRYPTION_SHOULD_AUTHENTICATE: i32 = 766;

/// The verify token the proxy sends in its Encryption Request: 4 bytes of
/// fresh randomness per connection; the client must echo them RSA-encrypted
/// in the Encryption Response.
const VERIFY_TOKEN_LEN: usize = 4;

/// Clamp for client-facing kick reasons. Backends' own disconnect reasons
/// are user-facing text, we just make sure they cannot grow the frame
/// unboundedly; the remaining bytes are cut at a char boundary.
const MAX_KICK_REASON_CHARS: usize = 256;

/// One backend-attempt window: `[advanced] connection-timeout` plus five
/// seconds of headroom. Every blocking read in the login phase is bounded
/// by this (or by the remaining overall budget, whichever is smaller), so a
/// black-holed backend is dropped in favor of the next one in the chain.
fn attempt_budget(config: &Config) -> Duration {
    Duration::from_millis(u64::from(config.advanced.connection_timeout)) + LOGIN_BUDGET_HEADROOM
}

/// The outcome of one backend attempt.
enum Attempt {
    /// Login Success was delivered to the client; the session is now in
    /// the play state. The backend connection is handed to the byte pump —
    /// the backend wire state stays behind (the pump copies bytes
    /// verbatim, so it needs none of it).
    Done { backend: TcpStream },
    /// The client connection is gone (or its socket died); nothing to do.
    ClientGone,
    /// The backend kicked the player (or is not viable); the chain moves
    /// on. The string is the backend's reason when it kicked, or a
    /// diagnostic otherwise (for logs and the final disconnect).
    NotViable(String),
    /// A modern-forwarding backend completed login without asking for
    /// `velocity:player_info`. This is a security violation, not a
    /// transient failure: the player is disconnected and the chain is not
    /// advanced.
    SecurityViolation(String),
}

/// Handle one login-intent connection end to end.
///
/// `stream` has already been read by exactly the handshake frame
/// ([`crate::read_handshake`]); the pipeline continues in this function and
/// returns once the session has either reached the play state (and the
/// byte pump has stopped), been kicked by a backend, or expired. Every
/// wait inside is armed: no path blocks forever. See the module docs for
/// the full contract.
///
/// `slot` is the connection's guard; its phase flag is flipped to
/// `play` the moment the Login Success reaches the client.
///
/// # Errors
///
/// Failures before (or around) writing to the client are returned as
/// [`ProxyError`]; everything after a clean Login Success is `Ok(())`.
/// Errors are safe to log; the client never sees internals.
pub async fn handle_login(
    state: &ProxyState,
    stream: &mut TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    handshake: &HandshakeResult,
    slot: &ConnectionSlot,
) -> Result<(), ProxyError> {
    login_flow(state, stream, peer, conn_id, handshake, slot).await
}

/// The whole login exchange. Split from [`handle_login`] so the entry
/// point stays a trivial forward; the budgets are armed inside.
async fn login_flow(
    state: &ProxyState,
    stream: &mut TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    handshake: &HandshakeResult,
    slot: &ConnectionSlot,
) -> Result<(), ProxyError> {
    if embedded_registry().is_none() {
        // build bug: the compiled-in tables are invalid. Loud in the log,
        // nothing useful to tell the client.
        return Err(ProxyError::Registry(
            "the embedded protocol registry failed to load".to_owned(),
        ));
    }

    // ---- 1. Login Start (the client link is plain at this point)
    let mut client = Wire::new();
    let per_attempt = attempt_budget(&state.config());
    let login_start = match timeout(per_attempt, wire::read_packet(stream, &mut client)).await {
        Ok(Ok(packet)) => packet,
        Ok(Err(error)) => return Err(error),
        // the sneak client (or a dropped one): close silently, like the
        // other protocol-violation paths
        Err(_) => {
            debug!(conn = conn_id, %peer, "login start did not arrive within the login budget; closing");
            return Ok(());
        }
    };
    if registry_id(handshake.protocol, PacketSlot::LoginServerboundStart) != Some(login_start.id) {
        return Err(ProxyError::BadPacket(format!(
            "expected a login start packet, got id {}",
            login_start.id
        )));
    }

    // parse_login_start_name wants the frame body *with* the packet id;
    // rebuild it for the partial parser
    let mut frame_body = BytesMut::with_capacity(login_start.payload.len() + 5);
    write_varint(login_start.id, &mut frame_body);
    frame_body.extend_from_slice(&login_start.payload);
    let name = parse_login_start_name(&frame_body)
        .map_err(|error| ProxyError::BadPacket(error.to_string()))?;

    // ---- 2. username validation
    if let Err(error) = validate_username(&name) {
        debug!(conn = conn_id, %peer, name = %name, %error, "rejected invalid username");
        send_disconnect(stream, &mut client, handshake.protocol, "Invalid username").await;
        return Ok(());
    }

    // ---- 3. profile resolution (and the client-side encryption handshake)
    let profile = match timeout(
        per_attempt,
        resolve_profile(state, stream, &mut client, conn_id, handshake, &name),
    )
    .await
    {
        Ok(Ok(profile)) => profile,
        Ok(Err(ProfileError::Disconnected(reason))) => {
            send_disconnect(stream, &mut client, handshake.protocol, &reason).await;
            return Ok(());
        }
        Ok(Err(ProfileError::Fatal(error))) => return Err(error),
        Err(_) => {
            debug!(conn = conn_id, %peer, "the encryption handshake did not complete within the login budget; closing");
            return Ok(());
        }
    };

    // ---- 4. routing: forced host matches first, else the try chain
    let backends = route_backends(state, &handshake.server_address);
    if backends.is_empty() {
        warn!(conn = conn_id, %peer, "no backend available for this host; disconnecting");
        send_disconnect(
            stream,
            &mut client,
            handshake.protocol,
            "No available server",
        )
        .await;
        return Ok(());
    }

    // ---- 5-6. backend attempts with kick fallback
    let client_ip = clean_ipv6_scope(&peer.ip().to_string()).into_owned();
    // overall budget: the encryption window plus one window per backend,
    // with slack; each attempt is additionally capped by its own window,
    // so a hung backend yields to the next one instead of eating the flow
    let per_attempt = attempt_budget(&state.config());
    let overall = per_attempt.saturating_mul(backends.len() as u32) + Duration::from_secs(1);
    let deadline = Instant::now() + overall;
    let mut last_reason: Option<String> = None;
    for backend in backends {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            last_reason = Some("backend timed out".to_owned());
            break;
        }
        let window = remaining.min(per_attempt);
        let backend_name = backend.name().to_owned();
        let outcome = timeout(
            window,
            attempt_backend(
                state,
                stream,
                &mut client,
                peer,
                conn_id,
                handshake,
                backend,
                &profile,
                &login_start,
                &client_ip,
            ),
        )
        .await;
        match outcome {
            Ok(Attempt::Done { backend, .. }) => {
                // the backend wire state is not needed by the pump: zlib
                // passes through untouched, and the backend never carries
                // a cipher — drop it on the floor
                info!(conn = conn_id, %peer, "conn={conn_id} transitioning to play state");
                slot.mark_play();
                let mut backend = backend;
                let write_stall =
                    Duration::from_millis(u64::from(state.config().advanced.read_timeout));
                crate::play::run_play_state(
                    stream,
                    &mut backend,
                    &mut client,
                    conn_id,
                    peer,
                    write_stall,
                )
                .await;
                return Ok(());
            }
            Ok(Attempt::ClientGone) => return Ok(()),
            Ok(Attempt::SecurityViolation(reason)) => {
                // security violation: refuse, never fall back
                warn!(conn = conn_id, %peer, %reason, "backend completed login without the forwarding exchange; disconnecting");
                send_disconnect(
                    stream,
                    &mut client,
                    handshake.protocol,
                    "Unable to connect to a server",
                )
                .await;
                return Ok(());
            }
            Ok(Attempt::NotViable(reason)) => {
                debug!(conn = conn_id, %peer, %reason, "backend not viable; trying the next one");
                last_reason = Some(reason);
            }
            Err(_) => {
                // this backend burned its window; fall back to the next one
                debug!(conn = conn_id, %peer, backend = %backend_name, "backend timed out; trying the next one");
                last_reason = Some(format!("backend {backend_name} timed out"));
            }
        }
    }

    let reason = last_reason.unwrap_or_else(|| "No available server".to_owned());
    send_disconnect(stream, &mut client, handshake.protocol, &reason).await;
    Ok(())
}

/// Why the login attempt up to the profile resolution could not finish.
#[derive(Debug)]
enum ProfileError {
    /// The client was told (a kick reason); the connection wraps up.
    Disconnected(String),
    /// An actual error (I/O, registry, internal); the caller surfaces it.
    Fatal(ProxyError),
}

impl From<ProxyError> for ProfileError {
    fn from(error: ProxyError) -> Self {
        Self::Fatal(error)
    }
}

/// Validate the client's profile: offline derivation, or the online-mode
/// encryption handshake with Mojang verification.
async fn resolve_profile(
    state: &ProxyState,
    stream: &mut TcpStream,
    client: &mut Wire,
    conn_id: u64,
    handshake: &HandshakeResult,
    name: &str,
) -> Result<GameProfile, ProfileError> {
    if !state.config().online_mode {
        let validator = OfflineSessionValidator;
        return validator
            .verify_client(name, "")
            .await
            .map_err(|error| ProfileError::Fatal(ProxyError::BadPacket(error.to_string())));
    }

    // ---- 3a. the proxy's own Encryption Request (per-process keypair)
    let keypair = process_keypair().map_err(ProfileError::Fatal)?;
    let expected_verify_token: [u8; VERIFY_TOKEN_LEN] = rand::random();
    let protocol = handshake.protocol;

    // payload: String(serverId="") | PrefixedArray(publicKey DER)
    //        | PrefixedArray(verifyToken) | shouldAuthenticate (1.20.5+)
    let mut payload = BytesMut::with_capacity(320);
    write_string(&mut payload, "", 20)
        .map_err(|error| ProfileError::Fatal(ProxyError::BadPacket(error.to_string())))?;
    write_byte_array(&mut payload, &keypair.public_der);
    write_byte_array(&mut payload, &expected_verify_token);
    if protocol >= PROTOCOL_ENCRYPTION_SHOULD_AUTHENTICATE {
        payload.extend_from_slice(&[0x01]); // the client must authenticate via Mojang
    }
    let request_id = registry_id(protocol, PacketSlot::LoginClientboundEncryptionRequest)
        .ok_or_else(|| {
            ProfileError::Fatal(ProxyError::Registry(
                "no packet id for the encryption request".to_owned(),
            ))
        })?;
    wire::write_packet(
        stream,
        client,
        Packet {
            id: request_id,
            payload: payload.freeze(),
        },
    )
    .await?;

    // ---- 3b. the Encryption Response (RSA-encrypted secret + token)
    let response = wire::read_packet(stream, client).await?;
    if classify_login(protocol, Direction::Serverbound, response.id)
        != Some(PacketKind::EncryptionResponse)
    {
        return Err(ProfileError::Fatal(ProxyError::BadPacket(format!(
            "expected an encryption response, got id {}",
            response.id
        ))));
    }
    let mut cursor: &[u8] = &response.payload;
    // RSA-2048 ciphertexts are 256 bytes; generous cap for future keys
    let shared_secret_bytes = read_byte_array(&mut cursor, 8192).ok_or_else(|| {
        ProfileError::Fatal(ProxyError::BadPacket(
            "malformed shared secret in the encryption response".to_owned(),
        ))
    })?;
    let verify_token_bytes = read_byte_array(&mut cursor, 8192).ok_or_else(|| {
        ProfileError::Fatal(ProxyError::BadPacket(
            "malformed verify token in the encryption response".to_owned(),
        ))
    })?;

    let shared_secret = keypair
        .decrypt_with_private(&shared_secret_bytes)
        .map_err(|_| ProfileError::Disconnected("Encryption keys rejected".to_owned()))?;
    if shared_secret.len() != SharedSecret::LEN {
        // RSA decryption of an attacker-chosen ciphertext yields synthetic
        // bytes of arbitrary length; refuse anything but a 16-byte secret
        return Err(ProfileError::Disconnected(
            "Invalid shared secret".to_owned(),
        ));
    }
    let verify_token = keypair
        .decrypt_with_private(&verify_token_bytes)
        .map_err(|_| ProfileError::Disconnected("Encryption keys rejected".to_owned()))?;
    if verify_token != expected_verify_token {
        return Err(ProfileError::Disconnected(
            "Invalid verify token".to_owned(),
        ));
    }
    let mut secret_bytes = [0_u8; SharedSecret::LEN];
    secret_bytes.copy_from_slice(&shared_secret);
    let shared_secret = SharedSecret::from_bytes(secret_bytes);

    // ---- 3c. the client link switches to AES-128-CFB8 (key and IV are the secret)
    client.enable_cipher(AesCfb8::new(&shared_secret));

    // ---- 3d. verify the session with the Mojang session server (hasJoined)
    let server_id = server_id_hash("", &secret_bytes, &keypair.public_der);
    let validator =
        MojangSessionValidator::new(state.auth.session_server_url.clone(), state.auth.timeout)
            .map_err(|error| ProfileError::Fatal(ProxyError::Misconfigured(error.to_string())))?;
    match validator.verify_client(name, &server_id).await {
        Ok(profile) => {
            debug!(conn = conn_id, name = %profile.name, "session verified with the session server");
            Ok(profile)
        }
        Err(AuthError::SessionRejected(_)) => Err(ProfileError::Disconnected(
            "Failed to verify username!".to_owned(),
        )),
        Err(other) => {
            debug!(conn = conn_id, %other, "session server verification failed");
            Err(ProfileError::Disconnected(
                "Failed to verify username!".to_owned(),
            ))
        }
    }
}

/// Try one backend. See the module docs for the packet exchange; the
/// `Attempt` classification drives the kick fallback in [`login_flow`].
#[allow(clippy::too_many_arguments)]
async fn attempt_backend(
    state: &ProxyState,
    stream: &mut TcpStream,
    client: &mut Wire,
    peer: SocketAddr,
    conn_id: u64,
    handshake: &HandshakeResult,
    backend: Backend,
    profile: &GameProfile,
    login_start: &Packet,
    client_ip: &str,
) -> Attempt {
    let config = state.config();
    let protocol = handshake.protocol;

    // ---- 5. connect with [advanced] connection-timeout
    let connect_timeout = Duration::from_millis(u64::from(config.advanced.connection_timeout));
    let connect = timeout(connect_timeout, TcpStream::connect(backend.addr())).await;
    let mut backend_stream = match connect {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            mark_offline(state, backend.name());
            return Attempt::NotViable(format!("connect to {} failed: {error}", backend.name()));
        }
        Err(_) => {
            mark_offline(state, backend.name());
            return Attempt::NotViable(format!("connect to {} timed out", backend.name()));
        }
    };
    if let Err(error) = backend_stream.set_nodelay(true) {
        warn!(conn = conn_id, %peer, %error, "failed to set TCP_NODELAY on the backend link; continuing");
    }

    // ---- 5. the backend handshake: a forwarding address for legacy modes,
    //         the client's original address otherwise
    let mut wire = Wire::new();
    let server_address = match config.player_info_forwarding_mode {
        ForwardingMode::Legacy => {
            // the shared secret is NOT sent in legacy mode; the profile goes
            // inline in the address instead
            create_legacy_forwarding_address(&handshake.server_address, client_ip, profile, None)
        }
        ForwardingMode::BungeeGuard => {
            // the shared forwarding secret doubles as the guard token; it is
            // never logged, only used as a token byte string
            create_legacy_forwarding_address(
                &handshake.server_address,
                client_ip,
                profile,
                Some(config.forwarding_secret.as_bytes()),
            )
        }
        ForwardingMode::Modern | ForwardingMode::Disabled => handshake.server_address.clone(),
    };
    let backend_handshake = Handshake {
        protocol,
        server_address,
        server_port: handshake.server_port,
        // the backend speaks the login flow, always
        intent: Intent::Login,
    };
    let handshake_payload = match backend_handshake.encode_payload() {
        Ok(payload) => payload,
        Err(error) => {
            return Attempt::NotViable(format!("backend handshake encode failed: {error}"));
        }
    };
    if wire::write_packet(
        &mut backend_stream,
        &mut wire,
        Packet {
            id: Handshake::PACKET_ID,
            payload: handshake_payload,
        },
    )
    .await
    .is_err()
    {
        return Attempt::NotViable(format!(
            "backend {} closed on the handshake",
            backend.name()
        ));
    }
    if wire::write_packet(&mut backend_stream, &mut wire, login_start.clone())
        .await
        .is_err()
    {
        return Attempt::NotViable(format!(
            "backend {} closed on the login start",
            backend.name()
        ));
    }

    // ---- 6. the backend login exchange
    let modern = config.player_info_forwarding_mode == ForwardingMode::Modern;
    let secret = config.forwarding_secret.as_bytes();
    let level = config.advanced.compression_level;
    let mut player_info_done = false;
    loop {
        let packet = match wire::read_packet(&mut backend_stream, &mut wire).await {
            Ok(packet) => packet,
            Err(error) => {
                return Attempt::NotViable(format!("backend {}: {error}", backend.name()));
            }
        };
        let Some(kind) = classify_login(protocol, Direction::Clientbound, packet.id) else {
            return Attempt::NotViable(format!(
                "backend {} spoke unclassifiable packet id {}",
                backend.name(),
                packet.id
            ));
        };
        match kind {
            PacketKind::SetCompression => {
                let mut payload = &packet.payload[..];
                let threshold = decode_varint(&mut payload).unwrap_or(-1);
                let enable = if threshold >= 0 {
                    Compression::new(threshold, level).ok()
                } else {
                    None
                };
                // the Set Compression packet itself travels plain on both
                // links; forward it verbatim, then switch both sides
                wire.enable_compression(enable);
                if wire::write_packet(stream, client, packet).await.is_err() {
                    return Attempt::ClientGone;
                }
                client.enable_compression(if threshold >= 0 {
                    Compression::new(threshold, level).ok()
                } else {
                    None
                });
            }
            PacketKind::LoginPluginRequest => {
                match handle_plugin_request(
                    &mut backend_stream,
                    &mut wire,
                    protocol,
                    &packet,
                    modern,
                    secret,
                    client_ip,
                    profile,
                )
                .await
                {
                    Some(done) => player_info_done = done,
                    None => {
                        return Attempt::NotViable(format!(
                            "backend {}: plugin request handling failed",
                            backend.name()
                        ));
                    }
                }
            }
            PacketKind::EncryptionRequest => {
                // a backend in online mode would double-authenticate the
                // client; this proxy does not serve that shape. Move the
                // chain instead of pretending.
                return Attempt::NotViable(format!(
                    "backend {} requires its own encryption (online-mode backend)",
                    backend.name()
                ));
            }
            PacketKind::LoginDisconnect => {
                let reason = extract_disconnect_reason(&packet.payload);
                return Attempt::NotViable(reason);
            }
            PacketKind::LoginSuccess => {
                if modern && !player_info_done {
                    // security: a backend that never asked who the player
                    // is must not be served. No fallback.
                    return Attempt::SecurityViolation(format!(
                        "backend {} skipped the velocity:player_info exchange",
                        backend.name()
                    ));
                }
                // forward the Login Success verbatim to the client; the
                // frame is version-agnostic passthrough
                if wire::write_packet(stream, client, packet).await.is_err() {
                    return Attempt::ClientGone;
                }
                return Attempt::Done {
                    backend: backend_stream,
                };
            }
            // anything else (cookie packets, acknowledged, play packets
            // early): the backend is speaking a shape we do not model here
            other => {
                return Attempt::NotViable(format!(
                    "backend {} sent unexpected {other} during login",
                    backend.name()
                ));
            }
        }
    }
}

/// Answer a Login Plugin Request from the backend.
///
/// Returns `Some(player_info_done)` on completion — `true` when the request
/// was `velocity:player_info` and was answered with the sealed forwarding
/// payload — or `None` when the packet could not be parsed or the response
/// could not be written (the backend is treated as not viable).
#[allow(clippy::too_many_arguments)]
async fn handle_plugin_request(
    stream: &mut TcpStream,
    wire: &mut Wire,
    protocol: i32,
    packet: &Packet,
    modern: bool,
    secret: &[u8],
    client_ip: &str,
    profile: &GameProfile,
) -> Option<bool> {
    let mut cursor: &[u8] = &packet.payload;
    let message_id = decode_varint(&mut cursor).ok()?;
    let channel = read_string(&mut cursor, MAX_PACKET_SIZE).ok()?;
    // the rest of the packet is the channel-specific data, ignored

    let response_id = registry_id(protocol, PacketSlot::LoginServerboundPluginResponse)?;
    if modern && channel == VELOCITY_PLAYER_INFO {
        let sealed = create_forwarding_data(secret, client_ip, profile);
        let mut payload = BytesMut::with_capacity(sealed.len() + 5);
        write_varint(message_id, &mut payload);
        payload.extend_from_slice(&[0x01]); // success
        payload.extend_from_slice(&sealed);
        wire::write_packet(
            stream,
            wire,
            Packet {
                id: response_id,
                payload: payload.freeze(),
            },
        )
        .await
        .ok()?;
        Some(true)
    } else {
        // an unknown channel is answered with success = false and no data,
        // exactly what a vanilla client says when it does not understand
        let mut payload = BytesMut::new();
        write_varint(message_id, &mut payload);
        payload.extend_from_slice(&[0x00]); // not successful
        wire::write_packet(
            stream,
            wire,
            Packet {
                id: response_id,
                payload: payload.freeze(),
            },
        )
        .await
        .ok()?;
        Some(false)
    }
}

/// Send a login disconnect to the client with `reason` as a plain-text
/// JSON component. Best effort: a client that cannot be reached is fine.
async fn send_disconnect(stream: &mut TcpStream, client: &mut Wire, protocol: i32, reason: &str) {
    let reason_json = serde_json::json!({ "text": clamp_reason(reason) }).to_string();
    let Some(disconnect_id) = registry_id(protocol, PacketSlot::LoginClientboundDisconnect) else {
        return;
    };
    let mut payload = BytesMut::new();
    if write_string(&mut payload, &reason_json, MAX_PACKET_SIZE).is_err() {
        return;
    }
    let packet = Packet {
        id: disconnect_id,
        payload: payload.freeze(),
    };
    // a best-effort write; the connection closes regardless of the outcome
    let _ = wire::write_packet(stream, client, packet).await;
}

/// Truncate a backend-provided kick reason at a char boundary.
fn clamp_reason(reason: &str) -> String {
    reason.chars().take(MAX_KICK_REASON_CHARS).collect()
}

/// Extract a human-readable reason out of a login disconnect payload
/// (a JSON text component). Total: garbage yields a generic text, never a
/// panic and never the raw bytes.
fn extract_disconnect_reason(payload: &[u8]) -> String {
    let mut cursor: &[u8] = payload;
    let Ok(json) = read_string(&mut cursor, MAX_PACKET_SIZE) else {
        return "Disconnected".to_owned();
    };
    match serde_json::from_str::<serde_json::Value>(&json) {
        Ok(serde_json::Value::Object(mut map)) => match map.remove("text") {
            Some(serde_json::Value::String(text)) => text,
            _ => json,
        },
        _ => json,
    }
}

/// The ordered list of backends to try: forced-host matches in declaration
/// order win over the `[servers]` try chain; otherwise the try chain in
/// order. Unknown names are skipped (the registry validated them at
/// construction time).
fn route_backends(state: &ProxyState, host: &str) -> Vec<Backend> {
    let Some(router) = state.router() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(names) = router.resolve_host(host) {
        for name in names {
            if let Some(backend) = router.get(name) {
                out.push(backend.clone());
            }
        }
    }
    if out.is_empty() {
        for name in router.try_chain() {
            if let Some(backend) = router.get(name) {
                out.push(backend.clone());
            }
        }
    }
    out
}

/// Resolve the packet id for `slot` at `protocol`, falling back to the
/// newest known release when the client speaks a version the registry has
/// no table for; `None` only when even that is missing (a build bug).
fn registry_id(protocol: i32, slot: PacketSlot) -> Option<i32> {
    let registry = embedded_registry()?;
    registry
        .packet_id(protocol, slot)
        .or_else(|| registry.packet_id(registry.latest().protocol, slot))
}

/// Classify a login packet at `protocol` and `direction`.
fn classify_login(protocol: i32, direction: Direction, id: i32) -> Option<PacketKind> {
    let registry = embedded_registry()?;
    let fallback = registry.latest();
    let version = registry.from_protocol(protocol).unwrap_or(fallback);
    Some(registry.classify(ConnectionState::Login, direction, version, id))
}

/// Build the backend registry from the `[servers]` and `[forced-hosts]`
/// sections of `config`.
///
/// DNS resolution happens here (blocking `to_socket_addrs`, once, at
/// startup); the proxy never resolves in the hot path. A server address
/// that does not resolve fails the whole build — silently dropping a
/// backend from routing is how players end up on the wrong server.
///
/// # Errors
///
/// Returns a human-readable message when a server address cannot be
/// resolved or the configuration is unusable.
pub fn registry_from_config(config: &Config) -> Result<candyriya_route::BackendRegistry, String> {
    use std::net::ToSocketAddrs;

    let mut backends = Vec::with_capacity(config.servers.servers.len());
    for (name, address) in &config.servers.servers {
        let resolved = address
            .to_socket_addrs()
            .map_err(|error| format!("server {name:?} ({address:?}) did not resolve: {error}"))?;
        let addr = resolved
            .into_iter()
            .next()
            .ok_or_else(|| format!("server {name:?} ({address:?}) resolved to no addresses"))?;
        backends.push((name.clone(), addr));
    }
    let mut registry =
        candyriya_route::BackendRegistry::new(backends, config.servers.try_servers.clone())
            .map_err(|error| error.to_string())?;
    if !config.forced_hosts.is_empty() {
        let forced = candyriya_route::ForcedHosts::new(config.forced_hosts.clone())
            .map_err(|error| error.to_string())?;
        registry
            .set_forced_hosts(forced)
            .map_err(|error| error.to_string())?;
    }
    Ok(registry)
}

/// The process-wide RSA keypair for the online-mode handshake.
///
/// One keypair per process, created lazily at the first online login
/// (RSA-2048 generation is CPU-heavy — per connection it would stall a
/// worker for hundreds of milliseconds and burn CPU per login; vanilla
/// generates one at startup for the same reason). The first caller pays
/// the generation; everyone else shares it. The result (including a
/// generation failure) is cached like the key itself.
fn process_keypair() -> Result<&'static EncryptionKeypair, ProxyError> {
    static KEYPAIR: OnceLock<Result<EncryptionKeypair, String>> = OnceLock::new();
    KEYPAIR
        .get_or_init(|| EncryptionKeypair::generate().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| {
            ProxyError::Misconfigured(format!("failed to generate the RSA keypair: {error}"))
        })
}

/// Write a VarInt-length-prefixed byte array.
fn write_byte_array(out: &mut BytesMut, bytes: &[u8]) {
    let len = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    write_varint(len, out);
    out.extend_from_slice(bytes);
}

/// Read a VarInt-length-prefixed byte array, with a size cap.
fn read_byte_array(src: &mut &[u8], max: usize) -> Option<Vec<u8>> {
    let len = decode_varint(src).ok()?;
    let len = usize::try_from(len).ok()?;
    let limit = max.min(MAX_PACKET_SIZE);
    if len > limit || src.len() < len {
        return None;
    }
    let (head, tail) = src.split_at(len);
    let out = head.to_vec();
    *src = tail;
    Some(out)
}

/// Mark a backend offline after a connect failure, so health-aware paths
/// observe reality. Best effort: an unregistered name is ignored.
fn mark_offline(state: &ProxyState, name: &str) {
    if let Some(router) = state.router() {
        let _ = router.set_state(name, BackendState::Offline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use candyriya_config::ForwardingSecret;
    use candyriya_player::offline_uuid;
    use candyriya_protocol::write_string;
    use candyriya_route::{BackendRegistry, ForcedHosts};
    use tokio::net::TcpListener;

    fn dummy_handshake() -> HandshakeResult {
        HandshakeResult {
            protocol: 767,
            server_address: "play.example.com".to_owned(),
            server_port: 25565,
            intent: Intent::Login,
        }
    }

    fn offline_state() -> ProxyState {
        let config = Config {
            online_mode: false,
            forwarding_secret: ForwardingSecret::new(vec![0x53; 32]),
            ..Config::default()
        };
        ProxyState::new(Arc::new(config))
    }

    #[tokio::test]
    async fn offline_profile_uses_the_vanilla_offline_uuid() {
        let state = offline_state();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (mut client_stream, _server_end) = {
            let (a, b) = tokio::join!(TcpStream::connect(addr), listener.accept());
            (a.unwrap(), b)
        };
        let mut wire = Wire::new();
        let profile = resolve_profile(
            &state,
            &mut client_stream,
            &mut wire,
            1,
            &dummy_handshake(),
            "Steve",
        )
        .await
        .expect("offline profile resolves without the network");
        assert_eq!(profile.id, offline_uuid("Steve"));
        assert_eq!(profile.name, "Steve");
        assert!(profile.properties.is_empty());
    }

    #[test]
    fn extract_disconnect_reason_reads_the_text_field() {
        let json = "{\"text\":\"maintenance\"}";
        let mut payload = BytesMut::new();
        write_string(&mut payload, json, MAX_PACKET_SIZE).unwrap();
        assert_eq!(extract_disconnect_reason(&payload), "maintenance");
    }

    #[test]
    fn extract_disconnect_reason_returns_raw_json_or_generic() {
        // a json value without a "text" string comes back raw, not crashed
        let json = "[1, 2, 3]";
        let mut payload = BytesMut::new();
        write_string(&mut payload, json, MAX_PACKET_SIZE).unwrap();
        assert_eq!(extract_disconnect_reason(&payload), json);
        // bytes that are not even a string yield the generic text
        assert_eq!(extract_disconnect_reason(&[0xFF, 0xFF]), "Disconnected");
    }

    #[test]
    fn clamp_reason_truncates_at_char_boundaries() {
        let reason = "é".repeat(300); // 2 bytes per char, 600 bytes total
        let clamped = clamp_reason(&reason);
        assert_eq!(clamped.chars().count(), MAX_KICK_REASON_CHARS);
        // ascii-only fast path
        assert_eq!(clamp_reason(&"a".repeat(300)).len(), MAX_KICK_REASON_CHARS);
    }

    #[test]
    fn route_prefers_forced_hosts_over_the_try_chain() {
        let addr_a: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        let addr_b: std::net::SocketAddr = "127.0.0.1:8".parse().unwrap();
        let mut registry = BackendRegistry::new(
            vec![("a".to_owned(), addr_a), ("b".to_owned(), addr_b)],
            vec!["b".to_owned()],
        )
        .unwrap();
        registry
            .set_forced_hosts(
                ForcedHosts::new(vec![(
                    "forced.example.com".to_owned(),
                    vec!["a".to_owned()],
                )])
                .expect("valid forced hosts"),
            )
            .unwrap();

        let config = Config {
            online_mode: false,
            forwarding_secret: ForwardingSecret::new(vec![0x53; 32]),
            ..Config::default()
        };
        let state = ProxyState::new(Arc::new(config)).with_router(Arc::new(registry));

        let forced = route_backends(&state, "forced.example.com");
        assert_eq!(forced.len(), 1);
        assert_eq!(forced[0].name(), "a");

        let plain = route_backends(&state, "plain.example.com");
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].name(), "b");

        // no router wired in: the answer is an empty list, never a panic
        let bare = ProxyState::new(Arc::new(Config::default()));
        assert!(route_backends(&bare, "forced.example.com").is_empty());
    }
}
