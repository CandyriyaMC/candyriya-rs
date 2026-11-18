# candyriya-native-abi

The `extern "C"` contract shared by the proxy and native (non-WASM) candyriya
plugins: versioned `#[repr(C)]` structs, one opaque instance handle, byte
buffers, and the versioned `CandyriyaPluginApiV1` function table.

This crate is **types only**. No runtime, no loader, no dlopen, no host
functions. The loader and the supervision wiring land in phase 2 of the epic;
host functions land with it or in phase 3. WASM plugins are untouched by
this crate and keep running in parallel.

If you are here to write a native plugin, skip to
[Writing a new plugin](#writing-a-new-plugin).

## The flow

```text
proxy                                          plugin (cdylib/so/dylib)
─────────────────────────────────────────────  ─────────────────────────
dlopen(plugin file)
resolve `candyriya_plugin_init`
────────────────────────────> candyriya_plugin_init(args)
                               args = { abi_version: 1, api: *const table }
                               checks the handshake, answers
<──────────────────────────── PluginInitOutput { status, abi_version, metadata }
status != Ok? stop — plugin refused
────────────────────────────> candyriya_plugin_on_load(api)  -> CandyriyaStatus
────────────────────────────> candyriya_plugin_on_enable(api) -> CandyriyaStatus
(the demo prints one line here)
```

Failure is always "fail closed": a plugin that sees anything it does not
recognise — a wrong ABI echo, a mis-sized table, a null pointer — returns
`CandyriyaStatus::Failed`, and the proxy does not call it again.

## The pieces

| Item | What it is |
|---|---|
| `CANDYRIYA_NATIVE_ABI_VERSION` | `u32`, currently `1`. The proxy, the args and the output all carry it. |
| `CandyriyaBuf` | `(ptr: *const u8, len: u32)` — a borrowed byte buffer. No ownership, no allocator. |
| `CandyriyaStatus` | `Ok = 0`, `Failed = 1`. Unknown values are failures by rule. |
| `PluginMetadata` | `id`, `version`, `name`, `description` as `CandyriyaBuf`s. Empty `description` means absent. |
| `CandyriyaPluginHandle` | `repr(transparent)` over `*mut c_void` — an opaque, proxy-owned instance token. Never dereferenced, never forged, never freed by the plugin. |
| `CandyriyaPluginApiV1` | the table the proxy hands out with every entry point: `abi_version`, `size`, `instance`. Deliberately tiny — two `u32`s and a pointer. |
| `PluginInitArgs` / `PluginInitOutput` | the init handshake: proxy → plugin and plugin → proxy. |
| `CANDYRIYA_PLUGIN_INIT_EXPORT`, `CANDYRIYA_PLUGIN_ON_LOAD_EXPORT`, `CANDYRIYA_PLUGIN_ON_ENABLE_EXPORT` | the three symbol names, as constants. |

The three exports a plugin must provide:

```text
unsafe extern "C" fn candyriya_plugin_init(args: *const PluginInitArgs) -> PluginInitOutput;
unsafe extern "C" fn candyriya_plugin_on_load(api: *const CandyriyaPluginApiV1) -> CandyriyaStatus;
unsafe extern "C" fn candyriya_plugin_on_enable(api: *const CandyriyaPluginApiV1) -> CandyriyaStatus;
```

`CandyriyaPluginApiV1` ships **zero host functions on purpose**. Proxy→plugin
calls travel through the plugin's own exports; every plugin→proxy call
(configuration, cache, events, …) is defined by a later ABI version. The
table, the handle and the versioning rules exist now so they are real, not
welded on later.

### Buffer rules (all of them)

- A `CandyriyaBuf` points at memory owned by the side that produced it.
- It is valid for the duration of the call it is passed in; the consumer
  copies what it wants to keep and never retains the pointer.
- `ptr: null, len: 0` is the canonical empty buffer. `null` with a non-zero
  `len` is a contract violation — readers yield an empty slice instead of
  dereferencing null.
- `len` is a `u32`; a bigger buffer is not representable in V1.
- Metadata buffers in practice point at `&'static` bytes inside the plugin.

## Writing a new plugin

1. Create a crate with `crate-type = ["cdylib", "rlib"]` and depend on this
   crate. `rlib` is required so your tests can call the exports in-process —
   no loader needed to check yourself.
2. Export the three symbols above. Every export starts by re-checking the
   handshake (`PluginInitArgs::is_valid`, then `CandyriyaPluginApiV1::is_valid`)
   and returns `Failed` on anything unexpected.
3. Fill `PluginInitOutput::ok` with metadata built from static byte strings:
   `id`, `version`, `name` non-empty; `description` optional.
4. Print or log whatever you like to see — right now `println!` is all you
   have, since there are no host functions yet.
5. On `on_enable`, do the actual work. On `on_load`, register nothing (there
   is nothing to register yet — that arrives with V2).
6. Build and run the tests of
   [`crates/plugins/greeting`](../../plugins/greeting) — it is the reference
   implementation of exactly these steps; copy its shape, not its size.

The result is `greeting.so` on Linux, `greeting.dll` on Windows,
`greeting.dylib` on macOS (with the `lib` prefix on Unix-likes). The crate
is platform-agnostic by construction: no `cfg`s, no platform crates, no
linker arguments. The five CI release targets all apply unchanged.

A native plugin runs **in-process**. It is not sandboxed, and the ABI does
not give it a privilege model. Treat a native plugin with the same trust you
give the proxy's own crates. (WASM plugins remain the sandboxed option until
phase 4 retires them.)

## Versioning rules

1. **V1 is frozen.** No field of any V1 type is added, removed, reordered or
   re-typed, ever. No existing behaviour changes meaning. `abi_version` of
   V1 is fixed at `1`.
2. **Every table starts with `abi_version` at offset 0**, always `u32`,
   always the ABI level. Any future table (V2, V3, …) keeps this layout, so
   the proxy can always tell versions apart by the first four bytes.
3. **The version echo travels with both sides** — the proxy writes it in
   `PluginInitArgs::abi_version`, the plugin echoes it in
   `PluginInitOutput::abi_version`. A mismatch is a refusal, not a
   negotiation.
4. **`CandyriyaPluginApiV1::size` is a tripwire, not a negotiation** — it is
   `size_of::<CandyriyaPluginApiV1>()`; both sides were compiled against the
   same type, so it can only differ when something is fundamentally broken.
   A plugin seeing a mismatch refuses.
5. **Unknown `CandyriyaStatus` values mean `Failed`.** The enum is
   `#[non_exhaustive]` precisely so every consumer carries a wildcard arm.
6. **Empty `description` means absent**; `PluginMetadata::EMPTY` is the only
   valid payload of a failed init; `is_complete()` is the "did this plugin
   report an identity" floor (the full manifest rules live proxy-side).
7. **Additive changes never ride on V1.** No silent extension of the table,
   no new fields squeezed in, not even "optional" ones.
8. **Breaking anything is a new ABI version.** See the next section.

## What changes between V1 and future iterations

Planned, and only this:

- **V2 introduces the first host functions** — through a brand-new
  `CandyriyaPluginApiV2` table type, never by mutating V1. For a deprecation
  window the proxy speaks both: the ABI is selected by the first `u32` at
  offset 0, and the table pointer in `PluginInitArgs` refers to whichever
  the handshake established. V1 plugins keep working against a V2 proxy.
- **V2 may extend the exports** — e.g. a new `candyriya_plugin_on_disable`
  export or a different shutdown contract. If an export's signature changes
  in any way, that is a new ABI version by definition.
- **Host functions arrive as `extern "C"` fn pointers in the V2 table,**
  appended to the V1 fields (`abi_version`, `size`, `instance`) — the V1
  prefix is byte-compatible, so reading the first 16 bytes stays valid for
  both.
- **`CandyriyaStatus` grows variants only alongside an ABI version,** and
  only variants that old consumers safely treat as failure.
- **Nothing in V1 is deprecated, removed or renamed** until every supported
  proxy and every supported plugin is past the transition window; the order
  of those events is decided by the release, not by this document.
- **What never changes:** the `CandyriyaBuf` shape, `repr(C)` everywhere,
  `abi_version` at offset 0, the fail-closed rule, the no-ownership buffer
  rule, the empty-description-means-absent rule.

This crate guarantees stability across **minor** proxy releases. A proxy that
speaks V2 still refuses V0/V-forgotten builds cleanly — there is no silent
partial compatibility, ever.

## Layout cheat sheet (64-bit)

| Type | Size | Notes |
|---|---|---|
| `CandyriyaBuf` | 16 bytes | `*const u8` + `u32` + padding |
| `CandyriyaStatus` | 4 bytes | `#[repr(u32)]` |
| `CandyriyaPluginHandle` | 8 bytes | transparent over `*mut c_void` |
| `PluginMetadata` | 64 bytes | four buffers |
| `CandyriyaPluginApiV1` | 16 bytes | two `u32`s + a pointer |
| `PluginInitArgs` | 16 bytes | `u32` + `*const` table + padding |
| `PluginInitOutput` | 72 bytes | status + echo + metadata |

(The sizes are `size_of` output, pinned by the contract tests; 32-bit
targets shrink the pointer fields, and the tests are width-aware rather
than absolute.)

## What is deliberately not here

- `dlopen`/`libloading` and the plugin state machine — phase 2 (the Plugin
  Controller; `candyriya-init`'s supervision tree owns lifecycle).
- Host functions of any kind — phase 2/3; V1's table is empty by design.
- `plugin.toml` parsing and dependency resolution — the proxy's loader, not
  the ABI, validates identities.
- Threads, mailboxes, fuel, timeouts — the host plugin model, unchanged
  from the WASM runtime.
- Any claim of sandboxing — native plugins are in-process code.

## Development

```shell
cargo test -p candyriya-native-abi
cargo build --release -p greeting     # .so / .dll / .dylib per platform
```

Error codes in this crate never carry text; diagnostics live in the proxy's
logs, and the ABI stays `u32`-sized for a reason.
