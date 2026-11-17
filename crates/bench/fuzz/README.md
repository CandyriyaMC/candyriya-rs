# candyriya-fuzz

Coverage-guided [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html)
targets for the highest-risk parsers in the workspace — the code that runs on
bytes a hostile client (or a hostile backend) put on the wire. Every target
checks that garbage in ends in a clean `Result` or `Option`, never in a panic,
an out-of-bounds read, or an allocation the declared lengths do not justify.

The targets are part of the workspace so CI can type-check them with a plain
stable toolchain (`cargo check --package candyriya-fuzz`). Actually *running*
the fuzzers needs `cargo-fuzz` and a nightly toolchain; see below.

## Targets

| Target | What it fuzzes |
| ------ | -------------- |
| `packet_codec` | `PacketCodec::decode` on raw bytes: the length prefix, the frame split, and the roundtrip invariant (any packet the decoder accepts must re-encode and decode to itself). |
| `protocol_registry` | `Registry::classify` / `packet_id` / `from_name` / `from_protocol` / `PacketSlot::from_key` on arbitrary state/direction/protocol/packet-id combinations. Panics are never OK; untracked states must come back `Unknown`, and anything resolving as `Unknown(id)` must carry the raw id verbatim. |
| `forwarding` | `verify_forwarding_data`: raw unauthenticated bytes (must die at the HMAC check, never panic) and arbitrary payloads sealed with a known secret (so the field parser itself is exercised on hostile input — truncation, bad booleans, huge property counts, trailing bytes). A secret/payload split derived from the input exercises both arguments at once. |
| `handshake` | `HandshakeRead::push` on adversarial chunkings (seeded xorshift chunk sizes plus a byte-at-a-time drip-feed), including frames followed by pipelined bytes. The 2 MiB cap path must reject before buffering; rejecting paths must never panic. |
| `string_reader` | `read_string` on arbitrary blobs at several caps (0, 1, 16, 255, the format default, plus a cap derived from the input). The reader must never return a string that exceeds its cap or hands out bytes past the declared length. |

Each target lives in `fuzz_targets/<name>.rs` and is a `[[bin]]` of this
crate. No `unsafe` anywhere; the workspace forbids it.

## Running the fuzzers

Requires Linux or macOS (libFuzzer does not run on Windows), a nightly
toolchain and `cargo-fuzz`:

```console
rustup install nightly
cargo install cargo-fuzz
```

Run a target for a fixed time, e.g. 60 seconds:

```console
cargo fuzz run packet_codec -- -max_total_time=60
```

List the available targets:

```console
cargo fuzz list
```

`crates/bench/fuzz` is the crate directory, so from the repository root use
`--fuzz-dir crates/bench/fuzz` or run from inside this directory:

```console
cd crates/bench/fuzz
cargo fuzz run string_reader
```

Corpus and crash artifacts are written to `corpus/` and `artifacts/` next to
this file and are git-ignored. Any crasher found here is a security bug:
report it privately per `SECURITY.md`, not as a public issue.

## Building without cargo-fuzz

`cargo check --package candyriya-fuzz` (what CI runs) only needs a stable
toolchain and a C++ compiler (the `libfuzzer-sys` build script compiles the
bundled libFuzzer runtime). The fuzz targets themselves are ordinary Rust
bins with `#![no_main]`; the libFuzzer entry point comes from
`libfuzzer-sys`.
