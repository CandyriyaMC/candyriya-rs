---
layout: default
title: Architecture
---

# Architecture

This page catalogues every crate in the Candyriya workspace: what it is
for, what it depends on, and how bytes move through the system. It is a
reference for people reading the code base, not an introduction to the proxy
— for running the proxy, see the [installation guide](installation).

## How to read this page

Candyriya is one product split into many packages. Every crate named
`candyriya-*` is a separate package with its own `Cargo.toml`, its own tests
and its own release cadence — but they are versioned and developed together,
and none of them is useful or supported on its own. Think of the workspace
as a single binary assembled from interchangeable, individually replaceable
parts, not as a set of libraries.

Three conventions hold across the whole tree:

- **One concern per crate.** A crate owns exactly one concern (packet framing,
  compression, permission resolution). Cross-cutting utilities go to
  `candyriya-utils`; nothing else is shared privately.
- **Acyclic dependencies.** Dependencies between workspace crates form a DAG.
  A crate may depend on crates that are strictly below it in the layering
  described after the diagram, never on a crate above it, and never in a
  cycle.
- **No unsafe, no fakes.** The workspace forbids `unsafe` code at the
  compiler level (`unsafe_code = "forbid"` in the shared lints). There are no
  mock objects in tests; the integration harness (`candyriya-it`) speaks the
  real protocol over loopback. Where a slice is unfinished, the docs say so
  plainly (see [where the build stands](#where-the-build-stands)) instead of
  implying it works.

Crates are listed below in layer order: foundations first, then data models,
protocol, messaging, identity, the proxy core, commands, and the binary.
Each entry names its purpose and its place in the graph.

## The connection pipeline at a glance

```text
client connects
      │
      ▼
accept loop ───────── cap: global semaphore + per-IP connection limiter
      │
      ▼
handshake frame read  cap: 2 MiB frame, intent ∈ {status, login, transfer},
      │              address charset check, handshake timeout
      ▼
routing decision on intent
      ├── intent = status ──► status path (ping/MOTD, protocol echo)
      ├── intent = login  ──► login path (auth → forwarding → backend)
      └── intent = transfer ─► transfer path (1.20.5+, only when configured)
      │
      ▼ (login path)
authentication ─────── offline: name-derived identity
      │                  online: RSA key exchange + Mojang session check
      ▼
compression + forwarding ── Set Compression, then modern (HMAC-signed),
      │                      bungeeguard or legacy forwarding to the backend
      ▼
routing to backend ─────── forced-hosts table → try chain → health states
      │
      ▼
hot path ──────────────── client ↔ backend byte splice, proxy stays out
      │
      ▼
shutdown ──────────────── scheduler drain, accept loop stops first,
                          in-flight work finishes to a deadline, report
```

The diagram is the intended data flow of a connection. Some stages are
implemented and unit-tested but not yet wired into a single end-to-end run;
see [where the build stands](#where-the-build-stands) for the honest
status of each stage.

## What happens to a connection

**Accept.** The listener (a `socket2`-backed TCP socket with the production
socket options) feeds one accept-loop task, registered under a stable name
with the task service. Before a connection reaches any protocol code, a
per-IP concurrent-connection cap (`candyriya-ratelimit`) and a global cap
(a semaphore derived from the configured connection limit) allocate it a
slot; a connection that cannot get a slot is closed immediately. One task
per connection, named by a monotonic connection id, no sharing of connection
state between tasks.

**Handshake read.** The first frame is read incrementally and validated
before any other packet is looked at: the 2 MiB frame cap is enforced before
the bytes ever reach the codec, the packet id must be the handshake id,
the intent must be status, login or (1.20.5+) transfer, the protocol number
is recorded and compared against the tracked maximum (a newer client is a
warning, not a rejection), and the address is checked against a safe hostname
set. Any failure closes the connection without a reply beyond what the
protocol allows.

**Routing decision.** The intent decides the path. Status serves the server
list: a status response echoing the client's protocol version back (the
proxy never translates versions), the configured MOTD and favicon, then a
ping/pong pair. Login starts the authentication flow. Transfer ends at this
proxy and hands the player to another trusted proxy; it is only honored
when the network enables it.

**Authentication.** Offline mode derives the UUID from the player name and
trusts it. Online mode runs the real exchange: the proxy sends an
encryption request carrying its RSA-2048 public key, the client answers with
the shared secret and verify token encrypted to that key, the proxy
decrypts them and checks them, then confirms the session with Mojang's
`hasJoined` endpoint. The secret and token are never logged, and nothing on
the wire crashes the pre-login path on bad input.

**Compression and forwarding.** After authentication, both directions switch
to the compressed framing if a Set Compression packet was negotiated. When
the connection reaches the backend, the backend (which runs in offline mode)
asks the proxy for the player's identity over the login plugin channel; the
proxy answers with an HMAC-SHA256-signed payload describing the client
address, UUID, name and properties. Backends authenticate that payload with
the shared forwarding secret before parsing it; without the secret they
cannot forge the login. Older backends can use the legacy scheme (identity
smuggled into the handshake address, optionally with a BungeeGuard token).

**Backend routing.** Which backend receives the connection is decided from
the address the client used: the forced-hosts table maps hostnames to
backends (with single-label wildcards), everything else falls through to the
`try` list, in declaration order. Backends know whether they are healthy
through a real status-ping health check driven by the task service. A
backend that kicks or drops the player during login walks the chain; when
the chain runs out, the player is disconnected with the configured reason.

**The hot path.** Once both sides are connected, the proxy stops interpreting
packets. It splices the two sockets together and lets the bytes flow; the
proxy's job during play is bookkeeping (connection ids, metrics, logging
hooks), not packet inspection. The splice is designed around pooled buffers
and bounded channels — backpressure by construction, no unbounded queues
anywhere.

**Shutdown.** A stop signal first closes the accept loop (in-flight
connections keep their slots), then the task service drains: perpetual
watchers are cancelled, finite work is awaited to a deadline, and the
process reports what completed, what timed out and what panicked. Nothing
is killed silently.

## The crates

### Foundations

**candyriya-utils** — shared small primitives with no runtime of their own:
UTF-8-safe truncation, hex encode/decode, a non-empty-string newtype, an
atomic file write (temp file, `sync_all`, rename, with best-effort cleanup),
cryptographic random tokens and human-readable formatting. Every other
crate may use it; it depends on nothing in the workspace.

**candyriya-i18n** — the locale loader for all user-facing text. Two embedded
locales (English as the default and fallback, plus a Russian locale), a
fallback chain (active locale → plugins → English → key), hot reload through
`arc-swap`, and a registration hook so plugin locales merge into the same
machine. Values are MiniMessage strings; lookup never fails loudly — a
missing key resolves to the key itself with a one-time warning.

**candyriya-eventbus** — a typed in-process event bus. Handlers are registered
by event type, `post` delivers synchronously, panicking handlers are caught,
logged and counted, and subscriptions remove themselves on drop. The bus is
deliberately small; it is the seam between subsystems that must not reference
each other directly.

**candyriya-metrics** — the process-wide Prometheus install. Any crate records
through the `metrics` facade; this crate owns the one-time global setup and
the shared metric name constants so the whole proxy agrees on one naming
scheme. Disabled install is a no-op that never consumes the one-shot slot;
enabled install binds the HTTP listener synchronously so a busy port is
reported, not discovered later.

**candyriya-telemetry** — the tracing subscriber. Compact text logs, UTC
RFC 3339 timestamps, thread ids, optional daily-rotating file sink with a
bounded non-blocking writer, `NO_COLOR` and TTY detection, and an optional
OpenTelemetry export behind a cargo feature that is off by default. Double
initialization is an error, never a panic.

**candyriya-scheduler** — the single owner of all execution contexts. Every
delayed, repeating, cron-scheduled or background piece of work in the system
goes through this crate's task service: named async tasks on the caller's
runtime, blocking work on a fixed `rayon` pool, scoped plugin tasks with
hard concurrency/queue quotas and per-task timeouts, per-task metrics and
panic containment, cooperative cancellation, and a graceful `drain` with a
deadline. Core crates register with it; nothing spawns ad hoc.

**candyriya-cache** — the only caching surface. A thin, opinionated wrapper
over `cachegirl`: one root under the data directory, two namespace families
(`cache/…` and `temp/…`) for core and each plugin, byte and resource quotas
with atomic enforcement at creation, a pooled buffer arena for hot paths,
and cleanup passes invoked on the scheduler — never from inside the crate.
No hand-rolled TTL maps exist anywhere in the proxy.

### Protocol and wire formats

**candyriya-protocol** — the pure wire-format primitives: VarInt/VarLong
coding, length-prefixed UTF-8 strings (with the UTF-16 code-unit cap), the
two-longs UUID format, and a `tokio_util` codec enforcing the 2 MiB packet
cap and frame splitting. It knows nothing about versions or networking.

**candyriya-protocol-registry** — the data-driven version tables, covering
stable releases from 1.7.2 through the 26.x line. Embedded snapshots ship in
the binary, an override directory can replace them wholesale, and an
optional network refresh fetches a validated newer table at startup. It
maps a handshake protocol number to the packet ids the proxy must recognize
for status and login; everything else is deliberately out of scope.

**candyriya-compression** — the zlib manager for the compressed framing:
threshold and level handling, frame encode/decode, the declared-length cap
checked before any inflation (zip-bomb safe), and the 3-byte length-prefix
limit from the vanilla spec. Pure data in, data out; `flate2` with the pure
Rust `miniz_oxide` backend.

**candyriya-encryption** — the cryptographic half of the login key exchange:
RSA-2048 keypairs (public key distributed as SPKI/X.509 DER), the 16-byte
shared secret (zeroized on drop), the AES-128-CFB8 cipher pair with
continuous state, and the session-server `serverId` hash exactly as the
protocol defines it. No network code; framing belongs to
`candyriya-protocol`.

**candyriya-messaging** — the protocol-agnostic core for plugin messaging:
a byte-exact codec for Java's modified UTF-8 (CESU-8) as used by the
`BungeeCord` channel, a channel identifier model, and the plugin-message
envelope. It contains no network, no async and no unsafe.

**candyriya-messaging-bungeecord** — the BungeeCord wire format, 1:1: the
18 sub-channels with their exact request and response encodings, built on
the Java codec above. It exists so that plugins and backends speaking the
legacy channel work unmodified.

**candyriya-messaging-velocity** — Velocity's plugin messaging semantics
as types: the 4-case event model (player→proxy, proxy→backend any, backend→
proxy, proxy→player specific), `ForwardResult` handled/forward, the channel
registrar, and the BungeeCord compatibility table. No packets are written
here; it models what the proxy does with them.

**candyriya-messaging-candyriya** — the proxy's own channel, `candyriya:main`:
a versioned v1 binary protocol with VarInt-framed strings (not CESU-8), a
documented packet table for serverbound and clientbound directions, and
strict direction pairing. This is the native surface for plugins that know
they are talking to Candyriya.

### Identity and persistence

**candyriya-auth** — server-side authentication for the login flow. Offline
mode is a one-liner (deterministic identity from the name). Online mode is
the real thing: `GET` against Mojang's `hasJoined` endpoint with a timeout,
strict response interpretation, properties passed through verbatim. It never
sees a password; it verifies a session the client already holds with
Microsoft.

**candyriya-player** — the domain model of identities: the game profile and
its properties, the offline-UUID derivation, the live session type (with its
own login/configuration/play connection states), the historical record type,
and the storage trait the store crate implements. No I/O.

**candyriya-player-store** — file-backed persistence for the historical
records: one JSON file, atomic rewrite on every save (temp file +
`sync_all` + rename), validated on open (size cap, per-record decode,
duplicate UUIDs are a hard error), human-readable format. Single-process
ownership; multi-process writers are explicitly out of scope.

### Configuration

**candyriya-config** — the validated TOML configuration. Layered (embedded
annotated defaults, user file on top), forgiving of unknown keys, unforgiving
of garbage, with all detectable problems reported together at load. The
forwarding secret is resolved relative to the configuration file, generated
on first run with owner-only permissions, never rotated implicitly, and
never logged. Hot reload swaps atomically through `arc-swap`; a failed
reload leaves the running configuration intact. See
[Configuration](configuration) for the full reference.

### Permissions

**candyriya-permissions** — the proxy's own permission engine, not a port of
another project's: validated node newtype with wildcard semantics, contexts,
allow/deny grants, a node registry with defaults, and a resolver implementing
specificity ranking with deny-wins on exact-length ties and default-deny as
the floor. Resolution is a pure function; caching is deliberately left to
the layer above. See [Permissions](permissions) for the full reference.

**candyriya-permissions-config** — the default, config-backed provider built
on the engine: it turns the `[permissions]` section of `candyriya.toml`
into the flat effective grant list the resolver consumes, validating every
node in one pass, assembling the implicit all-subject group, group grants
and (when constructed from parts) per-player assignments. It never resolves
decisions itself.

### Routing and limits

**candyriya-forwarding** — the two identity-forwarding codecs the proxy
speaks to backends: modern (HMAC-SHA256-signed payload over the
`velocity:player_info` plugin channel, byte-compatible with the contract
Velocity defines and Paper verifies) and legacy (identity inline in the
handshake address, with the optional BungeeGuard token). Pure byte codecs;
all parsers authenticate before parsing.

**candyriya-route** — backend resolution and health: the backend registry
(name → address, atomic state tracking), the `try` fallback chain, the
forced-hosts table with single-label wildcard matching, and a real Minecraft
status ping used by health checks. It never spawns tasks; the caller drives
it from the scheduler.

**candyriya-ratelimit** — the DDoS resilience primitives: token buckets for
handshake/login rate limiting, a fixed-window counter per key, and a
connection limiter combining a global cap with a per-IP cap behind an RAII
guard. `std`-only by design; the time source is injected for testable
logic.

### Plugins

**candyriya-plugin-api** — the ABI contract between the proxy and WebAssembly
plugins, types only, no runtime: the proxy version handshake, the
`plugin.toml` manifest model with full validation, the typed event model
(kinds, envelopes, cancellation), component construction via the project's
component library, the strict MiniMessage acceptance gate, and the title /
boss bar / tab list packet bodies (payloads only — the proxy resolves the
packet ids from its own per-version registry and writes them itself).
Everything crossing the boundary is JSON bytes of these types.

**candyriya-plugin** — the host runtime: directory discovery across `plugins/`
and `libraries/`, manifest parsing and duplicate-id detection, the dedicated
named OS thread every plugin gets (`candyriya-plugin-<id>`), a bounded
mailbox for calls with a receive timeout, panic containment at the FFI
boundary, per-plugin records and lifecycle state, and the load report of
issues as `(where, why)` tuples. Plugins never run on the async workers.
(The dependency resolver, host functions, resource limits and hot reload
build on this crate in later milestones.)

### The proxy core

**candyriya-proxy** — the connection pipeline itself: the bind with the
production socket options (`SO_REUSEADDR`, `SO_KEEPALIVE`, `TCP_NODELAY`,
optional `SO_REUSEPORT`), the accept loop with its global and per-IP caps,
the connection task lifecycle, the incremental handshake state machine with
its hard limits and its validation, and the shutdown contract with the task
service (flip the signal, the accept loop drops, in-flight work drains).
The routing decision, the login pipeline and the hot-path splice land as
their own slices and are tracked in the build status below.

### Commands

**candyriya-command** — the built-in command set and its brigadier wiring.
The dispatcher is a thin adapter over azalea-brigadier: it validates
literals, attaches permission gates at the tree level (the `/candyriya`
subcommands `info`, `servers`, `switch`, `end`, `reload`, `ps`, `tree` are
gated; the player-facing commands `/glist`, `/send`, `/server` and `/goto`
are ungated, Velocity-style), and dispatches to pure command
implementations that read the world through the `NetworkDirectory` trait
and mutate it through `ProxyEffects`. Output
is MiniMessage lines through an `OutputSender` seam; the crate has no
network, no async and no I/O. The upcoming `/candyriya plugin` surface
(phase 2 of the native-plugins epic) is specified in
[Commands](commands#candyriya-plugin) and gated by the
`candyriya.command.plugin.*` nodes; the running tree does not carry it yet.

### Composition root

**candyriya (the binary)** — the composition root: `clap` argument parsing,
tracing initialization, metrics and OpenTelemetry wiring, the cron/background
jobs registered on the task service, and the construction order of every
subsystem. Its `main` is intentionally thin; the wiring lands crate by
crate. This is the only crate that composes the workspace, and nothing
depends on it.

### Verification and benchmarks

**candyriya-it** — the integration harness: a real server-side TCP backend
implementing the handshake, status and login flow with the real wire codec,
and a real client for driving it. Used by integration tests to verify
behavior byte for byte; no mocks, no stubs, loopback only.

**candyriya-bench** — criterion benchmarks for the hot paths: VarInt codec,
packet roundtrip, registry lookup, buffer pool, forwarding payload creation.
Not a runtime dependency; a measurement tool.

**candyriya-fuzz** — cargo-fuzz targets for the parsers: packet codec,
protocol registry, forwarding verify, handshake state machine, string
reader. Not part of the build; the CI checks that the targets compile.

## Direction rules

Dependencies between workspace crates flow in one direction, from leaves to
the root:

1. **Leaf crates** depend on nothing in the workspace (`candyriya-protocol`,
   `candyriya-protocol-registry`, `candyriya-compression`,
   `candyriya-encryption`, `candyriya-permissions`, `candyriya-utils`,
   `candyriya-i18n`, `candyriya-eventbus`, `candyriya-metrics`,
   `candyriya-telemetry`, `candyriya-scheduler`, `candyriya-cache`,
   `candyriya-ratelimit`, `candyriya-messaging`, `candyriya-config`).
2. **Mid-layer crates** depend on leaves: `candyriya-player` ← registry,
   `candyriya-auth` ← player, `candyriya-permissions-config` ← permissions,
   `candyriya-player-store` ← player, `candyriya-forwarding` ← protocol +
   player, `candyriya-route` ← protocol, `candyriya-messaging-bungeecord` ←
   messaging, `candyriya-messaging-velocity` ← messaging + bungeecord,
   `candyriya-messaging-candyriya` ← messaging + protocol,
   `candyriya-plugin-api` ← protocol, `candyriya-plugin` ← plugin-api +
   cache, `candyriya-command` ← permissions + utils.
3. **The proxy core** depends on mid-layer and leaves: `candyriya-proxy` ←
   config, protocol, registry, ratelimit, scheduler. (As more slices land it
   will pull in auth, forwarding, route, player and the messaging crates —
   always in this direction, never upward.)
4. **The composition root** depends on whatever it composes:
   `candyriya` ← proxy, config, metrics, telemetry, scheduler, command.
   Nothing depends on it.
5. **Tooling** depends on libraries: `candyriya-it` ← protocol + registry,
   `candyriya-bench` ← protocol, registry, compression, cache, forwarding,
   player; `candyriya-fuzz` ← protocol, registry, forwarding, proxy. Nothing
   depends on tooling.

Two extra rules keep the graph honest:

- **No cycles, ever.** If a proposed dependency would close a loop, the new
  code belongs in a crate lower down, or in `candyriya-utils`. The shared
  lints cannot enforce this mechanically, so it is enforced by review and by
  keeping each crate small enough to audit.
- **Public API is the only surface, and it is documented.** Intra-workspace
  dependencies go through crates' public items only. `pub(crate)` is an
  implementation detail that reaches no further than its own crate.
  Plugin-facing types (the `candyriya-plugin-api` shapes) are the only types
  that cross the process boundary; everything else is internal to the
  product.

## Design notes

**100% safe Rust.** The workspace forbids `unsafe` code at compile time.
Where the platform cannot be reached through a safe API — notably TCP Fast
Open — the option is skipped with a warning instead of reaching for `unsafe`.
Dependency policy (`cargo deny`, advisory scanning) is being wired into CI;
internal policy is no Java ports, no hand-rolled crypto primitives, no
hand-rolled TTL maps.

**No stubs in what ships.** The crates merged so far implement what they
declare. The test harness is a real backend and a real client; the only
stand-ins are in benchmarks, and they are labeled as such. The one unfinished
part — the composition root's wiring — is called out in the build status
section instead of being papered over; a crate that is only partly wired
into the running proxy is documented as such, never hidden behind a
"works" claim.

**Compatibility is a contract, not an implementation.** The proxy speaks the
vanilla protocol directly and re-implements, from the published wire
formats, the byte-level contracts of the ecosystem: Velocity's modern
forwarding, the BungeeCord plugin-messaging format (including its Java
modified UTF-8 strings), BungeeGuard's token property, and Velocity's
plugin-messaging semantics. These are re-implemented natively — no Java
code runs anywhere, no classes are mirrored — and are verified in tests
against golden vectors. On the wire, a 1.21 client talks to a 1.21 backend
without any translation layer inside the proxy; translation, when needed,
lives on the backends.

**Runtime model.** Tokio drives all async I/O; the blocking pool (fixed size
from the config, default the CPU count) absorbs CPU-heavy work such as
compression, crypto and batch (de)serialization; the scheduler owns both and
never lets either be spawned ad hoc. The event bus is synchronous and
panic-contained; `arc-swap` gives lock-free reads of the running
configuration; bounded channels are mandatory wherever bytes flow between
subsystems.

**The plugin execution model.** Every plugin runs on its own dedicated,
named OS thread, never on Tokio workers. Calls cross a bounded mailbox with
a receive timeout; a full mailbox is an error, not a blocked worker; a
panicking plugin is caught at the boundary, logged, counted and either
recovered or kicked from the pool. The core never waits on plugins, and
plugin tasks run at low priority with strict quotas.

**Thread ownership, explicitly.** One accept-loop unit; one unit per
connection; one exporter thread for the metrics HTTP listener; one dedicated
thread per loaded plugin; blocking work only on the scheduler's fixed pool;
and a daily-rotating file writer thread for logs. Everything else is async
or eliminated by design. Names in the logs and the `ps` table are unit
names — `conn-<n>`, `proxy.accept`, `plugin-<id>` — not thread names; the
mapping from a unit to the task service that runs it is noted under
[the supervision tree](#the-supervision-tree-and-the-ps-surface).

**Text and rendering.** All user-facing strings come from the embedded
locales and are MiniMessage-formatted; the MOTD, kick and disconnect
reasons are parsed at the boundary before being serialized into packets.
Unknown tags are rejected before they reach the wire; untrusted text is
escaped before interpolation. The proxy never hand-builds protocol JSON or
`§`-codes.

**Caching.** All cache-like state lives behind `candyriya-cache` (cachegirl),
with per-plugin namespaces and quotas, an atomic lease model and a scheduled
cleanup sweep. The buffer pool is not quota-accounted; accounted resources
are. There is intentionally no permissions resolver cache today — see
[Permissions](permissions) for why.

## The supervision tree and the `ps` surface

Everything the proxy runs is a unit in one supervision tree, rooted at
`init` (pid 1). Units carry a pid, a parent, a name, a kind
(`oneshot`, `long-lived`, `every`, `connection`, `plugin`) and a lifecycle
state; exited units stay visible as zombies until the reaper harvests
them, and children of a dying unit re-parent to `init`. The tree is the
source of truth for what exists — not a side channel. Services
(`config`, `cache`, `route`, `host`, `proxy.accept`, `stats.every-60s`,
`players`, `plugins`, `console`, …) are children of `init`; the loader
phase before the tree exists is adopted as already-exited `oneshot`
bootstrap markers.

Two subtrees carry the high-churn kinds:

- **`players`** is the group node for live sessions. Each accepted
  connection runs as a `conn-<n>` unit under it, with a cooperative park
  gate (freeze) and per-connection byte counters. Shutdown cascades
  accept → players → connections.
- **`plugins`** is the group node for the plugin subsystem. Each loaded
  plugin runs as a `plugin-<id>` unit; an unrecoverable failure re-stages
  the plugin with bounded backoff and lands in `FailedTerminal`, which
  stays visible rather than vanishing.

`/candyriya ps` renders the process table from a snapshot of the tree
(permission node `candyriya.command.ps`; the console holds `*`). The
default view collapses connection units into one `conn × N` row carrying
the count, byte sums, the worst state present, the restart sum and the
oldest uptime; every other unit gets its own row. `/candyriya ps --full`
expands everything. `/candyriya tree` prints the same information as a
pure indented tree with box-drawing guides, names and brief captions.
Single-letter states: `R` running, `F` frozen (park gate closed, a
cooperative freeze — TCP backpressure does the rest), `Z` zombie, `t`
restarting, `!` failed-terminal. Columns: `PID PPID NAME STATE RESTARTS
UPTIME RX TX LAST EXIT`. `RX`/`TX` are the connection byte counters;
`UPTIME` is the unit's own run time. There is no CPU or RSS column and
never will be — the tree does not measure them. The output ends with a
summary line (`units: N (conn: M, plugins: K) · zombies: A (stuck: B)`)
computed from the tree's own counters. `/candyriya ps --watch` repaints
the same table every second while the console is open; any line read
(Enter) or Ctrl-C ends the watch, and one final ordinary frame is
printed on exit. On a terminal each repaint homes the cursor and erases
below it (the screen is cleared once, at watch entry); with
`NO_COLOR`/plain output the frames are printed newline-separated and
carry no escape bytes. The watch is console-only — other sources see
the ordinary one-shot table. All user-facing strings in the commands go
through the embedded locales, en and ex_girl; the rendering itself is
MiniMessage and honors the console's color mode.

## The native plugin ABI (phase 1)

Phase 1 of the native plugin epic lands the contract, nothing else: the
**candyriya-native-abi** crate (`crates/common/candyriya-native-abi`) —
the `extern "C"` boundary every native plugin is built against. It carries
versioned `#[repr(C)]` structs, an opaque instance handle, `(ptr, len)` byte
buffers and the `CandyriyaPluginApiV1` table the proxy hands out with every
entry point; V1 deliberately carries no host functions, so every proxy→plugin
call travels through the plugin's own exports and every plugin→proxy call is
defined by a later ABI version. `crates/plugins/greeting` is the reference
plugin: a platform-agnostic `cdylib` (`.so`/`.dll`/`.dylib`) that exports
`candyriya_plugin_init` / `candyriya_plugin_on_load` /
`candyriya_plugin_on_enable` and prints one line on enable. The crate
README is the normative document: the call flow, the buffer rules, the
versioning rules (V1 is frozen; extensions arrive as new table types with
`abi_version` at offset 0, never as edits to V1) and how to write a new
plugin. Nothing in the tree calls the ABI yet — the Plugin Controller that
dlopens plugins and drives the state machine through the supervision tree
is phase 2, ABI API expansion is phase 3, and WASM plugins run in parallel
and stay until phase 4. Unsafe code in the workspace remains forbidden
outside the two ABI crates, where the opt-out is explicit and scoped.

## A login end to end

Follow one player joining an offline-mode network whose backends speak
modern forwarding. The bytes travel exactly as described here; no step is
skipped and no layer is elided.

1. The client opens a TCP connection. The accept loop grants it a slot
   (per-IP and global caps), assigns it connection id `N`, and hands the
   socket to a per-connection task. The log records `conn=N peer=…`.
2. The client sends the handshake frame. The handshake state machine reads
   it incrementally: frame length checked against the 2 MiB cap, packet id
   must be `0x00`, intent must be login (`2`), the address passes the
   hostname charset check, and the timeout — `[advanced] handshake-timeout` —
   has not expired. Any violation closes the connection silently.
3. The login state begins. The client sends Login Start with its name. In
   offline mode there is no network round-trip: the UUID is derived from
   the name, and the session type records the negotiated protocol version.
4. The proxy negotiates compression if the threshold is not `-1`: both
   sides switch to the two-VarInt framing. From here on, every packet is
   length-checked before inflation; a hostile frame cannot make the decoder
   expand past the declared length.
5. The proxy has to reach a backend, so it must decide where first: the
   handshake address is matched against the forced-hosts table (exact,
   wildcard, or catch-all, in rank order). No match falls through to the
   `try` list. Both paths end at a backend name resolved through the
   registry.
6. The proxy connects to that backend and completes its own handshake and
   the offline login there. During login the backend asks for the player's
   identity over the login plugin channel; the proxy answers with the
   forwarding payload: a 32-byte HMAC-SHA256 signature over a versioned
   body containing the client address (no port), the UUID, the name and the
   properties — signed with the shared forwarding secret.
7. The backend verifies the signature before touching the body (anything
   else is rejected outright), unpacks the profile, and the player is in.
8. The hot path splices the two sockets together. Packets flow without
   interpretation; the proxy keeps connection id `N` in its books and the
   counters move.

Now the same flow with a MiniMessage-backed rejection, which is how failure
text is always produced: the player disconnects from the backend and the
`try` chain is exhausted. The proxy builds a Disconnect packet written to a
login-state connection. The reason string comes from the locale catalog
(English by default), is a MiniMessage document, is parsed at the point of
use by the project's component library, is serialized to the JSON component
the protocol requires, and — if compression was negotiated — is sealed into
a frame. The raw string a client sees is exactly the configured message;
nothing about why it failed is leaked, and the detailed reason (backend name,
attempt order, error class) appears in the proxy logs instead. Any
client-controlled fragment interpolated into that message (for example a
forced-host value) passes through the MiniMessage escape first, so a hostile
client cannot smuggle tags into a disconnect screen.

## Where the build stands

The protocol foundation, the crate catalogue above and the workspace's CI
are in place: fmt, clippy with `-D warnings`, tests and rustdoc all gate
merges. The slices already merged include the codec and registry, compression
and encryption, auth (online and offline), configuration with validation and
hot reload, permissions and its config provider, routing with health checks,
forwarding, cache, scheduler, metrics, telemetry, i18n, the messaging crates,
the plugin loader and the plugin ABI, and the unified command set.

Slices still landing — meaning the code exists in the workspace but is not
yet exercised end to end in a single binary run — include the status
intercept (a correct status response served from config), the routing
decision inside the pipeline, the login pipeline including forwarding, the
hot-path splice, the wiring of the built-in commands into the runtime and
the composition root itself. Until those land, treat the diagram above as
the shape the system is converging to, and the crate paragraphs above as
exact statements of what each part does today.

## Related docs

- [Installation](installation) — building, running, backend forwarding setup
- [Configuration](configuration) — the `candyriya.toml` reference
- [Permissions](permissions) — the permission engine, nodes and precedence
- [Contributing](https://github.com/CandyriyaMC/candyriya-rs/blob/main/CONTRIBUTING.md) —
  how the workspace is checked and merged
