# Phase 2 implementation plan — native plugins: Plugin Controller + state machine in the tree + hot reload

Status: plan for lead approval. Nothing in this document is merged code. Ground truth verified
against `experiment/native-plugins @ 640cda5` (phase 1 merged: `candyriya-native-abi` + `greeting`
cdylib, CI native-abi job on ubuntu/windows/macos). All phase-2 commits land on
`experiment/native-plugins`; this file lives on `feat/native-phase2-prep` (throwaway prep branch).

- **Phase 1 (done):** `candyriya-native-abi` — the `extern "C"` contract: `#[repr(C)]`
  `CandyriyaBuf` (borrowed `(ptr, u32)`), `PluginMetadata`, `PluginInitArgs{abi_version=1, api: *const CandyriyaPluginApiV1}`,
  `PluginInitOutput{status, abi_version, metadata}`, `CandyriyaStatus` (fail-closed, non-exhaustive),
  `CandyriyaPluginApiV1{abi_version, size: size_of::<Self>(), instance: CandyriyaPluginHandle}` —
  **zero host functions, frozen**. Exports: `candyriya_plugin_init`, `candyriya_plugin_on_load`,
  `candyriya_plugin_on_enable`. **No `on_disable`/`on_unload` exports exist in V1** — see §10.1, the
  single biggest blocker. `crates/plugins/greeting` — the reference cdylib, prints one line on
  `on_enable`. CI native-abi job builds the `.dll/.so/.dylib` and runs the contract tests on 3 OSes.
- Issue: #310. This plan covers phase 2 only: Plugin Controller crate, state machine driven through
  `candyriya-init`'s supervision tree, hot reload for native plugins, native units in `ps`/`tree`,
  implementation-level docs. Everything else is phase 3/4 (see §8).

## Table of contents

1. Goals recap — what "phase 2" means concretely
2. Crates to bring into the workspace
3. Commit sequence (landing order, on `experiment/native-plugins`)
4. The plugin lifecycle — transitions, owners, ps correspondence, crash semantics
5. What gets copied from the WASM implementation — and what dies in phase 3/4
6. Plugin threading model for native plugins
7. Performance characteristics — where the ABI boundary is, BORROW vs CLONED
8. Explicitly out of scope for phase 2
9. The first phase-2 agent to launch (cold-start brief)
10. Unresolved questions that must land answers before implementation
11. Deviations from the earlier design pass — what lives here vs #310

## 1. Goals recap — what phase 2 means concretely

From issue #310, phase 2: *"Plugin Controller crate: state machine `DISCOVERED → LOADING → LOADED → …
→ RUNNING → UNLOADED` fully driven through `candyriya-init`'s tree; hot reload reuses the existing
control machinery; plugin units land in ps/tree."* Unpacked:

- **Plugin Controller** — a supervised service unit named `plugin-controller`, child of the existing
  `plugins` group unit (which exists today, LongLived, `Never`, pending body, `ServiceKind::Plugins`).
  It owns the dispatcher-facing lifecycle of every native plugin. It is the *only* writer of the
  machine state (single-writer discipline); plugin units and loaders report, they never decide.
- **State machine in the tree** — the 11 states are not a log line, they are tree-visible reality:
  each native plugin is a `plugin-<id>` unit whose lifecycle body forwards `PluginMsg::Verb` to the
  plugin's OS thread and awaits a bounded ack. Units show up in `ps`/`tree` like any other unit;
  failure surfaces as `UnitState::Zombie`/`Restarting`/`FailedTerminal` per the existing reaper.
- **Hot reload for native plugins** — `/candyriya reload plugins` and the `plugin-controller`'s
  `Reload` verb reuse the *stage-then-swap* shape of the existing `PluginReloader` (stage completely,
  swap atomically, old keeps running on stage failure), adapted: the stage is a separate `dlopen` of
  the *new* library plus a double-context window, not a wasm compile. The WASM path stays untouched.
- **What phase 2 is NOT** — no host functions (the V1 table stays empty), no event dispatch into
  native plugins, no `/candyriya plugin` CLI unless the lead confirms it (issue letter says phase 2,
  OQ7 — see §8 and §10.5), no package manager, no WASM removal (phase 4).

Done means: boot on `experiment/native-plugins` discovers `crates/plugins/greeting` (and nothing
else, since no other native plugin exists), drives it `DISCOVERED → … → RUNNING` through the tree,
`/candyriya reload plugins` swaps it to a restaged build without a process restart, `/candyriya ps`
and `tree` show `plugins ─ plugin-controller ─ plugin-greeting`, and a deliberate panic/hang in the
plugin produces `FailedTerminal`/`unloaded-detached` honestly instead of a hung proxy (to the extent
the native model allows — see §4.5).

## 2. Crates to bring into the workspace

| Crate | Path | Scope (one sentence) | Unsafe? |
|---|---|---|---|
| `candyriya-plugin-controller` (NEW) | `crates/plugin/candyriya-plugin-controller` | The supervisor: `PluginRecord` + the 11-state machine, `PluginMsg`/`PluginResp` wire types, `CtrlMsg` operator surface, the health sweep (`TaskService::every`), controller unit wiring, `sync_native_plugin_units`. | **No** — `#![forbid(unsafe_code)]` |
| `candyriya-native-instance` (NEW) | `crates/plugin/candyriya-native-instance` | The dlopen boundary: `libloading` open/close (`RTLD_NOW`+`RTLD_LOCAL` unix / `LoadLibraryW` windows), stage-copy-under-`temp/` reload staging, symbol resolution (`candyriya_plugin_init` + optional probed exports), the per-plugin dedicated OS thread + bounded `sync_channel(16)` mailbox, `catch_unwind` both directions, `dlclose`-only-after-join. | **Yes** — the only new unsafe crate; crate-local `#![allow(unsafe_code)]` with explicit lead sign-off (OQ8) |
| `candyriya-plugin-sdk` (NEW, optional in phase 2) | `crates/plugin/candyriya-plugin-sdk` | The safe façade over the ABI for plugin authors: `trait Plugin { on_load/on_enable/on_disable/on_unload }`, panic-catching, `#[unsafe(no_mangle)]` export boilerplate. | Tiny (export wrappers only) |
| `candyriya-plugin` | `crates/plugin/candyriya-plugin/` | **EXTEND, not rewrite**: side-effect-free `discover() -> DiscoveryReport{records-precursor, issues}` — scan for `.so/.dll/.dylib` next to `plugin.toml`; reuse `manifest.rs`, `dep_resolution.rs`, `validate_unique` as-is. WASM path untouched. | No |
| `candyriya-metrics` | `crates/common/candyriya-metrics/` | **ADD constants only**: `candyriya_plugin_lifecycle_total{verb,outcome}`, state gauge, stuck-unloaded counter; register in `names.rs` + ALL_NAMES test. | No |
| `candyriya-native-abi` | (exists) | The contract. Read, never modified in phase 2 except the README "optional probed exports" note (if §10.1(a) lands). | Yes (crate-local, phase 1) |
| `candyriya-init`, `candyriya-scheduler` | (exist) | **UNCHANGED.** Service units + capability IPC suffice; controller is a service unit; native plugin units are service units (parent = controller). The kernel's `UnitKind::Plugin` and `ServiceKind::Plugins` already exist. | No |
| `candyriya-plugin-api` | (exists) | **UNCHANGED.** Compat layer stays for WASM through phase 3; replacement is entirely phase 3/4. | No |
| `candyriya-config` | (exists) | **UNCHANGED.** `[plugins]` stays wasm-phrased (docs land phase 3); phase-2 timeouts/deadlines are code constants, not TOML knobs (OQ10). | No |

Not brought: no second dynlib crate (libloading is the only one, pin ONE version, deny.toml already allows ISC and
`multiple-versions="deny"`); no `windows-sys`/`libc` direct calls (libloading wraps both); no new serialization crate
(wire types are internal type-erased envelopes, `host_wire.rs` pattern); no changes to `tokio`, `rayon`, `TaskService`.

## 3. Commit sequence (in order, on `experiment/native-plugins`)

Each commit is one logical unit, Conventional Commits, nothing else in the tree touched. Agents
(proposed, sizes ~1-2 days each):

1. **`feat(controller): PluginRecord + 11-state machine (pure)`** — the controller crate skeleton:
   `PluginWireState` (11 variants, no `Failed`), `PluginRecord{id, origin, flavor(Native|Wasm),
   manifest, issues, state, generation: u64, reason: Option<String>}`, the transition table
   (`apply(record, event)`), deadline stamps, generation-bump-per-spawn, `disable_failed` latch.
   No dlopen, no unsafe, no `candyriya-init` — pure logic + unit tests. *(A36)*
2. **`feat(controller): PluginMsg/PluginResp + AckError + CtrlMsg wire types`** — internal,
   type-erased, closed variant sets, `AckError{code, message ≤ 1 KiB}`, `OUTCOME_*`-style bounded
   labels only (never per-id IPC labels); metrics names land in `names.rs` + ALL_NAMES green. *(A36)*
3. **`feat(plugin): side-effect-free native discovery pass`** — `discover()` in `candyriya-plugin`:
   find `<id>/<id>.<so|dll|dylib>` (or platform-mapping fallback) next to `plugin.toml`, flavor field
   (`Native|Wasm`), native-wins-with-WARN when both exist (OQ13), no instantiation, no thread. *(A37)*
4. **`feat(native-instance): dlopen boundary and dedicated-thread instance`** — the only unsafe crate:
   `candyriya_native_instance::NativeInstance::load(path, PluginInitArgs)` — dlopen/LoadLibraryW,
   resolve `candyriya_plugin_init`, handshake (`abi_version`/`size`), mint table + handle, call
   `on_load`/`on_enable`, plus the mailbox thread shape of `instance.rs` (`ManagedCall{export,
   respond}`, `catch_unwind`, close-on-drop). Windows stage-copy under
   `<data_dir>/temp/native-reload-<id>-<gen>.dll` (OQ11). Join with 5s timeout → detach. *(A38)* —
   **gated on OQ8 (freeze review + unsafe sign-off) and the §10.1 ratification.**
5. **`feat(controller): controller service unit + native plugin units in the tree`** — controller as
   `plugin-controller` service unit under the `plugins` group (parent rule: no grants needed);
   native plugin units as service units under the controller (`UnitKind::Plugin`,
   `ServiceKind::Plugins`, `RestartPolicy::Never` for phase 2 per OQ3); `sync_native_plugin_units`;
   `PluginMsg` forwarding + ack await with 10s deadlines; stale-ack rejection by generation;
   zombie/reap via existing sweeps; `/candyriya ps` renders `plugins ─ plugin-controller ─
   plugin-<id>`. *(A38/A39)*
6. **`feat(controller): health cells for native + 1s sweep`** — `watch<bool>` per id raised at the FFI
   boundary on caught panic (native has no fuel/epoch); unit parks on cell → exit `Failure` →
   visible; `TaskService::every(1s)` ping sweep (`PluginMsg::Health`) for deadline-miss detection;
   ERROR logged once per unrecoverable transition. *(A39)*
7. **`feat(controller): native reload — stage-then-swap`** — `Reload` as staged replacement: new lib
   staged (copy under `temp/` on Windows), dlopen + handshake + `on_load`+`on_enable` on a *staging*
   instance while the old one keeps running; swap under dispatcher quiescence for that id; stage
   failure ⇒ old untouched; double-instance window bounded by a per-id budget (max 1 staged pair).
   Enable/disable preserved across swaps; plugin-owned `ServiceRegistry` entries revoked in bulk on
   swap (verify the `Any` downcast story at review; otherwise revocation defers to phase 3). *(A40)* —
   **gated on OQ6.**
8. **`feat(cli): /candyriya plugin list|info|enable|disable|unload|reload|inspect`** — brigadier
   nodes, i18n keys (en + ex_girl, symmetry test green), `CtrlMsg` wired over tree IPC (controller is
   a service unit, like the `host` unit precedent), ps `--full` tombstones with `!`. *(A41)* —
   **gated on OQ7; if declined, this commit drops out and the CLI is handed to phase 3.**
9. **`test(it): native lifecycle end-to-end against the greeting cdylib`** — build the greeting
   cdylib, boot the composition root, assert the full machine reaches RUNNING, disable/unload returns
   to UNLOADED, `/candyriya reload plugins` swaps a restaged build, a panic fixture flags the health
   cell and the unit exits `Failure`. Mirror `tests/plugin_units.rs` and `tests/hot_reload.rs`;
   no network. *(A42)*
10. **`ci: windows-latest dynlib smoke job`** — build `greeting.dll`, dlopen it, probe the handshake,
    unload it, on `windows-latest`; the whole dlopen/file-lock story is unverifiable on ubuntu today.
    *(A42)* — gated on OQ17 (accept or reject).
11. **`docs(architecture): native plugin units in the tree`** — `docs/architecture.md` tree section
    gains the controller subtree; `docs/plugins.md` "There is no native plugin mode, and never will
    be" line is replaced with an honest pointer (WASM until phase 4, native supported from phase 2);
    short implementation-level ADR (`.github/docs/native-plugins.md` or crate docs): the ABI
    contract, versioning rules, and the exact steps to write a plugin. **End-user plugin docs are
    NOT written this phase (§8).** *(A43)* — gated on OQ14.
12. **`chore(native-instance): pin libloading =0.9.1 exactly`** (if not already pinned by commit 4;
    deny.toml is otherwise untouched). *(A38)*

Ordering rationale: 1→2 are contract-internal and unblock everything; 3 is independent; 4 is the risky
FFI piece and lands behind the freeze review; 5→6→7 wire the machine into the tree; 8 is user-facing
on top; 9 proves it; 10/11 close the loop. Phase-1 merge has already happened; **no phase-2 work is
pull-requested against `main`/`dev` — everything lands on `experiment/native-plugins` first.**

## 4. The plugin lifecycle — transitions across the supervisor tree

One writer: `plugin-controller`. One call path: controller → tree IPC → plugin unit → plugin OS
thread. Every transition bounded: load/init/enable/disable/unload = **10s**, join = **5s** (constants,
not config — OQ10). Every ack = `{generation, status, duration_ms}`; stale generation ⇒ drop + WARN
metric, never honored. **No FAILED state** (epic mandates 11); failure = `record.reason` + terminal
path through UNLOADING→UNLOADED.

### 4.1 Table (owner · crate · deadline · failure behavior)

| # | State | Owner (advances) | Executed in | Deadline | On failure |
|---|---|---|---|---|---|
| 1 | DISCOVERED | controller registry (no unit yet) | `plugin-controller` + `candyriya-plugin::discover()` | — (scan is sync) | Issues ⇒ `Failed` record, no unit; dep-cycle members all `Failed`; no auto-retry |
| 2 | LOADING | plugin unit spawned by controller (parent=controller) + plugin OS thread | controller drives; `candyriya-native-instance` executes | 10s | dlopen/symbol/handshake/ctor abort ⇒ `Err` ack ⇒ `Failed(reason)` ⇒ teardown (drop ctx, join, `dlclose`) ⇒ unit exits. NO retry |
| 3 | LOADED | controller (marker) | controller | instant | crash between ack and init ⇒ in-flight call fails ⇒ `Failed` ⇒ teardown |
| 4 | INITIALIZING | controller → unit → plugin thread | `candyriya-native-instance` | 10s | ack err/timeout/crash ⇒ `Failed`, **skip ENABLING**, teardown |
| 5 | INITIALIZED | controller (marker) | controller | instant | — (order bookkeeping: Kahn order, enable queue) |
| 6 | ENABLING | controller → unit → plugin thread | `candyriya-native-instance` | 10s | err/timeout/crash ⇒ `Failed`, **skip DISABLING** (nothing ran), teardown |
| 7 | RUNNING | controller (steady) + 1s `TaskService::every` sweep | controller | sweep 1s | runtime panic ⇒ health cell ⇒ unit exits `Failure` ⇒ `Failed` ⇒ **skip DISABLING** (thread may be a corpse) ⇒ UNLOADING |
| 8 | DISABLING | controller → unit → plugin thread | `candyriya-native-instance` | 10s | err/timeout/thread-dead ⇒ `disable_failed` latch ⇒ **NOT unloaded, no auto-advance**; plugin stays resident, ps-visible, operator decides (OQ2) |
| 9 | DISABLED | controller (steady) | controller | — | — (re-enable without reload; reload keeps disabled) |
| 10 | UNLOADING | controller → unit → plugin thread | `candyriya-native-instance` | 10s ack, 5s join | join timeout ⇒ **detach thread, never TerminateThread/pthread_cancel; skip `dlclose` permanently for this mapping**; record `unloaded-detached` |
| 11 | UNLOADED | controller (terminal) | controller + reaper | — | — (`ps` shows unit's final exit; lib file free to delete unless detached) |

Preconditions that are strict gates, not suggestions: LOADING requires all hard deps at least
INITIALIZED (enable-order gate: deps must be RUNNING — OQ19); ENABLING requires **all hard deps
RUNNING** (a failed/absent dep cascades dependents to `Failed`, never enabled); UNLOADING requires
DISABLED-confirmed (or "never enabled" marker) AND zero active callbacks AND drained mailbox.

Enable/disable state survives reload. "Disabled" ≠ "unloaded": disabled = thread idles on mailbox,
library mapped, context alive; unloaded = none of that.

### 4.2 Reload (phase 2)

`Reload(id)` is a *macro* over these transitions — `Disable → Unload → rescan → Load → Init → Enable`
— with the recommended v1 shape **stage-then-swap** (OQ6): the staged replacement is dlopen'd,
handshake'd and `on_enable`'d **while the old instance keeps RUNNING**; the swap happens under
dispatcher quiescence for that id; stage failure ⇒ old untouched (inherits `PluginReloader`'s failure
policy exactly). Cost: two live contexts briefly — bounded by a max-1-staged-pair budget per id
(the native analogue of `max_instances`). Alternative v1: plain macro with a bounded outage
(simpler, worse semantics: enable-of-new failure leaves the plugin ABSENT). **Decision needed: OQ6.**

### 4.3 ps / tree correspondence (there is no literal "RUNNABLE" in the tree)

| Machine state | `UnitState` | `ps` letter (`state_char`) | Severity | Notes |
|---|---|---|---|---|
| DISCOVERED…ENABLING (transitional) | `Running` (body parks on oneshot work) | `R` | 0 | `ps --watch` ticks during transitions |
| RUNNING | `Running` | `R` | 0 | kind caption stays `candyriya.kind.plugin` (`PsUnitKind::Plugin`) |
| RUNNING, thread flagged by health cell, unit about to exit Failure | `Running` + `parked` | `F` | 1 | transient; exit `Failure` is immediate, same cell shape as WASM units |
| Unloaded after failure | `Zombie` → reaped | `Z` | 2 | visible until reap; `reason` inline in `--full` |
| (Only if OQ3=OnFailure) backing off | `Restarting` | `t` | 3 | backoff 1s→30s jittered, 5 attempts / 10 min → `!` |
| `disable_failed` / stuck | `Running` | `R` + `!` tombstone in `--full` | 4 | record latch; operator action required |
| `FailedTerminal` (OQ3=OnFailure only) | `FailedTerminal` | `!` | 4 | start limit breached, stays visible forever |
| UNLOADED (clean) | `Zombie` → reaped | `Z` | 2 | final exit status shown once, then gone |

Restart counters/backoff: with the recommended phase-2 policy (`RestartPolicy::Never`, OQ3), no native
plugin unit ever enters `Restarting`; `RestartLimits` (base 1s, cap 30s, window 600s) simply never
applies. If OQ3 flips, the native restage path reuses exactly the existing windows and limits (no new
knobs), with restage = full `Reload` of that id, not the wasm `PluginReloader`.

### 4.4 Discovery cadence

Discovery runs (a) at boot (composition root spawn wiring, same pass as WASM), (b) at
`/candyriya reload plugins` (the same `resolve_plugins`-class pass, plus native candidates), and
(c) never automatically otherwise — a file appearing on disk is *not* a load event. Stray native
libraries (no manifest) are reported as issues, same semantics as stray `.wasm`.

### 4.5 Crash semantics (honest limits, to be documented in the ADR)

- **Panic inside plugin code** — caught by `catch_unwind` at the FFI boundary (SDK entries are
  mandatory-wrapped; the wrapper crate is the only place unsafe lives) → health cell → unit `Failure`
  → `Failed` → `UNLOADING` (skip `DISABLING` for a dead thread).
- **Wedged (infinite loop) plugin** — nothing detects it until it misses a deadline; ack 10s timeout
  → failure transition; join 5s timeout → **thread detached, mapping stays**; record
  `unloaded-detached`, PS shows it, metric counted, ERROR logged once. One core burns forever.
- **Segfault/abort in plugin code (incl. `DllMain`/`.init`/constructor)** — the *process* dies. No
  tree, unit, or controller can help; the supervision tree supervises exits and cells, not memory.
  This is the accepted price of native plugins, stated in the ADR and docs honestly.
- **Plugin calls `libc::abort` / stack overflow** — same: process death. Documented limitation.
- **Native crash-restart** — never automatic in phase 2 recommended config (OQ3): no dlopen storms,
  no silent state loss on restage; operator runs `/candyriya reload plugins` or reboots.

## 5. What gets copied from the WASM implementation — and what must NOT

Copied (shape, not code — the WASM crate stays intact until phase 4):

- `instance.rs` shape → `candyriya-native-instance`: dedicated named OS thread
  (`candyriya-plugin-<id>`), bounded `sync_channel(16)` mailbox, caller waits with deadline,
  `Busy`/`Timeout`/`PluginGone` error vocabulary, `catch_unwind` in the worker, drop-closes-mailbox,
  join with timeout, **drop never blocks**, `worker_loop` → `cmd_loop`.
- `units.rs` shape → controller's spawn/`sync_native_plugin_units`: idempotent name-scan,
  spawn-under-parent, cancel-on-vanish, `spawn_blocking` for the staging work.
- `hot_reload.rs` shape → `Reload`: stage-then-swap failure policy, `watch` stays wasm-only; the
  native reload is an explicit operator verb (a native watcher is not planned for phase 2).
- `plugin_set.rs` shape → controller's ordered registry (IndexMap, first-record-wins,
  load order = dispatch/enable order).
- `health.rs` shape → native health cells: sticky per-id `watch<bool>`, `reason()` kept, reset on
  fresh build (a restage is an explicit clean slate).
- `plugin_loader.rs` shape → `discover()`: libraries-before-plugins order, alphabetical within root,
  `(where, why)` issues, first-in-load-order-wins on duplicate ids (`validate_unique` reused).
- `service_registry.rs` revocation machinery → bulk-revoke plugin-owned entries on swap/disable/
  unload (verify `Any` downcast; if it doesn't generalize, revocation lands phase 3).
- `dispatch.rs` payload-build-once pattern → no dispatch in phase 2 (native plugins get events in
  phase 3), noted so the phase-3 author knows the shape to reuse.

**Explicitly NOT copied — the named retirement list:**

1. The **instance-spawn path**: `PluginLoader::instantiate` (wasm compile), `PluginInstance::new`
   (extism `PluginBuilder`, `with_memory_max`, fuel/epoch), `limits.rs` (`check_module` parse caps,
   `InstanceSlots`, `CallRateLimiter`, `LogBudget`, `HandleTable` — the WASM-only sandbox accounting).
   Native replaces it with dlopen + FFI wrappers; none of the fuel/memory-page semantics exist
   natively and none are ported.
2. The **extism/wasmtime dependency chain** (`extism`, `wasmparser`, `wat` dev-deps, `wasmtime` and
   its internal closure) — untouched this phase, deleted in phase 4 (issue constraint: both flavors
   coexist until then).
3. The **WASM-specific test fixtures** — `tests/common/mod.rs` WAT builders
   (`static_export_plugin_wasm`, `echo_host_fn_wasm`, …), `tests/limits.rs` module-cap fixtures,
   `tests/dispatch.rs`/`tests/host_functions.rs` WAT-driven cases. Replaced by: **the real greeting
   cdylib** (built by CI and by `cargo test -p greeting`) driving the native lifecycle IT; a *panic
   fixture* and a *hang fixture* cdylib added in phase 2 for the failure paths. The WAT harness is
   retired only in phase 4 with the rest of the WASM path.
4. `PluginState`'s **3-variant enum stays exactly as-is** (`Loaded | Failed | Disabled`) for WASM.
   Do NOT widen it to 11 — the controller owns `PluginRecord`; unifying is a phase-4 mistake, not a
   phase-2 convenience.
5. **`PluginHealthSet`** — reused as-is for native cells (same `PluginHealth` type; the cell does not
   care what flavor raised it). Only the *raisers* differ.
6. **The "can_power flag on the .plugin file"** — **NOT FOUND anywhere in this branch, its history,
   or the wider tree.** No `*.plugin` descriptor exists, no `can_power` key in any manifest or test
   fixture, no such identifier anywhere in the tree. The phase-2 plan therefore has no such input.
   Most plausible intended meanings, in descending order: (i) the wasm-era `InstanceSlots` /
   `max-instances` budget knob ("can this instance be powered on?") — in native-land there is no
   live-instance budget in phase 2 (one instance per plugin by construction; the staged-reload pair is
   the only double-mapping, bounded to 1); (ii) the `plugin_main` manifest field, whose meaning
   *does* change: today it names a wasm export checked via `instance.function_exists`; in native it
   is vestigial — the entry points are fixed symbol names, and the field must be ignored-with-warning
   or repurposed, decided in the freeze review. **This needs lead clarification before
   implementation; until then it is treated as (ii) and logged, not designed against.** See §10.6.
7. **`plugin.toml` `plugin_main`** semantics change per above — never edit the manifest schema in
   phase 2; parser stays untouched.
8. `PluginReloader` proper — not reused for native (flavor-specific staging); it stays the WASM
   path until phase 4.

## 6. Plugin threading model — native

Rules, identical in shape to the WASM plugin model, restated for native:

- **Every native plugin runs on its own dedicated, named OS thread** — `candyriya-plugin-<id>` —
  spawned inside `candyriya-native-instance`, never on a tokio or rayon worker, never a `TaskService`
  task. The tree unit body (tokio) only forwards and awaits; it does no plugin work itself.
- **Plugin code never runs anywhere else.** dlopen/constructor risk is accepted and documented (§4.5);
  `catch_unwind` guards every FFI call proxy-side.
- **Bounded mailbox**: `sync_channel::<NativeCommand>(16)`; full ⇒ caller gets `PluginError::Busy`
  (the design's "plugin busy" contract), caller decides drop vs wait. Lifecycle calls carry a
  oneshot `respond` with a 10s deadline; `Health` is fire-and-check.
- **Restage**: a restage = a *new* OS thread for the new instance; the old instance's mailbox is
  closed after its final state transition; the old thread exits on mailbox close (or is joined with
  5s then detached at unload). Threads are never interrupted mid-call; a restaged plugin's old
  thread may linger one call duration past the swap, by design.
- **Lifecycle signals** (all arrive via the mailbox): `Load`, `Init`, `Enable`, `Disable`, `Unload`,
  `Health` — the `PluginMsg` verbs from §3 commit 2. Shutdown cascade (`/candyriya end`): cancel → plugin units
  cancel → mailbox closes → bounded joins drain in reverse-spawn order before finalizers.
- **Thread naming**: `candyriya-plugin-<id>` exactly like WASM units (id namespace shared across
  flavors by registry construction; uniqueness checked in one place — `validate_unique`).
- **Metrics discipline**: thread time recorded at the FFI wrapper; no per-id labels on IPC series;
  per-plugin series stay bounded by plugin count.

## 7. Performance characteristics — where the boundary is; BORROW vs CLONED

**The ABI boundary is exactly six call sites** (V1 + the optional probed disable/unload, §10.1):
`dlopen`/`LoadLibraryW` + symbol resolution (once per load), `candyriya_plugin_init(args)`,
`candyriya_plugin_on_load(api)`, `candyriya_plugin_on_enable(api)`, and (if present)
`candyriya_plugin_on_disable(api)` / `candyriya_plugin_on_unload(api)`. **Inside the proxy there is
no FFI anywhere else.** Events/config/cache/etc. do not cross in phase 2 at all — native plugins get
no host functions until phase 3.

| Crossing | Direction | BORROW | CLONED (and why) |
|---|---|---|---|
| `PluginInitArgs{api: *const CandyriyaPluginApiV1}` | proxy→plugin | The table is proxy memory, borrowed for the call; plugin must not retain it | — |
| `PluginInitOutput{status, abi_version, metadata}` | plugin→proxy | — | **By value, then copied**: output is `Copy` by construction; the proxy immediately copies `id/version/name/description` out of the borrowed `CandyriyaBuf`s (bounded: id ≤ 64B, version/name ≤ 256B, description ≤ 1KiB) into its own record; buffers never escape the call |
| `CandyriyaBuf` payloads (future host fns) | both | Borrowed slices valid for the call duration only (`as_slice` under the Safety contract; null+0 is canonical empty) | Anything surviving the call is cloned by the consumer; nothing crosses with ownership |
| Lifecycle acks (`PluginResp::*`) | unit→controller | — | Ack metadata (`generation`, `status`, `duration_ms`) is small and copied by value; `AckError.message ≤ 1 KiB` cloned |
| Plugin-owned strings in future `PluginMetadata` reads | plugin→proxy | — | Always copied at the boundary; proxy never retains plugin pointers past the call |
| Dispatch payload (phase 3 preview) | proxy→plugin × N | **Built once per dispatch** (event envelope bytes), borrowed by every receiving plugin call — never cloned per plugin | — |

Rules of thumb, binding for phase 2:

- **BORROW by default** for anything produced and consumed inside one call (which is everything in
  phase 2 — lifecycle carries zero bytes of payload).
- **CLONE only at ownership boundaries** (plugin→proxy identity at init; error text at failure;
  reload receipts). All bounded.
- No `Vec<u8>` copies on the healthy path; `candyriya_plugin_lifecycle_total` is the only new hot
  counter and it's cheap.
- Cost model: dlopen ≈ O(ms), once per load/stage; lifecycle call ≈ O(1) FFI + enum return; health
  sweep ≈ O(#plugins) IPC pings per second (not on the dispatch path); reload stage = 2 dlopens +
  2 brief mappings (≤1 pair per id, §4.2); unload = join(≤5s) + `dlclose` (never while any FFI frame
  could be live — the counter + join are the proof).
- A wedged plugin costs one core forever (§4.5) — performance tooling for that arrives phase 3
  (`candyriya_plugin_stuck_total` exists from commit 2).

## 8. Explicitly NOT in phase 2 (do not pull in)

1. **Package manager / install pipeline** — entirely out (future epic, apt-style registry; phase 4
   territory at the earliest). Nothing about distribution, signing, per-target matrix, or registry
   belongs in this phase.
2. **Replacement of `candyriya-plugin-api` with the native ABI** — a phase 3 deal, full stop. The
   WASM-visible contract stays; native plugins in phase 2 use only `candyriya-native-abi` +
   (optionally) the SDK.
3. **Docs for end-users of native plugins** — only implementation-level docs this phase (the ADR,
   the tree diagram, the one-liner in `docs/plugins.md`). The plugin author manual is phase 3/4.
4. **Host functions / proxy→plugin and plugin→proxy APIs** (config, cache, temp, libraries, events,
   timers, metrics, i18n, IPC-register) — phase 3. The V1 table stays at zero fns. A phase-2 `log()`
   slot is explicitly rejected unless the OQ8 freeze is reopened and lands it (§10.2).
5. **Scoped `TaskApi` for plugins** (max concurrent 4, max queued 16, 5s cancel, cooperative — the
   plugin task quotas) — phase 3. Phase-2 plugins get no background
   work besides their own OS thread.
6. **Event dispatch into native plugins** — phase 3. Phase-2 native plugins receive nothing but
   lifecycle calls.
7. **`/candyriya plugin` CLI** — *issue letter says phase 2*, flagged as OQ7 because it adds ~3
   agents and i18n obligations; if the lead trims it, it moves to phase 3 unchanged.
8. **Plugin-owned services for native plugins** — wire the revocation hook, do not expose
   registration to native code; phase 3.
9. **Stage-then-swap quiesce polish / zero-downtime swimlanes** — bounded outage or the double-pair
   budget is accepted as v1; per-plugin dispatch-pause refinements phase 3.
10. **Permissions model for native** — plugin permission design is phase 3; the manifest's
    `[permissions]` list stays structurally validated and unenforced this phase.
11. **WASM removal** (extism/wasmtime/plugin-api/wasm tests/CI jobs/docs) — phase 4, per the issue.
    Nothing in phase 2 edits wasm code paths, `PluginState`, or `limits.rs`.
12. **i18n for native plugins, plugin locales, `candyriya:i18nT` native equivalent** — phase 3.
13. **CI `windows-latest` for the whole workspace** — phase 2 adds only the dynlib smoke job (OQ17);
    a full 3-OS matrix is a separate decision.
14. **Metrics/alerting polish** (`candyriya_plugin_stuck_total` wiring, ps `--watch` lifecycle
    events) — phase 3 observability.

## 9. The first phase-2 agent to launch (cold-start brief)

**A36 — `feat(controller): PluginRecord + 11-state machine`, crate `crates/plugin/candyriya-plugin-controller`.**
This unblocks everything else, needs zero unsafe, zero dlopen, zero tree changes, and can start the
moment this plan is approved. Brief to hand it (verbatim):

> Context: phase 1 of the native-plugins epic (#310) landed `candyriya-native-abi` + `crates/plugins/greeting`
> at `experiment/native-plugins @ 640cda5`. Phase 2 needs a `candyriya-plugin-controller` crate. Your slice is
> the pure core only — no `candyriya-init`, no dlopen, no threads, no unsafe, no CLI, no tests outside this crate.
>
> Deliver: new workspace crate `crates/plugin/candyriya-plugin-controller` with:
> - `PluginWireState`: exactly 11 variants (`Discovered, Loading, Loaded, Initializing, Initialized,
>   Enabling, Running, Disabling, Disabled, Unloading, Unloaded`) — NO `Failed` variant; failure is
>   `reason: Option<String>` + the unloading/unloaded path; and a `disable_failed: bool` latch field.
> - `PluginRecord { id, origin: Library|Plugin, flavor: Native|Wasm, manifest: PluginManifest,
>   issues: Vec<String>, state, generation: u64, reason, disable_failed, created_at, state_entered_at }`.
> - `machine::apply(record, event) -> Transition` — the transition table per the phase-2 design pass:
>   DISCOVERED→LOADING requires deps-gate bookkeeping inputs (dep readiness passed in, do not resolve
>   deps — reuse `resolve_plugins` in the caller, not here); every transition returns the new state,
>   the events produced (e.g. `SpawnUnit`, `SendMsg(Verb)`, `Reap`, `DropContext`), and the deadline to
>   stamp on entry (load/init/enable/disable/unload = 10s as `std::time::Duration` constants
>   `LOAD_TIMEOUT` etc., not configurable). Deadlines are data on the transition, not sleeps.
> - `apply` must be total and deterministic: unknown (state, event) pairs return a `Transition::Reject`
>   with a reason, never panic. Stale-generation events are rejected by the caller, not here (generation
>   is carried through untouched).
> - `record::PluginRegistry`: `IndexMap<String, PluginRecord>` ordered as discovered (libraries first,
>   then plugins, alphabetical), unique ids (reuse `validate_unique` semantics — import the grammar
>   `[a-z0-9_.-]{1,64}` from `candyriya-plugin::validate_unique`, do not reimplement).
> - Wire types in `msg.rs`: `PluginMsg { Load{generation}, Init{generation}, Enable{generation},
>   Disable{generation}, Unload{generation}, Reload{generation}, Health }`, `PluginResp { Load{generation,
>   result: Result<LoadReceipt, AckError>}, …, Health(HealthReport) }`, `AckError{code: u32, message:
>   String (≤ 1 KiB, no backtraces)}`, `CtrlMsg { List, Get{id}, Enable{id}, Disable{id}, Reload{id},
>   Unload{id} }`. Internal types only, no serde, no wire serialization — type-erased like
>   `candyriya-plugin/src/host_wire.rs`. Metrics constants go in a separate follow-up commit, not here.
> - Tests in `tests/machine.rs`: one test per transition (happy + failure path), dep-gate rejection,
>   disable_failed latch behavior on re-disable, rejection of impossible transitions, `Unloaded` being
>   terminal, registry order stability across inserts, exactly the 11 states (a `const ALL: [PluginWireState;
>   11]` pinned by a test).
>
> Touch nothing else. Do not widen `candyriya-plugin`'s `PluginState`. Do not add dependencies beyond
> workspace-pinned `indexmap`, `parking_lot`, `thiserror`, `candyriya-plugin` (for `PluginManifest`).
> `#![forbid(unsafe_code)]`.
>
> Mirror the test style of `crates/plugin/candyriya-plugin/src/dep_resolution.rs` (inline unit tests,
> tiny fixtures built inline, zero I/O) and `tests/loader.rs` (issue-assertion shape) — NOT the WAT
> fixtures in `tests/common/mod.rs`. Reuse the exact 11-state names and `PluginMsg`/`PluginResp`
> shapes from the phase-2 design pass.

Launch A36 first; A37 (discovery) can start in parallel — they share no files.

## 10. Unresolved questions that MUST land answers before implementation starts

Answering = lead decision recorded on #310 (or this doc). Ordered by blast radius.

### 10.1 BLOCKER — DISABLING/UNLOADING have nothing to call (the V1 ABI gap)
The merged phase-1 ABI exports **exactly three** symbols: `candyriya_plugin_init`,
`candyriya_plugin_on_load`, `candyriya_plugin_on_enable`. The 11-state machine's DISABLING and
UNLOADING states (and the lifecycle contract in #310) require `on_disable`/`on_unload` callbacks. The design pass
assumed they exist; they do not, and V1 is frozen — additive exports may not ride on V1.
Options: **(a)** phase 2 probes *optional, probed, unversioned* exports
`candyriya_plugin_on_disable(api) -> CandyriyaStatus` and `candyriya_plugin_on_unload(api) ->
CandyriyaStatus` at Load time (libloading `Symbol::get`; absent ⇒ no-op + INFO log + record note; the
greeting crate gains both exports in the first phase-2 commit; formal adoption in V2/phase 3) — keeps
V1 frozen, unblocks the full machine, tiny ABI delta. **(b)** phase 2 ships the machine with plugin
code running only through enable; disable/unload are drain→join→dlclose only — honest but the epic's
"disable acknowledgement" never happens; the DISABLED state becomes a lie until phase 3. (c) Pull a V2
forward into phase 2 — rejected: scope collapse into phase 3, defeats staging.
**Recommended: (a).** If you had to unblock this in 5 minutes: edit `crates/plugins/greeting/src/lib.rs`
in this branch adding the two `#[unsafe(no_mangle)] pub unsafe extern "C" fn candyriya_plugin_on_disable` / `candyriya_plugin_on_unload` exports (same body shape as `on_load`: re-check the table, return `CandyriyaStatus::Ok`), record in #310 that V1 stays frozen and these are *optional probed* symbols to be ratified in the OQ8 freeze review, and hand A38 the §10.1(a) rule (probe at Load, absent ⇒ no-op, never error).

### 10.2 BLOCKER — ABI freeze review sign-off (OQ8)
The phase-1 artifact (merged at 640cda5) must be re-read and frozen *before* `candyriya-native-instance`
lands: string ownership (borrowed buffers, copy-in/copy-out), `size` tripwire, fail-closed handshake,
zero host fns, entry-point names. Same review must ratify or reject a phase-2 `log()` slot (rec:
reject — phase 2 ships lifecycle-only; log arrives in phase 3 with V2). Also required: explicit lead
sign-off on crate-local `#![allow(unsafe_code)]` for `candyriya-native-instance` **only** (the
workspace forbid stays; the controller never gets unsafe).

### 10.3 BLOCKER — reload semantics (OQ6)
Stage-then-swap (rec; two contexts coexist; double-instance budget = 1 staged pair per id; swap under
quiescence; stage failure keeps old RUNNING) vs plain macro (`Disable→Unload→Load→Init→Enable`; bounded
outage; enable-of-new failure leaves plugin ABSENT). One-line decision, kills/repairs a whole branch of
the machine and the reload IT.

### 10.4 BLOCKER — native crash-trap policy (OQ3)
`RestartPolicy::Never` + operator retry (rec) vs `OnFailure` + full `Reload` restage by the unit
(self-healing, dlopen-storm-prone, silent state loss). One-line decision.

### 10.5 BLOCKER(ish, scope) — `/candyriya plugin` CLI in phase 2 or not (OQ7)
The issue says phase 2; the agent budget does not comfortably fit it (~3 extra agents, i18n en/ex_girl
obligations, brigadier, reload wiring). If yes: included as commits 8; if no: moves to phase 3, and
phase 2 ships operator-only control (`ps` visibility + reload via existing `/candyriya reload`).

### 10.6 BLOCKER(ish, fact) — the `can_power` reference (§5.6)
Unverifiable; best-effort mapped to §5.6(ii) `plugin_main` meaning-change pending clarification; the
plan does not depend on it until the lead names what it is. *(This is the only unresolved item with no
recommended default.)*

### 10.7 Answers needed early, but implementation-flexible (not blockers)
- OQ1 keep exactly 11 states, failure via reason+terminal (rec).
- OQ19 dep-enable gate: deps must be RUNNING (rec) not merely INITIALIZED.
- OQ2 never auto-unload after `disable_failed` (rec); force-flag for wedged plugin: yes, logged+metric'd, or strictly manual?
- OQ12 after failed `on_load`/`on_enable`, do not call `on_unload` (rec).
- OQ4 controller failure policy: `Never` (rec). OQ5 sweep: `TaskService::every(1s)` (rec), no kernel composite.
- OQ9 separate `candyriya-native-instance` crate (rec) vs module inside `candyriya-plugin`.
- OQ10 constants not TOML knobs (rec). OQ11 stage-copy under `temp/` accepted universally (rec).
- OQ12/13 shared `plugin-<id>` names + single uniqueness check (rec). OQ14 docs one-liner in this phase (rec).
- OQ16 metrics names into `names.rs` (rec). OQ17 windows-latest smoke job yes (rec).
- OQ18 exact `CtrlMsg` verb set (List/Get/Enable/Disable/Reload/Unload) — finalize with OQ7.

**Answered already (for the record):** the dlopen mechanism is **libloading** — latest is **0.9.1**
(edition 2021, ISC license — allowed in deny.toml — MSRV 1.88, updated on crates.io 2026-11-13,
~164M recent downloads). It wraps `dlopen`/`LoadLibraryW` natively; the workspace stays edition-2024 on
a stable toolchain ≥ 1.88. Alternatives rejected: direct `windows-sys`/`libc` calls (two hand-rolled
unsafe surfaces for zero gain), and `dlopen` via a build script (no). `multiple-versions="deny"` is
satisfied: nothing else in the lock pulls libloading.

## 11. Deviations from the earlier design pass — what lives here vs #310

Fossilize in this doc (implementation detail, PR-review scope): §4 table deadlines as constants; the
"no literal RUNNABLE" clarification; commit granularity; A36's isolation (no init dep for the machine
core); narrowing the sketch's "phase-2 log() slot maybe" to "rejected unless OQ8 reopens"; native
reload delivered as stage-then-swap pending OQ6; §5's explicit retirement list.

Back to #310 (contract/ABI level — lead to comment/edit the issue; A35 does not edit it):
- §10.1 — the V1 export gap and the "optional probed on_disable/on_unload" ratification (or
  alternative (b)) — **this belongs on #310**, because it changes what phase 2 can claim complete.
- §10.6 — the `can_power` clarification, once the lead identifies the artifact.
- §10.2 — the freeze-review verdict on V1 (and whether phase-2 smoke job OQ17 is accepted).
- OQ1/OQ2/OQ3/OQ6/OQ7/OQ19 — the decision results, so the issue and this plan stay in sync.

One deviation worth restating: the design pass assumed `on_disable`/`on_unload` existed in V1
because it wrote the machine before reading the merged crate; the plan treats V1 as merged-reality
and routes around it (§10.1) rather than silently amending phase 1. Phase 1 is not reopened in
phase 2 either way — if §10.1 lands on (a), phase 1 ships a README addendum in a phase-2 prep commit,
no ABI mutation.
