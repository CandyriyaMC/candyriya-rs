---
layout: default
title: candyriya-rs
---

# candyriya-rs

A high-performance Minecraft Java Edition proxy written in Rust.

candyriya-rs is a from-scratch, asynchronous Minecraft proxy in the spirit of
[Velocity](https://papermc.io/software/velocity), built on Tokio and designed
for low latency and high connection counts. It speaks the vanilla protocol
directly and forwards player connections to backend servers, adding modern
facilities such as forced hosts, fallback routing and secure player info
forwarding.

## Status

**Pre-1.0, under active development.** The project is being built in the open
against real server software, but APIs and configuration formats may change
between commits. Use it at your own risk on production networks; the protocol
core handles all supported client versions, but many features are still being
landed crate by crate.

## Highlights

- Native Rust implementation of the Minecraft Java Edition handshake, status
  and login flow — raw passthrough for any client version
- Velocity-style modern forwarding with HMAC-signed player info
- Forced hosts and try/fallback routing with backend health checking
- Asynchronous hot path built on Tokio, with no `unsafe` code
- Extensible plugin system over WebAssembly (loaded at startup and hot-reloadable
  with `/candyriya reload`), a permission engine, and a
  cron-style task scheduler (rolling out incrementally)

## Documentation

- [Installation](installation) — building from source, running the proxy,
  connecting a backend
- [Configuration](configuration) — configuration file reference and hot reload
- [Development](development) — notes for hacking on candyriya-rs,
  including the audit pipeline
- [Permissions](permissions) — the permission engine: nodes, contexts,
  inheritance and wildcards
- [Plugins](plugins) — writing plugins against the WebAssembly API
- [Commands](commands) — built-in commands and how to invoke them
- [Architecture](architecture) — workspace layout and design notes

## Community

- [Issues](https://github.com/bejiihiu/candyriya-rs/issues) — bug reports and
  feature requests (please use the templates)
- [Discussions](https://github.com/bejiihiu/candyriya-rs/discussions) —
  questions and ideas
- [Contributing](https://github.com/bejiihiu/candyriya-rs/blob/main/CONTRIBUTING.md) —
  how to build, test and submit changes
- [Security policy](https://github.com/bejiihiu/candyriya-rs/blob/main/SECURITY.md) —
  report vulnerabilities privately, never in public issues

candyriya-rs is free software, licensed under
[GPL-3.0](https://github.com/bejiihiu/candyriya-rs/blob/main/LICENSE).
