# Candyriya

A from-scratch Minecraft Java Edition proxy written in Rust, built on Tokio.
It connects players to your backend servers, forwards their identity securely,
and gives a network a single place to handle routing, authentication and
plugins.

## What it is

Candyriya (candyriya-rs) is an independent community proxy, not a commercial product and
not a rebrand of anything. It speaks the vanilla protocol directly, routes
connections to backend servers, and adds the facilities a real network needs:
secure player-info forwarding, forced hosts, fallback routing, a WASM plugin
architecture and a built-in command set. It is licensed GPL-3.0 and developed
in the open.

## Who it is for

Candyriya is for people who run a Minecraft network and want a proxy they
can inspect, trust and extend:

- You run a lobby and a set of backends and want one native binary on the
  proxy host, with no JVM to babysit and no runtime to patch.
- You want interop, not lock-in: the proxy speaks the vanilla protocol,
  implements Velocity's modern forwarding as a wire contract, and understands
  BungeeCord and Velocity plugin messaging.
- You want to extend the proxy. Plugins are WebAssembly modules with an
  explicit descriptor (`plugin.toml`), their own cache namespace, and their
  own permission nodes.
- You want to read the code before you run it. The workspace forbids
  `unsafe`, and every check CI runs is documented in CONTRIBUTING.md.

The reason to select Candyriya for a network comes down to six things: a
core API bridge that plugins program against, proxy interop with the existing
plugin-messaging world, a plugin architecture that keeps untrusted code on a
short leash, a multi-thread async runtime that uses every core, a
cachegirl-backed cache shared by the core and by plugins, and full i18n for
every user-facing string.

## Features

- Any client version from 1.7 to the latest. The proxy routes the handshake
  and echoes the client's protocol version in the ping response; it never
  translates packets. Translation lives on the backends (ViaVersion and
  friends), so a client that is too old or too new for a backend is handled by
  the backend, or routed elsewhere through the fallback chain.
- Four forwarding modes, chosen per network: `modern` (HMAC-signed player
  info, Velocity-compatible pull model), `bungeeguard` (token-checked legacy
  data for older backends), `legacy` (BungeeCord inline data, plaintext), or
  `disabled`.
- A data-driven protocol registry covering 1.7 through the current 26.x line,
  with capabilities and packet tables sourced from upstream protocol data.
  New Minecraft releases land by refreshing the registry, not by rewriting
  the proxy.
- A command set built into the binary: `/candyriya` (info, servers, switch,
  end), `/server`, `/glist`, `/goto` and `/send`.
- A plugin system over WebAssembly (Extism). Plugins carry a `plugin.toml`
  descriptor, declare dependencies, run on dedicated OS threads with resource
  limits and timeouts, and never execute on the proxy's async workers.
- Per-plugin cache namespaces and quota accounting, backed by cachegirl, with
  atomic leases and a scheduled cleanup engine.
- A permissions engine built into the core: nodes, subjects, contexts
  (`server=`, `world=`), inheritance, wildcards and allow/deny rules. The
  default provider is config-backed; the provider interface can be replaced
  (MongoDB, PostgreSQL, or your own) without touching the core.
- Zero-copy hot path on Tokio. Bounded channels everywhere, backpressure by
  construction, no `unsafe` anywhere in the workspace.
- Low memory footprint. One native binary, no JVM on the proxy host; the
  working set is the connections you actually have.
- Graceful drain shutdown. On stop, the accept loop closes, in-flight work
  drains to a deadline, and the process reports what completed and what did
  not.
- Observability out of the box: Prometheus metrics, OpenTelemetry tracing,
  structured logs with per-connection IDs, daily log rotation, and color on a
  TTY only.
- i18n for every user-facing string: English is the default and fallback
  language, a Russian locale ships alongside it, and plugins bring their own
  locale files with the same fallback chain and hot reload.
- Structured console: table-style output with connection IDs, backend names
  highlighted, and a single color palette across the process.

## Installation

Installation is documented on the docs site rather than duplicated here:

- [Installation guide](https://candyriyamc.github.io/candyriya-rs/installation) —
  prebuilt release assets, building from source, online vs offline auth mode,
  modern forwarding setup, Paper backend configuration, a first-run checklist
  and an update procedure.

Prebuilt binaries are published on the
[GitHub Releases page](https://github.com/CandyriyaMC/candyriya-rs/releases) for
tagged versions. Until the first tag is cut, building from source is the
honest path.

## Documentation

The docs site is built from the [`docs/`](https://github.com/CandyriyaMC/candyriya-rs/tree/main/docs)
directory in this repository:

- [Installation](https://candyriyamc.github.io/candyriya-rs/installation) —
  requirements, downloads, forwarding setup and first-run checklist
- [Configuration](https://candyriyamc.github.io/candyriya-rs/configuration)
  (coming soon) — every key in `candyriya.toml`, validation and hot reload
- [Permissions](https://candyriyamc.github.io/candyriya-rs/permissions)
  (coming soon) — the permission engine: nodes, contexts, inheritance and
  wildcards
- [Plugin messaging](https://candyriyamc.github.io/candyriya-rs/plugin-messaging)
  (coming soon) — the BungeeCord, Velocity and native `candyriya:main`
  channel specs
- [Architecture](https://candyriyamc.github.io/candyriya-rs/architecture)
  (coming soon) — workspace layout, design notes and the crate dependency
  DAG
- [Commands](https://candyriyamc.github.io/candyriya-rs/commands)
  (coming soon) — the built-in command set and its permission nodes

Pages marked "coming soon" are being written; the source of truth is the
`docs/` directory on `main`.

## Building from source

You need a recent stable Rust toolchain (edition 2024), installed via
[rustup](https://rustup.rs). From a clone of the repository, run
`cargo build --release`. The resulting binary is `target/release/candyriya`
(`candyriya.exe` on Windows). CONTRIBUTING.md covers the checks CI runs
before a pull request is merged.

## Status and maturity

Pre-1.0, under active development. Candyriya is being built in the open,
crate by crate. Nothing here is a final API yet; configuration formats and
plugin contracts may change between commits, and pre-1.0 upgrades are your
responsibility.

Shipped on `main` so far:

- Workspace scaffold, protocol framing and the data-driven 1.7→latest
  (including 26.x) version registry
- Compression and encryption layers: vanilla wire frames, RSA/AES-128-CFB8,
  Mojang session authentication in online and offline mode
- Forwarding codecs for `modern` and `bungeeguard`
- Configuration loader with validation and hot reload
- The permissions engine and its config-backed default provider
- Routing: forced hosts, backend registry, health checks
- cachegirl-backed cache, offline-player store, i18n engine (English and
  Russian), event bus, central task scheduler with drain-on-shutdown
- Messaging: BungeeCord wire codec, Velocity plugin messaging semantics, and
  the native `candyriya:main` channel
- Plugin API surfaces (chat, title, bossbar, tablist, events) and the
  Extism-based loader with per-plugin descriptors and quotas
- Proxy accept loop with connection limits, the handshake pipeline with hard
  limits, and the unified admin and player command set
- Observability: Prometheus metrics exporter, structured tracing with daily
  rotation

On the horizon, high level:

- Zero-copy hot path and the end-to-end login → forwarding flow wired up
- Backend messaging hooks, per-plugin resource limits and WASM fuel metering
- The live admin console, and Prometheus/OpenTelemetry wired at boot
- The documentation pages listed above
- First tagged release with prebuilt binaries, smoke testing against Paper
  1.21.1, and a high-concurrency stress pass

No dates are promised. Work lands when it is ready, and the honest status of
any feature is what the code on `main` does, not what this page claims.

## Contributing

Bug reports and feature requests go through
[GitHub Issues](https://github.com/CandyriyaMC/candyriya-rs/issues) with the
templates; questions and ideas go to
[GitHub Discussions](https://github.com/CandyriyaMC/candyriya-rs/discussions).
Those pages are where network operators and plugin authors should start.
For live chat, the community gathers on the
[CandyriyaMC Discord server](https://discord.gg/fXwN4wETF9).

- [CONTRIBUTING.md](https://github.com/CandyriyaMC/candyriya-rs/blob/main/CONTRIBUTING.md)
  — how to build, test and submit changes; the repository squash-merges and
  uses Conventional Commits
- [Code of Conduct](https://github.com/CandyriyaMC/candyriya-rs/blob/main/.github/CODE_OF_CONDUCT.md)
  — the project follows the Contributor Covenant
- [SECURITY.md](https://github.com/CandyriyaMC/candyriya-rs/blob/main/SECURITY.md)
  — how to report a vulnerability privately; never file a security issue in
  public

## Credits

Candyriya stands on Tokio, the async runtime underneath everything here,
and on the wider Rust async ecosystem: bytes, crossbeam, dashmap, rayon,
parking_lot, arc-swap, flate2, figment, tracing and the OpenTelemetry tooling.
Its wire compatibility builds on the work of others: the Velocity project
defined the modern forwarding contract the proxy implements, Paper and
Purpur are the reference backends, BungeeCord defined the plugin messaging
wire format, and Minecraft protocol data from Mojang and the PrismarineJS
project feeds the version registry. The cache layer uses cachegirl, the text
and MiniMessage handling uses lobsterchat, and plugins run on Extism. All of
it is free software, and so is Candyriya.

## License

Candyriya is free software, licensed under the
[GNU General Public License v3.0](https://github.com/CandyriyaMC/candyriya-rs/blob/main/LICENSE).
You can study, modify and redistribute it under the same terms.
