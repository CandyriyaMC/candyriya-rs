//! Fuzz the handshake frame accumulator.
//!
//! `HandshakeRead` is the first thing that touches client bytes: it buffers
//! whatever the socket delivers and hands complete frames to the codec. The
//! 2 MiB cap and the error paths exist precisely so hostile input dies here
//! instead of deeper in the pipeline; pushing arbitrary byte soup (in both
//! fixed-size and adversarially-derived chunkings) must never panic.
//!
//! After the feed phase any buffered complete frames are drained with empty
//! pushes, so pipelined second frames are exercised too. The input is
//! capped at a few KiB: handshake frames are tens of bytes in practice, and
//! the 2 MiB cap path is covered by unit tests in the proxy crate.

#![no_main]

use candyriya_protocol::MAX_PACKET_SIZE;
use candyriya_proxy::HandshakeRead;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT: usize = 2048;

fn feed(reader: &mut HandshakeRead, data: &[u8]) {
    // chunk sizes derived from a xorshift64* state seeded by the input, so
    // the same bytes are fed in one chunking per run and different
    // chunkings across runs: 1-byte dribbles, odd sizes, big gulps
    let mut seed = [0_u8; 8];
    seed.copy_from_slice(&data[..data.len().min(8)]);
    let mut state = u64::from_le_bytes(seed) | 1;
    let mut offset = 0;
    while offset < data.len() {
        state ^= state >> 12;
        state ^= state >> 25;
        state ^= state >> 27;
        let span = ((state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 59) as usize) + 1;
        let end = (offset + span).min(data.len());
        match reader.push(&data[offset..end]) {
            Ok(Some(packet)) => assert!(packet.payload.len() <= MAX_PACKET_SIZE),
            Ok(None) => {}
            Err(_) => break, // machine is dead after an error; connection must close
        }
        offset = end;
    }
    // drain any complete frames still buffered behind the first one
    while let Ok(Some(packet)) = reader.push(&[]) {
        assert!(packet.payload.len() <= MAX_PACKET_SIZE);
    }
}

fuzz_target!(|data: &[u8]| {
    let data = &data[..data.len().min(MAX_INPUT)];

    // one pass with derived chunking
    let mut reader = HandshakeRead::new();
    feed(&mut reader, data);

    // one pass byte-at-a-time: the drip-feed case
    let mut reader = HandshakeRead::new();
    for byte in data {
        match reader.push(std::slice::from_ref(byte)) {
            Ok(Some(packet)) => assert!(packet.payload.len() <= MAX_PACKET_SIZE),
            Ok(None) => {}
            Err(_) => break,
        }
    }
    while let Ok(Some(packet)) = reader.push(&[]) {
        assert!(packet.payload.len() <= MAX_PACKET_SIZE);
    }
});
