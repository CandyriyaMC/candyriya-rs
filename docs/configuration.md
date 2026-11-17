---
layout: default
title: Configuration
---

# Configuration

candyriya reads a single `candyriya.toml` file. This page is the reference
for every key the proxy understands — there is no other configuration. The
defaults are not invented; they come from the embedded, fully commented
`DEFAULT_CONFIG_TOML` template that ships inside the binary. That template is
both the base layer every load starts from and the file you get on first
run, so what you see in a freshly generated `candyriya.toml` is exactly the
set of defaults documented here. Your file overrides only the keys it
mentions; everything else inherits the template values.

The file format is TOML with kebab-case keys. Unknown keys are reported with
a warning and ignored — never an error. A malformed file is a hard error with
the parser's line and column. All detectable semantic problems (port ranges,
cross-references, control characters) are collected into one error at startup
rather than reported one at a time.

## Index

| Section | What it controls |
|---------|------------------|
| [Loading and layering](#loading-and-layering) | how the file, the embedded template and your overrides combine |
| [Root keys](#root-keys) | bind address, MOTD, auth mode, forwarding, misc top-level switches |
| [\[protocol\]](#protocol) | protocol registry auto-update |
| [\[servers\]](#servers) | backend list and the `try` fallback order |
| [\[forced-hosts\]](#forced-hosts) | hostname-based routing with wildcard rules |
| [\[advanced\]](#advanced) | compression, timeouts, caps, rate limits, feature flags |
| [\[query\]](#query) | GameSpy4 query service |
| [\[permissions\]](#permissions) | permission provider and static grants |
| [\[metrics\]](#metrics) | Prometheus exporter |
| [\[runtime\]](#runtime) | Tokio worker threads and the blocking thread pool |
| [Hot reload](#hot-reload) | how reloads are applied at runtime |
| [Troubleshooting configuration errors](#troubleshooting-configuration-errors) | the five most common pitfalls |
| [Related docs](#related-docs) | installation, permissions, plugins, and more |

## Loading and layering

- **Base layer.** The embedded template is merged first. Omitted keys inherit
  its values verbatim.
- **User layer.** Your file is merged on top, key by key. A missing file is
  not an error: the proxy runs on the embedded defaults (and still resolves
  the forwarding secret relative to the configured path).
- **Syntax errors.** A file that does not parse is rejected with the TOML
  parser's line and column. Nothing is half-applied.
- **Unknown keys.** Any key not listed on this page is logged as a warning
  and skipped. Fixed sections list their known keys explicitly; `[servers]`,
  `[forced-hosts]` and `[permissions.grants]` accept arbitrary names by
  design. Typos therefore cannot silently break your network, but they also
  cannot silently do what you meant — watch the warnings.
- **Validation.** After decoding, semantic checks run and every problem found
  is reported together:
  - `config-version` must be exactly `"0.1"`.
  - Addresses (`bind`, `[servers]` entries, `metrics.prometheus-address`)
    must be valid `host:port` with a port in `1..=65535`; IPv6 hosts must be
    bracketed (`[::1]:25565`). Hostnames must be DNS-valid: no underscores,
    labels of 1–63 bytes, no empty labels, at most 253 bytes, no leading or
    trailing hyphen per label. IP literals are accepted as-is.
  - Cross-references are checked: everything in `try` and in
    `[forced-hosts]` must name a server defined under `[servers]`.
  - Control characters are rejected everywhere; whitespace is rejected in
    keys where it makes no sense (server names, hostnames, group names).

## Root keys

These live at the top level of the file, before any `[section]`.

```toml
config-version = "0.1"
bind = "0.0.0.0:25565"
motd = "<aqua>A Candyriya network"
```

**(string, default `"0.1"`) — `config-version`.** Schema version of the
file. The only accepted value is `"0.1"`; anything else is a validation
error and the proxy refuses to start. This key exists so future schema
changes can be detected explicitly instead of misinterpreted. Do not change
it unless the documentation for a new release tells you to.

**(string, default `"0.0.0.0:25565"`) — `bind`.** Address the proxy listens
on. `0.0.0.0` binds every IPv4 interface, `[::]` binds every IPv6 interface
(and, depending on the OS, IPv4 too). Pin a specific IP to restrict who can
reach the proxy, for example `127.0.0.1:25565` for a localhost-only test
setup. The port must be in `1..=65535`; `0`, `65536` and non-numeric ports
are rejected at startup. An empty host, whitespace, control characters or a
hostname with underscores are rejected. IPv6 literals must be bracketed.
Interactions: `[query] port` defaults to the same port (UDP, so no clash
with the TCP listener); `enable-reuse-port` and `max-connections` change how
the listener behaves, not what it binds.

**(string, default `"<aqua>A Candyriya network"`) — `motd`.** Message shown
in the server list ("multiplayer" screen). Formatting is MiniMessage only;
`§`-codes and hand-built JSON are not supported. Newline escapes
(`\n`) need the TOML escape. Control characters are rejected at load time;
unbalanced or unknown MiniMessage tags are a rendering concern, so keep the
input simple. Interactions: `show-max-players` controls the number shown
next to the MOTD; neither affects the actual connection cap.

**(integer, default `100`) — `show-max-players`.** The `max` value reported
in the ping response. Must be at least `1` (`0` is a validation error).
Cosmetic only — the real connection cap is `[advanced] max-connections`
(default `1024`). Set it to whatever you want the server list to display.

**(boolean, default `true`) — `online-mode`.** When `true`, connecting
players must prove ownership of a Mojang account through Mojang's session
servers; offline accounts cannot join. When `false`, any name joins and no
Mojang authentication happens; skins and properties are not fetched. The
value must match every backend: with `online-mode = true`, backends must
run with `online-mode=false` and trust the proxy through forwarding (see
`player-info-forwarding-mode`). A backend left in online mode rejects
proxied logins with the classic "Failed to verify username!" kick.
Interactions: `force-key-authentication` (signed-chat requirements) and the
backend's own `online-mode` — all three must agree. See the installation
guide for the full online/offline walkthrough.

**(boolean, default `true`) — `force-key-authentication`.** Require the
signed chat/profile keys that 1.19+ online clients send. Turn it off only on
offline networks (set `online-mode = false` first); leaving it on in offline
mode rejects every login. There is no validation tying the two keys
together, so this is on you.

**(string enum, default `"modern"`) — `player-info-forwarding-mode`.** How
the proxy proves to backends that it is the one talking to them. Accepted
values: `modern` (backend pulls the profile over a plugin messaging channel,
HMAC-signed with the forwarding secret; recommended, 1.13+ backends),
`bungeeguard` (legacy wire format plus a `bungeeguard-token` property
carrying the secret), `legacy` (unauthenticated inline forwarding; anyone
who can reach the backend port can forge logins), `disabled` (no
forwarding; backends see the proxy's own address). Any other string fails
to decode and the proxy does not start. Interactions: `modern` and
`bungeeguard` require `forwarding-secret-file` to resolve; `legacy` requires
`spigot.yml bungeecord=true` on the backend; with `disabled`, backends see
every player as the proxy. Details and backend requirements in the
installation guide.

**(string, default `"forwarding.secret"`) — `forwarding-secret-file`.** Path
of the file holding the 32-byte forwarding secret, base64-encoded. A
relative path resolves against the directory containing this configuration
file; an absolute path is used as-is. If the file does not exist, a fresh
32-byte secret is generated, base64-encoded and written with owner-only
permissions (mode `0600` on unix; on Windows the file is created with the
directory's default ACL and a loud warning is logged — tighten the ACL
yourself). If the file exists but is not valid base64, or decodes to
anything other than exactly 32 bytes, startup fails with a secret error —
silently replacing a secret would strand every backend that trusts it. The
secret never rotates implicitly: reloading keeps the existing secret, and
deleting the file plus restarting is the rotation procedure. Control
characters and empty/whitespace-only values are validation errors. Keep the
file private; it authenticates this proxy to every backend.

**(boolean, default `false`) — `prevent-client-proxy-connections`.** Drop
connections whose handshake looks like a proxied Bungee/Velocity address
from a client that is not a trusted proxy. Cheap anti-abuse for public
instances; leave it off for normal deployments, where the backend checks do
the real work. With an unauthenticated mode (`legacy`) this matters more
than usual.

**(boolean, default `true`) — `enable-player-address-logging`.** Log the
real client address on connect instead of the proxy's own view. Turn it off
if your log retention policy forbids storing client IPs; debugging
connection issues becomes harder.

**(boolean, default `false`) — `announce-forge`.** Announce Forge server
affinity so Forge clients see the proxy as an acceptable endpoint instead of
bailing on version checks. Enable only if your backends are Forge/hybrid
and your players use Forge clients.

**(boolean, default `false`) — `kick-existing-players`.** When a new
connection claims the same identity as a connected player (same UUID in
online mode, or the derived offline UUID), kick the already-connected player
and let the new one in. When `false`, the new login is rejected and the
old session keeps playing. Useful against stuck sessions; enabling it makes
account sharing trivially easy on offline networks.

**(string, default `"en"`) — `locale`.** Locale used for server-generated
messages. Must be 1–32 characters of lowercase `a-z`, digits, `-` or `_`
(e.g. `en`, `pt-br`); anything else is a validation error. Only `en` ships
by default; unknown values fall back to English strings rather than failing.

## [protocol]

The protocol registry maps client protocol versions to the packet IDs the
proxy uses for status and login handling. It ships embedded in the binary;
this section controls whether the proxy refreshes it from upstream on
startup. Role: keep version tables current without a proxy upgrade.

```toml
[protocol]
auto-update = true
```

**(boolean, default `true`) — `auto-update`.** When `true`, the proxy checks
`update-url` on startup and, if a newer protocolVersions.json is published,
replaces the embedded registry. Fetching is best effort: on network failure,
validation failure or a timeout the embedded registry is used and a warning
is logged; the proxy never fails to start because of an unreachable or
hostile URL. Requires outbound HTTPS from the host on first start. Set to
`false` on air-gapped hosts or where you want bit-for-bit reproducible
behavior.

**(string, default `"https://raw.githubusercontent.com/PrismarineJS/minecraft-data/master/data/pc/common/protocolVersions.json"`) — `update-url`.**
URL the registry is fetched from. Must be an absolute `http(s)` URL without
whitespace or control characters (validation error otherwise). Change with
care: the payload is validated (structure and version pairing) before it can
replace the embedded registry, and anything that fails validation is
rejected wholesale — a bad mirror degrades to a warning, not to a bricked
proxy. Interactions: only used when `auto-update = true`.

## [servers]

Named backend servers and the order in which they are tried. Role: this is
the entire backend topology of your network — who players connect to, and
where a player goes when their current server drops them.

```toml
[servers]
try = ["lobby"]
lobby = "127.0.0.1:25565"
minigames = "127.0.0.1:25566"
```

**(array of server names, default `[]`) — `try`.** Ordered fallback chain:
when a player's server is unreachable or kicks them, the proxy walks this
list in order and moves the player to the next entry; when the chain runs
out, the player is disconnected with the configured kick message. Required
to be non-empty whenever at least one server is configured — otherwise
startup fails with a validation error. Every name in `try` must exist as a
server entry below (validation error otherwise). `try` is a reserved key:
it can never name a server entry. The file order of the server entries
themselves carries no meaning; only `try` (and `[forced-hosts]`) determines
routing order. Interactions: `[advanced] failover-on-unexpected-server-disconnect`
governs whether backend-initiated disconnects trigger the walk;
`[forced-hosts]` can override the *initial* routing decision per hostname,
but fallback still walks `try`.

**(string, no default) — `<name> = "host:port"`.** One entry per backend,
e.g. `lobby = "127.0.0.1:25565"`. Server names must be non-empty and
contain no whitespace or control characters. Addresses are validated like
`bind`: `host:port`, port `1..=65535`, bracketed IPv6, DNS-valid hostnames
or IP literals — anything else is a validation error. Names are unique by
construction (TOML forbids duplicate keys). A name whose value fails to
parse is a validation error naming the offending entry.

## [forced-hosts]

Hostname → server list map, for `play.example.com`-style splits. Role:
override the initial routing target based on the hostname the client
connected to, before any `try` fallback logic is consulted.

```toml
[forced-hosts]
"play.example.com" = ["lobby"]
"*.minigames.example.com" = ["minigames", "lobby"]
```

**(table, `hostname = [servers]`, default `{}`) — `<hostname>`.** Each key is
a hostname pattern; each value is the ordered list of servers to route a
player to when they connect via that hostname. Hostnames must be non-empty
and contain no whitespace or control characters; the server list must not be
empty; every referenced server must exist under `[servers]` (validation
error otherwise). The configuration layer stores patterns verbatim — the
matching itself happens in the router:

- `*` matches any host. This is the catch-all and the least specific
  pattern.
- `example.com` matches only that exact host.
- `*.example.com` is a single-label wildcard. `a.example.com` matches;
  `a.b.example.com` does not (that is two labels where the `*` allows one);
  `example.com` does not match either.
- Matching is case-insensitive, trailing dots are stripped, and there is no
  IDNA/punycode conversion — patterns and hosts compare as raw lowercased
  UTF-8.
- Patterns are ranked by kind: exact beats wildcard, wildcard beats `*`.
  Ties are broken by declaration order — the earlier entry wins.
- A `*` anywhere other than as a whole first label (`a.*.com`, `*.*`,
  `ex*mple.com`), empty labels (`a..com`), duplicates that normalize to the
  same thing, and entries with an empty server list are rejected when the
  routing table is built — the proxy fails fast instead of silently not
  routing players. (Empty server lists are also rejected at config
  validation, before the router sees them.)

Interactions: `[servers] try` — the forced-hosts table picks the *initial*
server; if that server drops the player, the fallback chain is `try`, in
order, not the forced-hosts list.

## [advanced]

Knobs most operators never need to touch: compression, timeouts, caps and
feature flags. Role: the behavioral guts of the connection pipeline —
throughput (compression), liveness (timeouts), survivability (caps and rate
limits) and protocol plumbing (channels, transfers).

```toml
[advanced]
compression-threshold = 256
max-connections = 1024
```

**(integer, default `256`) — `compression-threshold`.** Packets smaller
than this many bytes are sent uncompressed; larger ones are zlib-compressed.
`-1` disables compression entirely (not recommended for real networks);
`0` compresses everything. Values below `-1` are a validation error.
Interactions: `compression-level` sets the effort; the threshold applies
symmetrically to both directions.

**(integer, default `-1`) — `compression-level`.** zlib compression level,
`-1..=9`. `-1` means the zlib default (6); `9` is the smallest output and
the slowest. Out-of-range values are a validation error.

**(integer, default `3000`) — `login-ratelimit`.** Milliseconds a login may
stay pending (player connected, login not completed) before it is dropped.
Useful against bot floods parking half-open logins. There is no enforced
lower bound — `0` is accepted and would effectively deny pending logins
instantly, so pick values in the low seconds and up.

**(integer, default `3000`) — `connection-timeout`.** Milliseconds without
traffic on an established connection before it is dropped. Vanilla-ish
default; lower it for twitchy DDoS protection, raise it if players behind
exotic NATs get cut. No enforced lower bound; `0` is accepted and would
time out any read immediately.

**(integer, default `30000`) — `read-timeout`.** Milliseconds a single read
on a connection may take before the connection is dropped. Bounds
per-read latency independently of the idle `connection-timeout`. No
enforced lower bound.

**(boolean, default `false`) — `haproxy-protocol`.** Wrap the first frame
of every accepted connection in the HAProxy PROXY protocol. Enable only
behind a load balancer that speaks it — every direct client will otherwise
send a handshake the proxy cannot parse and be dropped. Do not enable it on
a directly exposed listener.

**(boolean, default `false`) — `tcp-fast-open`.** TCP Fast Open for the
listener (Linux 3.7+). Accepted for forward compatibility: it currently
logs a warning and stays off, because enabling it requires a `setsockopt`
path with no safe API in the dependency stack. Setting it has no effect
today.

**(boolean, default `true`) — `bungee-plugin-message-channel`.** Answer the
legacy BungeeCord plugin messaging channel (`bungeecord:main`) so old
backends can talk to the proxy. Disable for strict networks that want only
modern channels.

**(boolean, default `false`) — `show-ping-requests`.** Log every server-list
ping. Verbose; useful when debugging a MOTD or favicon issue, noisy
otherwise.

**(boolean, default `true`) — `announce-proxy-commands`.** Announce the
proxy command set to clients (tab-complete). Turn it off to hide proxy
commands from the tab-complete dropdown on locked-down networks.

**(boolean, default `true`) — `failover-on-unexpected-server-disconnect`.**
When a backend disconnects a player unexpectedly (crash, restart, kick for
protocol mismatch), move the player to the next server in `try` instead of
kicking them. When `false`, every unexpected backend disconnect disconnects
the player from the proxy entirely. Interactions: `try` is the chain walked;
`[forced-hosts]` does not extend it.

**(boolean, default `false`) — `log-command-executions`.** Log every command
executed by players. Audit trail; noisy at scale. Log destinations follow
the normal log configuration.

**(boolean, default `true`) — `log-player-connections`.** Log player
connects and disconnects with real addresses. Turn off only if player-IP
logging forbidden on your network.

**(boolean, default `false`) — `accepts-transfers`.** Accept the
`minecraft:transfer` intent, letting other proxies in a trusted fleet hand
players off to this one. Off by default — enable only if every proxy in the
chain is yours. Interactions: works alongside
`player-info-forwarding-mode`; transfer senders are not authenticated by
this flag.

**(boolean, default `false`) — `enable-reuse-port`.** `SO_REUSEPORT` on the
listener: the kernel load-balances new connections across threads. Linux
and macOS only; on Windows the option does not exist, so a config asking for
it logs a warning and continues without it. Useful for multi-worker setups;
do not enable on a single-listener setup unless you know you want it — two
proxies can then bind the same port, which may surprise you.
Interactions: `runtime.worker-threads` — the benefit compounds with multiple
accept paths.

**(integer, default `50`) — `command-rate-limit`.** Milliseconds between
accepted commands per player. `0` disables the rate limit. Per-player,
not global.

**(integer, default `10`) — `tab-complete-rate-limit`.** Milliseconds
between accepted tab-complete requests per player. `0` disables the limit.
Per-player, not global.

**(integer, default `1024`) — `max-connections`.** Global concurrent
connection cap, enforced with a semaphore; when it is exhausted, new
connections are closed immediately. `0` is accepted and would reject every
incoming connection — never set it. Large values trade memory for capacity;
a reasonable value is your expected peak plus headroom. Interactions:
`show-max-players` (cosmetic), `max-new-connections-per-second` (rate
versus volume), `enable-reuse-port` and `runtime.worker-threads` (accept
throughput).

**(integer, default `5000`) — `handshake-timeout`.** Milliseconds of
silence on the initial handshake before the connection is dropped. Bounds
how long a slow or hostile client can hold an un-upgraded connection.
Accepted without lower-bound validation; `0` would drop every handshake
instantly.

**(integer, default `0`) — `max-new-connections-per-second`.** New
connections accepted per second. `0` (the default) means unlimited. Useful
as a second layer of flood protection on top of `max-connections`; set it to
something like your normal accept rate during a DDoS.

## [query]

GameSpy4 query — what `query.protocol` in `server.properties` exposes on a
dedicated server. Role: let monitoring panels and server lists query player
counts, and (optionally) plugin lists, over UDP. Disabled by default; enable
only on trusted networks — it leaks operational data.

```toml
[query]
enabled = false
port = 25565
map = "candyriya"
show-plugins = false
```

**(boolean, default `false`) — `enabled`.** Turn the query service on or
off. When off, nothing listens on the query port and the keys below are
innocuous.

**(integer, default `25565`) — `port`.** UDP port for the query service.
Must be in `1..=65535` (validation error otherwise). Defaults to the same
number as `bind`'s port, but UDP — no conflict with the TCP listener even
though they share the default. Change it if your firewall or a co-hosted
service needs the number free.

**(string, default `"candyriya"`) — `map`.** World map name reported in the
full query response. Must not be empty and must contain no control
characters (validation error otherwise). Purely informational.

**(boolean, default `false`) — `show-plugins`.** Report the plugin list in
the full query response. Additional information leak; enable only if you
are sure nothing on your network treats it as sensitive.

Interactions: none beyond `bind` (shared default port) and `enabled` being a
master switch.

## [permissions]

Permission provider selection and static group grants. Role: who can run
what. The proxy ships its own permission engine; this section picks the
provider that feeds it and, for the built-in `config` provider, defines the
static grants inline.

```toml
[permissions]
provider = "config"
default = "default"
grants = { admin = ["*"], moderator = ["candyriya.command.*"] }
```

**(string, default `"config"`) — `provider`.** Which permission provider to
use. `"config"` is the built-in static provider; plugins may install
database-backed providers and be named here instead. Must not be empty and
must contain no control characters (validation error otherwise). An unknown
name is reported by the provider when it loads, outside the config layer.

**(string, default `"default"`) — `default`.** Group assigned to players
with no explicit assignment. Must not be empty and contain no control
characters (validation error otherwise). The named group does not need to
exist in `grants` — an absent group simply grants nothing.

**(table, `group = [nodes]`, default `{}`) — `grants`.** Static
group → permission-node list. Structure is validated at load: group names
must be non-empty without whitespace or control characters; node strings
must be non-empty without control characters. Node *semantics* belong to the
provider. For the built-in engine: `*` matches every node, `foo.*` matches
`foo.bar` and `foo.bar.baz` but not `foo`; the most specific matching grant
wins; an exact deny wins over an allow at equal specificity; an unregistered
node with no matching grant defaults to deny. The config provider parses
grants when it loads and reports every invalid node at once, naming the
group and index. Interactions: `default` names the fallback group; commands
and plugins gate on their own nodes (e.g. `candyriya.command.*`); see the
Permissions page for the full node catalog and precedence rules.

## [metrics]

Prometheus exporter. Role: let your monitoring scrape the proxy. Off by
default; when on, the proxy serves an HTTP `/metrics` endpoint.

```toml
[metrics]
prometheus = false
prometheus-address = "0.0.0.0:9124"
```

**(boolean, default `false`) — `prometheus`.** Enable the exporter. When
off, no listener is opened and `prometheus-address` is unused.

**(string, default `"0.0.0.0:9124"`) — `prometheus-address`.** Listen
address (`host:port`) for the exporter. Validated like `bind`: port
`1..=65535`, bracketed IPv6, DNS-valid hostname or IP literal — anything
else is a validation error. Bind it to localhost or firewall it; the
endpoint exposes player and connection metrics without authentication.

Interactions: none structural; the exporter is independent of the proxy
listener and of the log level.

## [runtime]

Thread pool sizes. Role: how many cores the proxy is allowed to burn. Both
pools default to the number of logical CPUs — the right answer for almost
everyone — which is why these keys are commented out in the shipped file
and absent from it unless you add them.

```toml
[runtime]
worker-threads = 8
blocking-threads = 8
```

**(integer, default number of logical CPUs) — `worker-threads`.** Tokio
worker threads for async I/O and the packet pipeline. `0` is a validation
error. Sizing above the CPU count oversubscribes the runtime and hurts
latency; sizing below wastes cores the hot path could use. Only pin this
when you are partitioning cores deliberately (e.g. a dedicated proxy host
shared with a backup process).

**(integer, default number of logical CPUs) — `blocking-threads`.** Threads
in the fixed blocking (CPU-bound) pool — compression, crypto, HMAC, batch
serialization. `0` is a validation error. Keep this at or below
`worker-threads` unless profiling says otherwise. Both pools are fixed at
startup from these values — they are not automatically resized at runtime,
which is intentional: no surprise re-tuning under load.

Interactions: `[advanced] max-connections` (how much concurrency the pools
must absorb), `enable-reuse-port` (accept-side spreading), compression
settings (blocking-pool pressure).

## Hot reload

The loaded configuration lives behind an atomic snapshot (`arc-swap`).
Readers never block and never observe a half-updated configuration. A
reload re-reads the file from disk and swaps the new snapshot in atomically;
if anything fails — syntax, validation, secret resolution — the error is
returned and the previously running configuration stays fully intact. The
forwarding secret is read from the secret file when present; it is never
rotated implicitly by a reload (rotate by deleting the file and restarting).

## Troubleshooting configuration errors

**Port already in use.** Another process (often a previous candyriya or a
backend) holds `bind`'s port: the listener fails at startup with a bind
error, e.g. "address already in use". Stop the conflicting process or move
`bind` to a free port (and update your firewall). `SO_REUSEPORT` via
`enable-reuse-port = true` lets two candyriya instances share one port on
Linux/macOS by kernel load-balancing — it is not a fix for an unrelated
service holding the port, and it is ignored on Windows.

**Wrong interface to bind.** `bind = "127.0.0.1:25565"` makes the proxy
unreachable from the network — clients time out instead of getting
refused, and it looks like a proxy crash. Use `0.0.0.0` (or `[::]`) to
listen on every interface, or pin the one address players should connect
through. Validation rejects empty hosts, whitespace, control characters and
out-of-range or non-numeric ports before the process ever binds.

**Forwarding secret problems.** The secret file (`forwarding-secret-file`,
default `forwarding.secret`) must contain base64 that decodes to exactly 32
bytes. Anything else — truncated copy, wrong file, `0600` tightened away by
a careless `chmod`, CRLF-mangled copy through some text editor — fails
startup with a forwarding-secret error instead of "just working", because
silently replacing it would disconnect every backend that trusts it. On
unix the file is born with mode `0600`; on Windows it inherits the
directory's ACL and the proxy logs a warning — keep the data directory
user-private. The bytes in the file must match what every backend has in
`proxies.velocity.secret` (exact string, including base64 padding). To
rotate: delete the file, restart the proxy, update every backend.

**`online-mode` mismatch between proxy and backend.** With the default
`online-mode = true`, backends must run `online-mode=false` in
`server.properties` and trust the proxy via forwarding (`proxies.velocity.enabled=true`
plus the secret in `paper-global.yml`). A backend left in online mode
rejects every proxied login with "Failed to verify username!" — per player,
no matter what the proxy does. The mirror failure exists too: an offline-mode
proxy paired with an online-mode backend that does forwarding correctly will
still work, but premium players are not actually verified. Keep the three
values (`online-mode`, `force-key-authentication`, backend forwarding)
consistent and deliberate.

**`try` missing the fallback you expect.** The fallback walk happens over
`try` and nothing else: if the server you wanted as a fallback is not in
`try`, it is never tried — players whose backend dies are disconnected when
`try` is exhausted, even though a healthy backend sits in `[servers]`
unreachable from the chain. `try` must be non-empty whenever any server is
configured (validation error), and every name must exist under `[servers]`
(validation error) — both failures happen at startup, but a wrong-but-valid
`try` order fails silently at runtime. `[forced-hosts]` only steers the
*initial* route; it never extends the fallback chain.

## Related docs

- [Installation](installation) — building, running, systemd/Docker units and
  the forwarding setup walkthrough
- [Permissions](permissions) — permission nodes, the grant engine, contexts
  and wildcards; the nodes referenced in `[permissions]`
- [Plugins](plugins) — the WebAssembly plugin system and its manifest
- [Commands](commands) — built-in commands, including `/candyriya`
- [Architecture](architecture) — workspace layout and design notes
