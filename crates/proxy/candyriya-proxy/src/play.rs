//! The play-state hot path.
//!
//! Once the Login Success frame has been written to the client, the proxy
//! stops parsing the session entirely and becomes a byte pump between the
//! two TCP streams. This module owns that phase, in full:
//!
//! * **No framing, no parsing.** Play-state bytes are copied verbatim.
//!   Every transform that matters (AES-128-CFB8, zlib) is applied or
//!   bypassed at the byte level, so play traffic needs no packet ids and
//!   no per-version tables here — 1.21-era traffic works unchanged.
//! * **What gets transformed.** The client link is the only one the proxy
//!   terminates for crypto: in online mode it carries AES-128-CFB8 (the
//!   proxy ran its own encryption handshake with the client during login),
//!   and the feedback chain continues into the play state untouched — the
//!   cipher handed over from the login phase is split into its two
//!   independent halves, one per copy direction, through
//!   [`candyriya_encryption::AesCfb8::into_halves`]. The backend link never
//!   carries a cipher, so nothing on that side needs one either.
//! * **What passes through untouched.** zlib compression, when the backend
//!   negotiated it (Set Compression was observed during login and forwarded
//!   to the client), is applied symmetrically by the client and the
//!   backend with the same threshold. The two endpoints are then talking
//!   to each other through the proxy; re-framing or re-compressing here
//!   would only risk desync, so the bytes are copied as-is. A raw copy is
//!   therefore correct with or without compression, with or without a
//!   cipher — only the cipher needs proxy-side handling.
//! * **Backpressure.** Each direction owns one 8 KiB buffer and one
//!   `write_all` bounded by a stall budget. If the receiving side stops
//!   draining, the write stalls and the session is torn down instead of
//!   buffering without bound. A congested session never grows past the
//!   kernel socket buffers plus two 8 KiB chunks.
//! * **Half-close.** A read EOF shuts down the *write* half of the other
//!   socket (FIN is propagated) and the reverse direction keeps copying
//!   until it hits its own EOF or an error; only then does the pump stop.
//!   When either direction errors or stalls, the other is dropped together
//!   with it and both sockets close with the task.
//!
//! Out of scope: switching the backend link to another server
//! (`/candyriya switch`). That flow needs a fresh backend connection and a
//! re-wired pump, which is the switch agent's slice; nothing here knows
//! about it. Metrics are likewise not touched here — the metrics agent
//! owns the counter names and their bookkeeping.
//!
//! The `[advanced] read-timeout` key is reused as the write-stall budget:
//! it is the only established-connection knob the config has, and its
//! documented meaning ("milliseconds without data before an established
//! connection is dropped") covers a peer that stopped draining.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use candyriya_encryption::{Cfb8DecryptHalf, Cfb8EncryptHalf};

use crate::wire::Wire;

/// Size of one copy buffer. Matches the 8 KiB `tokio::io::copy_bidirectional`
/// uses internally — enough to saturate loopback/LAN links without holding
/// meaningful memory per connection. Not allocated up front: each pump owns
/// a stack array that lives as long as the copy loop.
const PUMP_BUF: usize = 8 * 1024;

/// The cipher transform applied to a chunk traveling one way.
///
/// Only the client link is proxy-terminated; the two variants exist so one
/// pump cannot accidentally encrypt a chunk that was meant for the backend
/// (or decrypt one addressed to the client).
enum Transform {
    /// Client -> backend: decrypt through the client-link receive half.
    Decrypt(Cfb8DecryptHalf),
    /// Backend -> client: encrypt through the client-link send half.
    Encrypt(Cfb8EncryptHalf),
}

impl Transform {
    fn apply(&mut self, chunk: &mut [u8]) {
        match self {
            Transform::Decrypt(half) => half.decrypt_in_place(chunk),
            Transform::Encrypt(half) => half.encrypt_in_place(chunk),
        }
    }
}

/// Pump bytes one way until EOF, a stall or an error.
///
/// `reader` is drained in [`PUMP_BUF`]-sized chunks; each chunk goes through
/// `transform` (when the session is encrypted) and is written to `writer`.
/// The write is bounded by `write_stall`: a peer that stopped draining gets
/// at most that long to accept one chunk before the session is declared
/// lost. On a clean read EOF the write half is shut down so the far end
/// observes the FIN, and the byte count is returned.
///
/// The counter is monotonic per call; a direction that never finishes
/// never reports a count.
async fn pump_one_way<R, W>(
    mut reader: R,
    mut writer: W,
    mut transform: Option<Transform>,
    write_stall: Duration,
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; PUMP_BUF];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            writer.shutdown().await?;
            return Ok(total);
        }
        let chunk = &mut buf[..n];
        if let Some(transform) = &mut transform {
            transform.apply(chunk);
        }
        match timeout(write_stall, writer.write_all(chunk)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "peer stopped reading; losing the session",
                ));
            }
        }
        total += n as u64;
    }
}

/// Run the play-state pump until both directions close, or one fails.
///
/// `client_wire` carries the client-link transforms; its cipher (when the
/// online-mode encryption handshake established one) is consumed and split
/// into the two copy directions. zlib is never re-framed here — see the
/// module docs. The function owns no connection state beyond its
/// arguments: when it returns, the caller drops both sockets and both
/// peers observe the close.
///
/// `write_stall` bounds each single chunk write; hitting it tears the
/// session down (both directions), it is not retried.
pub(crate) async fn run_play_state(
    client: &mut TcpStream,
    backend: &mut TcpStream,
    client_wire: &mut Wire,
    conn_id: u64,
    peer: SocketAddr,
    write_stall: Duration,
) {
    let compression = client_wire.compression_enabled();
    let (to_backend, to_client) = match client_wire.take_cipher() {
        Some(cipher) => {
            let (encrypt, decrypt) = cipher.into_halves();
            (
                Some(Transform::Decrypt(decrypt)),
                Some(Transform::Encrypt(encrypt)),
            )
        }
        None => (None, None),
    };

    let (client_read, client_write) = client.split();
    let (backend_read, backend_write) = backend.split();
    let client_to_backend = pump_one_way(client_read, backend_write, to_backend, write_stall);
    let backend_to_client = pump_one_way(backend_read, client_write, to_client, write_stall);

    // Both pumps are polled in this one task — no spawn, no channel. The
    // first error drops the other pump on the floor and ends the session;
    // dropping it mid-write cannot corrupt the wire because the socket
    // closes together with the task. On the success path both directions
    // reached EOF with the FIN propagated already.
    match tokio::try_join!(client_to_backend, backend_to_client) {
        Ok((client_to_backend_bytes, backend_to_client_bytes)) => {
            info!(
                conn = conn_id,
                %peer,
                client_to_backend_bytes,
                backend_to_client_bytes,
                compression,
                "play state ended"
            );
        }
        Err(error) => {
            debug!(conn = conn_id, %peer, %error, "play state ended with an error");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candyriya_encryption::{AesCfb8, SharedSecret};
    use tokio::io::duplex;

    #[tokio::test]
    async fn pump_copies_bytes_and_propagates_eof() {
        let (mut sender, pump_reader) = duplex(64);
        let (pump_writer, mut receiver) = duplex(64);
        sender.write_all(b"hello candyriya").await.unwrap();
        drop(sender); // the pump's read side must observe EOF

        let bytes = pump_one_way(pump_reader, pump_writer, None, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(bytes, 15);

        let mut got = Vec::new();
        receiver.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"hello candyriya");
    }

    #[tokio::test]
    async fn pump_write_stall_times_out() {
        let (mut feed, pump_reader) = duplex(1024);
        let payload = vec![0xAB; 512];
        feed.write_all(&payload).await.unwrap();
        drop(feed);
        // an 8-byte pipe that nobody drains: the 512-byte chunk written by
        // the pump fills it instantly and then stalls forever
        let (pump_writer, _keep) = duplex(8);
        let error = pump_one_way(pump_reader, pump_writer, None, Duration::from_millis(50))
            .await
            .expect_err("the write must stall and time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn pump_decrypts_what_the_client_encrypted() {
        const SECRET: [u8; 16] = [0x42; 16];
        let plaintext = b"the quick brown fox jumps over";
        let mut client_side = AesCfb8::new(&SharedSecret::from_bytes(SECRET));
        let mut wire_bytes = plaintext.to_vec();
        client_side.encrypt_in_place(&mut wire_bytes);

        let (mut feed, pump_reader) = duplex(1024);
        feed.write_all(&wire_bytes).await.unwrap();
        drop(feed);

        let (pump_writer, mut receiver) = duplex(1024);
        let proxy_side = AesCfb8::new(&SharedSecret::from_bytes(SECRET));
        let (_encrypt, decrypt) = proxy_side.into_halves();
        let bytes = pump_one_way(
            pump_reader,
            pump_writer,
            Some(Transform::Decrypt(decrypt)),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(bytes, plaintext.len() as u64);

        let mut got = Vec::new();
        receiver.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, plaintext);
    }

    #[tokio::test]
    async fn pump_encrypts_what_the_client_expects() {
        const SECRET: [u8; 16] = [0x77; 16];
        let (mut feed, pump_reader) = duplex(1024);
        feed.write_all(b"server says hello").await.unwrap();
        drop(feed);

        let (pump_writer, mut receiver) = duplex(1024);
        let proxy_side = AesCfb8::new(&SharedSecret::from_bytes(SECRET));
        let (encrypt, _decrypt) = proxy_side.into_halves();
        pump_one_way(
            pump_reader,
            pump_writer,
            Some(Transform::Encrypt(encrypt)),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        let mut wire_bytes = Vec::new();
        receiver.read_to_end(&mut wire_bytes).await.unwrap();
        let mut client_side = AesCfb8::new(&SharedSecret::from_bytes(SECRET));
        client_side.decrypt_in_place(&mut wire_bytes);
        assert_eq!(wire_bytes, b"server says hello");
    }
}
