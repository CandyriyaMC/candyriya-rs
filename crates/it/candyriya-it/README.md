# candyriya-it

Integration scaffolding for the candyriya workspace: a **real**, loopback,
server-side TCP backend and a real client speaking the Minecraft Java
Edition protocol over it, built on `candyriya-protocol` and
`candyriya-protocol-registry`. The loopback suite (`tests/integration.rs`)
runs everywhere and touches nothing but `127.0.0.1`.

## The Paper smoke test

`tests/smoke_paper.rs` is the end-to-end check of the whole stack:

* downloads (once, cached in `target/smoke/`) and SHA-256-verifies
  **Paper 1.21.1 build 133** — the exact object is pinned by URL and checksum,
  and any mismatch fails the run;
* materializes a fresh backend from the committed `paper-server/` template
  (velocity modern forwarding enforced, `online-mode=false`, a per-run secret
  substituted for the committed `SMOKE_SECRET_PLACEHOLDER` — no secret ever
  lands in git);
* launches Paper (`java -Xmx1G -jar … nogui`) and the **real `candyriya`
  binary** (built with `cargo build -p candyriya`), the proxy on a
  pseudo-terminal so its interactive console behaves like a human's terminal
  (it even answers the device-status-report query the way a real terminal
  does);
* drives a real client through the proxy: handshake, login, and an exact
  UUID assertion against the vanilla offline-mode derivation;
* checks the status ping end to end (configured MOTD, max players, protocol
  echo, ping payload echo);
* checks the backend log records the forwarded profile;
* runs `/candyriya info` over the pty console and checks its output;
* shuts both processes down the way they support (`/candyriya end` on the
  proxy console, `stop` on Paper's stdin) and requires a clean exit from
  each.

The jar, the world, the logs and both generated run directories live under
`target/smoke/` and are never committed. The proxy binary is *not* built by
the test — build it first.

### Running it

Prerequisites:

* Java 21+ on `PATH`;
* the proxy binary (`cargo build -p candyriya`) — or point
  `CANDYRIYA_SMOKE_BIN` at one;
* ~49 MB of network access on the first run (the jar is cached afterwards);
* a free port 25565 (backend) and 25569 (proxy).

```shell
CANDYRIYA_SMOKE=1 cargo test --features smoke -p candyriya-it --test smoke_paper -- --test-threads=1 --nocapture
```

The test is gated twice: the `smoke` cargo feature (without it the test is
`#[ignore]`d and does nothing — `cargo test --workspace --all-features` stays
green without any network or JVM) and the `CANDYRIYA_SMOKE=1` environment
variable. Progress is printed to stderr; on failure the last lines of the
child logs are shown.

Artifacts of the last run are kept under `target/smoke/run-backend/` (world,
`logs/latest.log`) and `target/smoke/run-proxy/` (`candyriya.toml`,
`logs/`) for debugging.

### Deviations from the ideal

* **Proxy `online-mode` is `false` in the smoke.** Online mode verifies the
  client via `hasJoined` against Mojang's session server, which cannot
  succeed without a real Microsoft account. Offline mode is the only
  configuration in which "the echoed UUID equals the vanilla offline-mode
  derivation" is meaningful, and it still exercises the modern forwarding
  path (the risky part) end to end, including Paper's HMAC verification.
* **`show-max-players` is top-level** in this proxy's config schema
  (`[advanced]` has no such key), so the smoke sets it at the root; an
  `[advanced] show-max-players` line would be silently ignored.
* **`try` lives inside `[servers]`**, not at the root — the root-level form
  would be treated as an unknown key and validation would fail.
* **The ping is answered by the proxy itself**; this proxy never forwards
  status requests, so there is no backend ping to observe. The assertion
  covers everything a client-visible ping can observe.
* **No literal SIGTERM.** Windows does not deliver one, and on Unix the
  proxy only breaks out of its main loop on Ctrl-C (SIGINT). The smoke stops
  each process the way that process actually supports, and asserts a
  self-driven clean exit with code 0 for both.
* **The client does not complete the post-1.20.2 configuration handshake**
  (a full configuration-state implementation is out of scope), so Paper
  never logs "logged in with entity id". Instead the smoke asserts the
  strongest available marker: Paper's `UUID of player Steve is <forwarded
  uuid>` line, which is printed only after the modern-forwarding HMAC
  verified against the shared secret.
* On hosts without a working MSVC linker or GNU binutils, building
  `candyriya` locally may not work (CI is the source of truth); on this
  machine a portable MinGW-w64 (winlibs) on `PATH` is what made the local
  smoke possible.
