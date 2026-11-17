---
layout: default
title: Installation
---

# Installation

This guide covers getting the Candyriya proxy onto a host, pointing it at
backend servers, and surviving the first start. It assumes you have run
Minecraft servers before; it does not explain what a proxy is. Candyriya is
pre-1.0 — APIs and config formats may shift between commits.

## Requirements

**The proxy does not need Java.** It is a single native Rust binary. Java is
only for your backends: Paper 1.21.x requires a Java 21 runtime (Temurin or
any other compliant build will do).

What you need on the proxy host:

- A 64-bit OS. Linux is the best-supported target (CI runs it), macOS on
  Apple Silicon or Intel works, and Windows 10/11 or Server 2019+ works.
- Outbound HTTPS on first start, if you leave `[protocol] auto-update` on.
- Disk: roughly 50 MB for the install itself, plus plugins and logs. World
  data lives on your backends, never on the proxy.
- Network: the proxy listens on a TCP port (`25565` by default). Backends
  listen on their own ports, ideally bound to localhost or firewalled off —
  a backend reachable directly bypasses every check the proxy makes.
  `[query]` (GameSpy4) uses UDP on the same port if you enable it, and
  `[metrics] prometheus` listens on `0.0.0.0:9124` if you turn it on.

On Windows specifically: use PowerShell, not `cmd.exe`. Quote any path that
contains spaces, and prefer `.\candyriya.exe` over bare names so you are
unambiguous about what runs. The `0600` permission on `forwarding.secret`
is unix-only — on Windows, ACLs are your responsibility, so keep the data
directory out of shared folders.

## Download

### Prebuilt release

Tagged releases publish binaries on the
[GitHub Releases page](https://github.com/CandyriyaMC/candyriya-rs/releases).
Assets are named `candyriya-<version>-<target>.tar.gz` (Linux and macOS) or
`candyriya-<version>-<target>.zip` (Windows), where `<target>` is the Rust
triple, e.g. `x86_64-unknown-linux-gnu` or `x86_64-pc-windows-msvc`. There is
no auto-update mechanism; upgrades are manual.

If no tagged release exists yet, you will need to build from source — for a
pre-1.0 project that is the honest default anyway.

### Build from source

The repository is GPL-3.0. You need a recent stable Rust toolchain (edition
2024, installed via [rustup](https://rustup.rs)) and a C toolchain for some
transitive dependencies (`build-essential` on Debian/Ubuntu, `gcc` on Fedora,
MSVC Build Tools or mingw-w64 on Windows):

```shell
git clone https://github.com/CandyriyaMC/candyriya-rs.git
cd candyriya-rs
cargo build --release
```

The binary lands at `target/release/candyriya` (`candyriya.exe` on Windows).

## Auth mode: online vs offline

`online-mode` in `candyriya.toml` decides whether connecting players must
prove they own a legitimate Mojang account through Mojang's session servers.
It has to be consistent across the proxy and every backend, and this is where
most first-time setups go wrong.

- **`online-mode = true` (default):** premium networks. Players authenticate
  with Mojang. Backends MUST run in offline mode from vanilla's point of
  view (`server.properties` `online-mode=false`) and MUST rely on proxy
  forwarding to know who the player is. A backend left in online mode will
  reject every proxied login — from the player's perspective that is the
  classic "Failed to verify username!" kick.
- **`online-mode = false`:** offline networks. Any name joins. Mojang
  authentication is skipped entirely; skins and properties are not fetched.
  Backends are also `online-mode=false`. This is normal for LAN and
  private networks.

Either way, `online-mode` on the backend side is not enough on its own — the
backend needs to know it is behind a proxy and trust exactly one. That is
what forwarding is for, below.

## Configuration walkthrough

Candyriya reads a single `candyriya.toml`. With no file at all it runs on
embedded defaults and generates a `forwarding.secret` on first start, so the
smallest viable install is: one binary, one data directory, one backend.

The keys you will actually touch on day one:

- `bind = "0.0.0.0:25565"` — where the proxy listens. Use `[::]:25565` for
  dual-stack.
- `motd` — the server-list message. MiniMessage only; `§`-codes are not
  supported. The shipped default is `<aqua>A Candyriya network`.
- `show-max-players = 100` — cosmetic number in the ping response. The real
  cap is `[advanced] max-connections` (default 1024).
- `online-mode = true` — see the previous section.
- `try = ["lobby"]` under `[servers]` — the ordered fallback chain. When a
  backend dies or kicks a player, the proxy walks this list; when it runs
  out, the player is disconnected with the configured kick message. Every
  name here must exist under `[servers]`.
- `lobby = "127.0.0.1:25566"` under `[servers]` — name → host:port. Order in
  the file is the order shown to players and the order used for fallback.
- `[forced-hosts]` — hostname → ordered server list, for
  `play.example.com`-style splits. All names must exist under `[servers]`.
- `[advanced]` — timeouts and caps. The defaults (compression at 256 bytes,
  read timeout 30 s, handshake timeout 5 s) are sane; leave them alone until
  you have a reason.
- `[query]` — GameSpy4 query, disabled by default. It leaks player counts
  and plugin lists; enable only on trusted networks.
- `[metrics]` — Prometheus exporter on `0.0.0.0:9124`, disabled by default.
  Bind to localhost or firewall it.
- `[runtime]` — `worker-threads` and `blocking-threads`, both defaulting to
  the number of logical CPUs. Leave commented unless you are pinning cores.

That is deliberately a subset. The full commented reference — every key,
including `[permissions]` grants and `[protocol] auto-update` — lives in the
[Configuration reference](configuration), along with the complete commented
default file. Unknown keys in your file are warned about, not errors, so
older files keep working across upgrades.

## Forwarding setup

Backends authenticate the proxy, not the player — it is what makes proxied
UUIDs trustworthy and stops clients from forging connections directly to
backends. `player-info-forwarding-mode` picks the mechanism. `modern` is the
default and you should not change it unless a backend forces you to.

1. Start the proxy once. It generates `forwarding.secret` in the data
   directory (32 random bytes, base64, mode `0600` on unix). It never rotates
   on its own; delete the file and restart to rotate.
2. Put the file's contents into each backend's `paper-global.yml` as
   `proxies.velocity.secret` (exact string, including any padding).
3. Set `proxies.velocity.enabled = true` and
   `proxies.velocity.online-mode` to match the proxy's `online-mode`.

| Mode | Security | Compatibility | Backend requirements |
|------|----------|---------------|----------------------|
| `modern` (default) | HMAC-signed profile data, pull-model | 1.13+ backends | Paper/Purpur with velocity support |
| `bungeeguard` | Token-checked legacy data | 1.8+ with BungeeGuard installed | `spigot.yml` `bungeecord: true` + BungeeGuard plugin, token = `forwarding.secret` |
| `legacy` | None — inline data is unauthenticated | Pre-1.13 backends | `spigot.yml` `bungeecord: true`; anyone who can reach the port can forge logins |
| `disabled` | n/a | Throwaway setups | Nothing; backends see the proxy's address |

For mixed-legacy fleets, `bungeeguard` is strictly better than `legacy` —
same wire format, plus the veto against forged handshakes. Use `legacy` only
when the backend cannot run BungeeGuard at all.

## Backend configuration for Paper

For a modern-forwarding setup, `config/paper-global.yml` on each backend
looks like this:

```yaml
proxies:
  velocity:
    enabled: true
    online-mode: true   # set to false to match an offline-mode proxy
    secret: "paste-the-contents-of-forwarding.secret-here"
```

and `config/spigot.yml` keeps legacy forwarding off:

```yaml
settings:
  bungeecord: false
```

For `bungeeguard` or `legacy` modes, flip `bungeecord` to `true` and leave
`proxies.velocity.enabled` at `false`. In BungeeGuard mode also install the
plugin and put the `forwarding.secret` contents in its `token.yml`. The
backend's `server.properties` keeps `online-mode=false` regardless — the
proxy does the Mojang authentication; the backend trusts the proxy.

Two things to expect after a correct setup: the backend log shows the
velocity forwarding channel in use, and clients that try to reach the
backend directly instead of through the proxy are rejected — that is the
whole point of the exercise.

## Client versions and the proxy

The proxy speaks raw passthrough: it routes the connection and echoes the
protocol version the client announced in the ping response, so launchers and
server lists behave as if the client were talking to a native server of that
version. It does **not** translate packets between versions.

Multi-version support happens at the backend end. Run
[ViaVersion](https://hangar.papermc.io/ViaVersion/ViaVersion) (and
[ViaBackwards](https://hangar.papermc.io/ViaVersion/ViaBackwards) if you
need older clients on a newer backend) on the backend or a lobby server. A
backend that receives a protocol it cannot serve kicks the player; the proxy
then walks the `try` chain, and if no backend accepts, disconnects with the
configured kick message.

## Running the proxy

The data directory is the current working directory by default; pass
`--data-dir <path>` to pin it. Everything Candyriya writes — `candyriya.toml`,
`forwarding.secret`, `logs/candyriya.log.YYYY-MM-DD` (daily rotation),
`players.json`, cache and plugins — lives there.

**systemd** (the usual choice):

```ini
[Unit]
Description=Candyriya Minecraft proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=candyriya
WorkingDirectory=/opt/candyriya
ExecStart=/opt/candyriya/candyriya --data-dir /opt/candyriya/data
Restart=always
RestartSec=5
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

**Docker.** No image is published yet; build one from the repo with any
glibc-based base (or musl if you build with the musl target), then:

```shell
docker run -d --name candyriya -p 25565:25565 -v ./data:/data <image> --data-dir /data
```

Mount `./data` so the config, secret and logs survive container replacement.

**Windows PowerShell:**

```powershell
& 'candyriya.exe' --data-dir .\run
```

For unattended use on Windows, wrap that line in a scheduled task set to
start at boot with highest privileges, or run it under NSSM.

## First-run checklist

1. Start the proxy once with an empty data directory. Confirm it creates
   `candyriya.toml` (embedded defaults), `forwarding.secret`, and the logs
   directory, and that the log shows a clean bind.
2. Stop it. Edit `candyriya.toml`: set `bind`, `motd`, `online-mode`, and the
   `[servers]` table with at least one backend and a matching `try`.
3. Copy `forwarding.secret` into each backend's `proxies.velocity.secret`.
4. Make sure each backend has `online-mode=false` in `server.properties` and
   `proxies.velocity.enabled=true` in `paper-global.yml`.
5. Start the proxy, then join from a client. Check the proxy log for the
   connection, and the backend log for the forwarding channel.

If it does not work, it is almost always one of these:

- **Kicked on join, backend log shows a forwarding/verification error.**
  Secret mismatch, or `proxies.velocity.online-mode` does not match the
  proxy. Diff the two strings byte-for-byte; the file ends with base64
  padding that is easy to drop when copying.
- **"Failed to verify username!"** Backend still has `online-mode=true` in
  `server.properties`. Set it to `false`.
- **Everyone is moved to or kicked from `try`, no backend ever answers.**
  Firewall or wrong port. The proxy's health check marks unreachable
  backends offline; `connection refused` in the log means the backend is
  down, bound to another interface, or the `[servers]` entry points at the
  wrong port.
- **BungeeGuard backend kicks despite legacy/bungeecord config.** The
  BungeeGuard token and `forwarding.secret` differ, or the backend still
  runs `bungeecord: false`.
- **Kicks players immediately when switching servers.** The target backend
  runs a newer or older protocol than the client and kicks instead of
  passing through — install ViaVersion there rather than asking the proxy
  to cope.

Configuration details live in the [Configuration reference](configuration);
permission nodes for admin commands are in [Permissions](permissions).

## Updating

Upgrading is swapping one binary. Stop the proxy, replace the file, start it
again. Carry over the whole data directory; the only things that must survive
the swap are `candyriya.toml` and `forwarding.secret` (backends are
configured against the secret, so rotating it means re-touching every
backend).

`players.json` is the offline-player store. It is a stable, plain JSON
document (one record per player, keyed by UUID) and carries over between
releases as-is, but it is not a public API — take a backup before any
major bump, and treat pre-1.0 migrations as your own responsibility. On
first start after an upgrade the proxy warns about unknown keys rather than
failing, so stale config entries from older versions are safe to leave in
place.
