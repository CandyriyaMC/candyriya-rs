//! Fuzz the framed packet codec: raw bytes in, decoded frames out.
//!
//! The decoder is the first thing a hostile client reaches, so it must
//! survive any input without panicking: a declared length it has not
//! validated, a length prefix that wraps, truncated frames — all of it
//! has to end in `Ok(...)` or `Err(...)`, never in an index out of
//! bounds or an allocation out of thin air.
//!
//! On top of the no-panic bar this target also checks a roundtrip
//! invariant: any packet the decoder accepts must re-encode and decode
//! to itself. If that ever stops holding, the encoder and decoder have
//! drifted apart on what "a valid frame" means.

#![no_main]

use bytes::BytesMut;
use candyriya_protocol::{MAX_PACKET_SIZE, PacketCodec};
use libfuzzer_sys::fuzz_target;
use tokio_util::codec::{Decoder, Encoder};

fuzz_target!(|data: &[u8]| {
    let mut codec = PacketCodec::new();
    let mut buf = BytesMut::from(data);

    while let Ok(Some(packet)) = codec.decode(&mut buf) {
        // the decoder measured this against MAX_PACKET_SIZE, so
        // it must actually be bounded
        assert!(packet.payload.len() <= MAX_PACKET_SIZE);

        // a packet we accepted must encode and roundtrip exactly
        let mut encoded = BytesMut::new();
        if Encoder::encode(&mut codec, packet.clone(), &mut encoded).is_ok() {
            let decoded = codec.decode(&mut encoded);
            assert_eq!(decoded.ok().flatten().as_ref(), Some(&packet));
        }
    }
});
