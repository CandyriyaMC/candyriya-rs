# ADR-0001: Native Plugin Controller in the supervision tree — lifecycle as a state machine, IPC-linked plugin units

**Status:** Accepted (tracked in [#310](https://github.com/CandyriyaMC/candyriya-rs/issues/310)) · **Date:** 2026-11-18

## Context

The proxy loads native plugins as dynamic libraries (`.dll` / `.so` / `.dylib`) into its own address
space via `dlopen`/`LoadLibraryW`. There is no process boundary between proxy and plugin code, and the
WASM runtime (extism/wasmtime) coexists during phases 1–3. Phase 1 landed the `extern "C"` ABI
(`candyriya-native-abi`, zero host functions, reference `crates/plugins/greeting`).

The supervision tree (`candyriya-init`) already provides `plugins` group units, `plugin-<id>` units,
capability IPC, bounded mailboxes, a reaper with restart policies, and `ps`/`tree` rendering — but it
knows nothing about plugin lifecycle semantics. Something must own the machine so the tree supervises
real units instead of log lines. This ADR records the phase-2 decision only; the full blueprint —
commit sequence, transition table, wire types, crash semantics, open questions OQ1–OQ19 — is
`docs/phase2-implementation-plan.md`, deliberately not duplicated here.

## Decision

**A `plugin-controller` service unit drives the 11-state machine over capability IPC; native plugins
run on dedicated OS threads; phase 4 will retire the WASM path.**

Concretely:

- One new crate, `candyriya-plugin-controller`, owns `PluginRecord` and is the *only* writer of
  lifecycle state. Plugin units and loaders report; they never decide (single-writer discipline).
- The machine is exactly `DISCOVERED → LOADING → LOADED → INITIALIZING → INITIALIZED → ENABLING →
  RUNNING → DISABLING → DISABLED → UNLOADING → UNLOADED`. No `FAILED` state: failure is a recorded
  reason plus the terminal path through `UNLOADING → UNLOADED`.
- One call path: controller → tree IPC (`PluginMsg`/`PluginResp`, type-erased, generation-tagged) →
  plugin unit → plugin OS thread `candyriya-plugin-<id>`. Native plugin units are service units parented
  to the controller, so ancestor-rule authorization applies — no kernel grants, no kernel changes.
- OS-thread rules inherited from the WASM model: never on tokio/rayon workers, bounded
  `sync_channel(16)` mailbox, `catch_unwind` at the FFI boundary both directions.
- Every transition is bounded (10s lifecycle deadlines, 5s join on unload) and acked
  `{generation, status, duration_ms}`; stale-generation acks are dropped, never honored. No retry inside
  the machine — failure lands on the failure path of the current state.
- Phase 2 ships `RestartPolicy::Never` for native plugin units (plan OQ3): no automatic restage; the
  operator retries via reload or reboot.
- Hot reload reuses the stage-then-swap shape (staged replacement while the old instance keeps running;
  stage failure leaves it untouched) (plan OQ6).
- WASM plugins are untouched in phases 2–3 and deleted in phase 4, per #310. Both lifecycle machines
  coexist deliberately until then; no unification is attempted.

## Consequences

**Positive**

- `ps`/`tree` show real units: `plugins ─ plugin-controller ─ plugin-<id>`; failure is visible
  (`Zombie` → reaped, `FailedTerminal`) instead of silent.
- Zero new kernel machinery — the controller is an ordinary service unit; ancestry authorizes
  controller→plugin IPC.
- Single-writer machine + generation tags + deadlines make every step bounded and stale acks harmless.
- Native plugins reuse the tree's supervision, reaper, metrics, and hot-reload shape instead of a
  parallel orchestration layer.

**Negative**

- Two lifecycle machines coexist during phases 2–3 (WASM 3-state vs native 11-state) for the same
  operator verbs; `ps` conflates them until phase 4 (mitigation: flavor caption on units).
- No memory isolation: the tree supervises exits and cells, not memory. A panic is caught at the FFI
  boundary; a segfault or constructor abort (`DllMain`/`.init`) is process death — before any supervisor
  sees a unit. Accepted, documented limitation.
- No fuel/epoch: a wedged native plugin is detectable only by missed deadlines; join timeout detaches
  the thread and leaves the mapping mapped — one core burns forever (`unloaded-detached`).
- `disable_failed` latch: a failed disable never auto-advances to unload; the plugin stays resident
  until the operator acts.

**Risks**

- Exit semantics: a native plugin crash exits the whole proxy non-zero; the tree cannot intercept it;
  sessions are lost, and systemd `Restart=always` (if configured) restarts the proxy, not the plugin.
- systemd readiness: the reference unit is `Type=simple`; systemd sees only process liveness — never
  plugin units, health cells, or the 11-state machine. Phase 2 wires no `sd_notify`/watchdog; plugin
  health is proxy-internal (`ps`, health cells) until a separate decision says otherwise.
- Open decisions gate *implementation*, not this decision, and are tracked in the plan: 11 vs 12 states
  (OQ1), the V1 `on_disable`/`on_unload` probe gap (§10.1), reload shape (OQ6), `/candyriya plugin` CLI
  scope (OQ7), ABI freeze sign-off and unsafe allow (OQ8).
