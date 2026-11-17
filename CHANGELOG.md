# Changelog

All notable changes to candyriya-rs are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

GitHub Releases are attached to tags automatically when they are pushed; each
release carries the prebuilt binaries as assets. See the
[Releases](https://github.com/bejiihiu/candyriya-rs/releases) page.

## [0.1.0] - 2026-11-17

The first tagged release of candyriya-rs: a from-scratch Minecraft Java Edition
proxy written in Rust — Tokio everywhere, no `unsafe`, and no JVM on the proxy
host. Feature complete and documented end to end; see
[Known limitations](#known-limitations) for the explicit edges.

### Download and install

- **Prebuilt binaries** are attached to every tagged release on the
  [Releases](https://github.com/bejiihiu/candyriya-rs/releases) page, one asset
  per target: `candyriya-<version>-x86_64-unknown-linux-gnu.tar.gz`,
  `candyriya-<version>-aarch64-unknown-linux-gnu.tar.gz`,
  `candyriya-<version>-x86_64-pc-windows-msvc.zip`,
  `candyriya-<version>-aarch64-apple-darwin.tar.gz` and
  `candyriya-<version>-x86_64-apple-darwin.tar.gz`. Built by the release
  workflow from the `v0.1.0` tag.
- **From source:** you need a stable Rust toolchain (edition 2024) via
  [rustup](https://rustup.rs). Then `cargo build --release`; the resulting
  binary is `target/release/candyriya` (`candyriya.exe` on Windows).
- **First run:** one binary in an empty directory is enough. On first start the
  proxy writes a commented `candyriya.toml` and generates a fresh
  `forwarding.secret` with owner-only permissions. Run the backend with
  `online-mode=false` and let modern forwarding tell it who the player is,
  then join.

The [installation guide](https://bejiihiu.github.io/candyriya-rs/installation)
covers the whole path: requirements and OS matrix, prebuilt assets and source
builds, online vs offline mode, modern forwarding end to end (Paper's
`paper-global.yml`, `server.properties`, `spigot.yml`), the minimal
`candyriya.toml` you will actually touch, a first-run checklist, common
failures and how to update.

### Added

**Wire protocol**

- **Protocol registry** — data-driven version tables covering stable releases
  from 1.7.2 through the current 26.x line (including 26.3), with packet ids
  for status and login, built from the PrismarineJS minecraft-data tables and
  embedded in the binary. `[protocol] auto-update = true` (the default)
  refreshes the tables from `update-url` on startup; an override file can
  replace them wholesale. New Minecraft releases arrive as registry refreshes,
  not as proxy code changes.
- **Compression** — the vanilla zlib framing implemented with pure-Rust
  `flate2`/`miniz_oxide`, no C zlib anywhere. Declared lengths are capped
  before any inflation (zip-bomb safe), and the 2 MiB packet ceiling plus the
  3-byte length prefix are enforced on encode and decode.
- **Encryption** — the login key exchange implemented from the spec: RSA-2048
  (PKCS#1 v1.5, as the protocol demands), AES-128-CFB8 with continuous cipher
  state, and the session-server `serverId` hash (SHA-1 as lowercase base-16).
  Secrets are zeroized on drop.
- **Version passthrough** — any client from 1.7 to the latest release is
  accepted at the handshake. The proxy never translates packets; version
  differences are a backend concern or the `try` chain's job to route around.

**Authentication**

- **Online mode** — real Microsoft account verification: the proxy issues its
  own encryption request, decrypts the client's shared secret and confirms the
  session with Mojang's `hasJoined` endpoint (timeout-bounded, strict response
  handling, properties passed through verbatim). The proxy never sees a
  password.
- **Offline mode** — deterministic identity from the player name (vanilla v3
  UUID, `OfflinePlayer:<name>`), no network involved.

**Routing and forwarding**

- **Forced hosts** — host-to-backends mapping with single-label wildcards
  (`*.example.com` matches `a.example.com`, not `a.b.example.com`) and rank
  order exact > wildcard > catch-all. Malformed input never matches.
- **`try` fallback chain** — a backend that kicks during login or fails to
  respond walks the chain in declaration order; when it runs out, the player
  is disconnected with the configured reason.
- **Health checks** — real Minecraft status pings against registered
  backends, driven by the task service.
- **Forwarding** — four modes: `modern` (the default; HMAC-SHA256-signed
  player info over the `velocity:player_info` channel, pull model,
  wire-compatible with the contract Velocity defines and Paper verifies),
  `bungeeguard` (legacy data with the token as the last property), `legacy`
  (identity inline in the handshake address), and `disabled`.

**Commands**

- **Extensible command API** on azalea-brigadier: permission-gated literals,
  argument trees, brigadier completions pass-through, and a seam
  (`NetworkDirectory` / `ProxyEffects`) that keeps command implementations free
  of network code. Built-in set: `/candyriya info|servers|switch|end`,
  `/glist`, `/server`, `/goto` and `/send` — the player-facing commands are
  ungated, Velocity-style.

**Permissions**

- **Own permission engine** (not a port of another project's): validated node
  newtype with `*` and `foo.*` wildcard semantics, subjects and groups,
  contexts (`server=`, `world=`), allow/deny grants, a node registry with
  defaults, and longest-match resolution — the most specific grant wins, ties
  break to deny, and unregistered nodes default to deny. Matching is plain
  segment comparison with no regex anywhere. Ships with a config-backed
  default provider (`[permissions] provider = "config"`); the provider
  interface admits other backends (MongoDB, PostgreSQL) without touching the
  core.

**Plugins**

- **WebAssembly plugin system** on Extism: `plugin.toml` descriptors (id,
  semver version, `plugin_main`, `depend`/`softdepend`, cache quotas,
  `[api].exports`) validated up front with all problems reported at once.
- **Dedicated OS threads** — every plugin runs on its own named thread
  (`candyriya-plugin-<id>`), never on the async workers; calls cross a
  bounded mailbox with a receive timeout, and a full mailbox fails fast
  instead of blocking the proxy. Panics are caught at the boundary.
- **Sandbox limits** — memory pages, table elements, fuel, per-call timeout,
  open handles, output/input/log byte caps and a per-plugin call-rate limit,
  all enforced host-side (see `[plugins]`).
- **Dependency resolution** — `libraries/` loads before `plugins/`, then a
  topological (Kahn) sort over `depend` (hard) and `softdepend` (ordering)
  with cycle detection and cascade reporting for missing hard dependencies.
- **Plugin hot replacement** — staged wasm + manifest replacement (the
  running plugin is untouched on a bad edit), load order preserved,
  file-watcher driven.
- **Per-plugin cache namespaces** — `cache/<plugin>/` and `temp/<plugin>/`
  through cachegirl with atomic byte and resource quotas and scheduled
  cleanup.

**Core infrastructure**

- **cachegirl-backed cache everywhere** — pooled buffers, per-plugin
  namespaces, quotas enforced atomically at creation, no hand-rolled TTL maps
  anywhere in the tree. Mojang profile properties are cached; session secrets
  are never cached or persisted.
- **Offline-player history** — `players.json` with atomic rewrite on every
  save, validated on open (size cap, per-record decode, duplicate UUIDs are a
  hard error).
- **Central task service** — every delayed, repeating, cron or background
  task goes through one scheduler: named tasks, cooperative cancellation,
  per-task metrics, panic containment, and a graceful drain with a deadline.
  Nothing spawns ad hoc.
- **Event bus** — typed, synchronous, panic-contained in-process bus for
  communication between subsystems.

**Observability**

- **Prometheus metrics** — pull exporter on `[metrics] prometheus-address`
  (default `0.0.0.0:9124`), disabled by default (no listener opened), fixed
  metric names, `service="candyriya"` global label, and a histogram for login
  duration.
- **Structured logs** — compact text with UTC RFC 3339 timestamps,
  per-connection IDs and task IDs, optional daily-rotating file sink
  (`logs/candyriya.log.YYYY-MM-DD`), colors on a TTY only with `NO_COLOR`
  support, and an optional OpenTelemetry OTLP export behind the `otlp` cargo
  feature.

**i18n**

- **Every user-facing string** flows through MiniMessage into the embedded
  locales: English (the default and the fallback) and `ex_girl` (Russian).
  Files in `<data_dir>/locales/{lang}/{file}.toml` override the embedded
  copies per file; missing keys fall back active-locale → English → key.
  Plugins register their own locales into the same machine via a `locales/`
  directory, with the same fallback chain and the same hot reload.

**Configuration and operations**

- **Velocity-style `candyriya.toml`** — see [Configuration](#configuration).
  Hot reload is atomic through `arc-swap`; a failed reload leaves the running
  configuration intact. The `forwarding.secret` is generated once on first
  run, resolved relative to the config file, never rotated implicitly and
  never logged.

**Build and release engineering**

- **100% safe Rust** — `unsafe_code = "forbid"` at the workspace level.
- **Integration harness** (`candyriya-it`) — a real server-side TCP backend
  and a real client on the vanilla wire codec, loopback only, no mocks;
  byte-for-byte verification of the login and status flows.
- **Fuzz targets** for the parsers (packet codec, protocol registry,
  forwarding verify, handshake state machine, string reader), checked in CI,
  plus criterion benchmarks for the hot paths.
- **CI gates on every push** — rustfmt, clippy with `-D warnings`, tests,
  rustdoc with `-D warnings`, cargo-deny and RustSec audit-check.
  `Cargo.lock` is committed; the release workflow builds five targets on tag
  push.

### Security

- **RUSTSEC-2023-0071 is fixed.** `rsa` is pinned to 0.10 with implicit-rejection
  PKCS#1 v1.5 padding; `time` was bumped in the same pass. The cargo-deny
  advisory gate in CI rejects any regress.
- **Offline mode is unverified by design, and documented as such.** With
  `online-mode = false`, anyone connecting with any name is accepted and
  nobody is checked against Mojang. Be clear-eyed about what that means:
  everyone connecting without verification is not verified at all. It is for
  trusted LANs and private networks; premium networks should run
  `online-mode = true` with backends in `online-mode=false` behind modern
  forwarding. See the installation guide for the full comparison.
- **No `unsafe` anywhere.** The forbid is a workspace lint, enforced at
  compile time; where the platform offers no safe API (for example TCP Fast
  Open), the option is skipped with a warning instead.
- **Plugins are sandboxed.** Plugins run on their own named OS threads with
  bounded mailboxes, fuel limits, memory caps, output caps, a handle table
  and a per-plugin call-rate limit; a plugin panic is contained at the
  boundary and never takes down the proxy.
- **The forwarding secret is sacred.** Generated on first run with
  owner-only permissions, never rotated implicitly, never logged or printed —
  not even partially — and rendered as `***` in debug output.
- **Dependency hygiene is a hard gate.** cargo-deny in CI rejects wildcard
  version requirements and unexplained duplicate crate versions (the failure
  modes that let a dependency drift across semver-incompatible releases),
  rejects unknown registries and git sources, enforces the license allowlist,
  and fails on unmaintained or unsound crates. RustSec audit-check annotates
  every run.
- **No internals on the wire.** Disconnect reasons shown to clients are
  generic; the detailed cause (backend, attempt order, error class) stays in
  the proxy logs, and client-controlled fragments are MiniMessage-escaped
  before interpolation.
- **Fail closed on unverified backends.** A modern-mode backend that
  completes login without the `velocity:player_info` exchange is refused —
  the player is disconnected instead of being served by an unverified
  backend, and the fallback chain is not advanced past it.

### Configuration

`candyriya.toml` is one annotated, commented file generated on first run.
Unknown keys are warnings, not errors; every validation problem is reported
together with file location. Hot reload swaps the running configuration
atomically; a failed reload keeps the old one. The full reference lives in
[docs/configuration.md](https://bejiihiu.github.io/candyriya-rs/configuration).

- Root keys: `bind`, `motd` (MiniMessage only; `§`-codes are not supported),
  `motd-favicon`, `show-max-players`, `online-mode`, `force-key-authentication`,
  `player-info-forwarding-mode` (`modern` default | `bungeeguard` | `legacy`
  | `disabled`), `forwarding-secret-file`, `prevent-client-proxy-connections`,
  `enable-player-address-logging`, `announce-forge`, `kick-existing-players`,
  `locale`, `config-version`.
- `[servers]` — name to `host:port`, in order; `try = ["lobby"]` — the
  ordered fallback chain.
- `[forced-hosts]` — host (exact or `*.example.com`) to a list of backends.
- `[advanced]` — `compression-threshold`, `compression-level`,
  `login-ratelimit`, `connection-timeout`, `read-timeout`,
  `handshake-timeout`, `max-connections`,
  `max-new-connections-per-second`, `enable-reuse-port` (Unix only; warns and
  is ignored on Windows), `tcp-fast-open` (skipped with a warning; no safe
  API exists), `bungee-plugin-message-channel`, `accepts-transfers`,
  `haproxy-protocol`, `command-rate-limit`, `tab-complete-rate-limit`, and
  the rest of the Velocity-shaped surface.
- `[query]` — GameSpy4 query keys (validated; disabled by default).
- `[permissions]` — `provider = "config"` (the built-in default provider) and
  `default = "default"`; `grants` maps group name to a list of nodes, where
  the `default` key is the implicit all-subject bucket. Per-player group
  assignment is not a first-class file key yet; it is handled by the provider
  layer.
- `[protocol]` — `auto-update = true` (default) and `update-url`
  (PrismarineJS tables).
- `[metrics]` — `prometheus = false` (default) and
  `prometheus-address = "0.0.0.0:9124"`.
- `[plugins]` — WASM sandbox caps: `max-memory-pages`, `max-fuel`
  (`0` = unlimited), `max-calls`, `max-call-timeout`, `max-output-bytes`,
  `max-input-bytes`, `max-log-bytes`.
- `[runtime]` — `worker-threads` and `blocking-threads`; both fixed sizes
  from configuration (default: CPU count), never auto-tuned.

### Docs

- **A full documentation site** lives in `docs/`, built to GitHub Pages,
  covering the whole product in four guides:
  - [Installation](https://bejiihiu.github.io/candyriya-rs/installation) —
    requirements, downloads, building from source, online vs offline mode,
    modern forwarding end to end, a first-run checklist, troubleshooting and
    updating.
  - [Configuration](https://bejiihiu.github.io/candyriya-rs/configuration) —
    every key in `candyriya.toml`, its type, default, behavior, validation
    rules and cross-key interactions; hot reload semantics.
  - [Permissions](https://bejiihiu.github.io/candyriya-rs/permissions) — the
    engine: node grammar, resolution precedence, contexts, groups, the
    config-backed provider, and worked examples.
  - [Architecture](https://bejiihiu.github.io/candyriya-rs/architecture) —
    every crate in the workspace, the connection pipeline end to end, the
    dependency DAG, design notes and an honest build-status section.
- **README.md** rewritten as a proper front page: what candyriya-rs is and
  who it is for, a feature tour, honest status and credits.
- **No stubs in the shipped guides.** The four guides above are complete
  references rather than placeholders; the plugin and command authoring pages
  on the docs index are marked “coming soon” and are not yet published.
- **Repository hygiene** — SECURITY.md (private vulnerability reporting via
  GitHub advisories), CONTRIBUTING.md, Code of Conduct, issue and pull-request
  templates, and `docs/development.md` documenting the CI audit pipeline.

### Known limitations

- **No in-session player switching.** A live session cannot be redirected to
  another backend from inside the play state in v0.1.0: a switch is handled as
  a re-authentication per connection, and true in-session redirection (backend
  reload without a fresh login) lands in a later release.
- **No third-party plugin API export yet.** Plugin descriptors and host
  functions are validated for workspace-internal consumers only; the plugin
  ABI is not a published stability contract for out-of-tree authors in
  v0.1.0. The public plugin authoring guide and the exported API land with
  the plugin docs in a later release.
- **TCP only.** There is no UDP/QUIC transport; the proxy speaks Minecraft's
  TCP protocol exclusively.

[0.1.0]: https://github.com/bejiihiu/candyriya-rs/releases/tag/v0.1.0
