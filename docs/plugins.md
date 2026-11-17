---
layout: default
title: Plugins
---

# Plugins

Candyriya runs plugins as [Extism](https://extism.org) WebAssembly modules.
This page is the guide for plugin authors: where plugins live, what
`plugin.toml` declares, what the host expects from your module, and what
works today versus what is planned. For a description of the plugin runtime
inside the proxy, see [Architecture](architecture); for the configuration
sections referenced here, see [Configuration](configuration).

This page is honest about status. Everything on it was checked against the
code in this repository. Where something is not implemented or not enforced
yet, it says so explicitly. There is no hidden or "secret" plugin API.

## Where things stand

The short version, before the details:

- **Implemented and merged:** WASM loading, sandbox limits, dependency
  resolution, the typed event model, the event dispatcher, hot-reload
  primitives, the service registry, and `/candyriya reload`.
- **Not wired into the proxy yet:** events are not dispatched into plugins
  during a real connection; plugins are loaded at startup and on demand via
  `/candyriya reload` (no filesystem watcher is armed in the CLI today);
  no host functions are registered.
- **Not enforced:** the `permissions` list in `plugin.toml` is validated
  structurally but never checked against any action.

Treat the rest of this page as the contract your plugin is written against
and as the current state of the runtime, accurately separated.

## System overview

A Candyriya plugin is a `.wasm` module with an Extism-compatible ABI,
loaded by [extism](https://extism.org) on top of wasmtime. The Extism
manifest the host builds carries no WASI: the plugin gets no filesystem,
no sockets, no environment, and no clock beyond what the host may one day
hand it explicitly. That choice is deliberate:

- **Sandbox.** A plugin is linear-memory-sandboxed WebAssembly. It cannot
  read or write the proxy's memory, open files, sockets or network
  connections unless the host exposes them, and there is no way for it to
  escape into the process. There is no "native plugin mode", and never will
  be: plugins are never loaded as native `.so`/`.dll`, and the host does
  not care what language produced the wasm.
- **One dedicated OS thread per plugin** (`candyriya-plugin-<id>`). A plugin
  never runs on a Tokio worker thread. All calls into the plugin happen on
  that thread, served from a bounded mailbox.
- **Bounded mailbox.** The mailbox holds 16 pending commands
  (`sync_channel(16)`). A caller that finds the mailbox full gets
  `PluginError::Busy` immediately rather than blocking; the full-or-empty
  answer is decided with `try_send`, never with an unbounded wait.
- **Timeouts everywhere.** Each call carries a deadline. The caller waits at
  most the configured timeout; the per-event dispatch uses a 10 s per-plugin
  timeout by default. A plugin that does not answer in time keeps whatever
  state it has (the call is not cancelled, just no longer awaited).
- **Kill-on-stuck.** A plugin spinning in an infinite loop is interrupted
  either by its fuel budget (when configured) or by the manifest timeout
  enforced through the wasmtime epoch deadline. The call surfaces as
  `PluginError::FuelExhausted` or `PluginError::Timeout`; no other plugin
  and no caller is ever blocked by it. A host-side panic while a plugin
  runs is caught with `catch_unwind` and reported as a normal error.

**Why WebAssembly, for the reader.** Any language that can produce an
Extism-compatible wasm module can be a plugin: Rust (via `extism-pdk`), Go,
AssemblyScript, C#, C, Zig and others. The host does not know or care what
language you used. The price is that the boundary carries bytes, not Rust
types: structured data crosses as JSON, and host capabilities arrive as
host functions (which do not exist yet - see
[Status](#where-things-stand)).

For readers coming from other proxies: there is no plugin classloader, no
dependency of the host on your module's internals, and no way for a broken
plugin to take the proxy down. A broken plugin is a record with
`Failed(reason)` that never runs wasm.

## Where plugins live

Plugins are discovered from two directories under the proxy's data
directory (the directory the proxy was started in; the current working
directory, until a `--data-dir` flag exists):

```text
<data_dir>/
  plugins/            # normal plugins
  libraries/          # library plugins: API other plugins depend on
```

Both roots use the same layout:

```text
plugins/
  my-plugin/
    plugin.toml     # required
    my-plugin.wasm  # or lib.wasm
libraries/
  some-library/
    plugin.toml
    some-library.wasm
```

Rules enforced by the loader:

- Each plugin lives in its own subdirectory named exactly after its id.
  The `id` in `plugin.toml` must match the directory name.
- The wasm module is `<id>.wasm`, falling back to `lib.wasm`. No other
  names are found.
- Hidden directories (`.foo`) are skipped silently. A `.wasm` file sitting
  directly in `plugins/` or `libraries/` (not in a subdirectory) is
  reported as a problem; plugins live in `<id>/` subdirectories.
- A directory with a `.wasm` but no `plugin.toml` is reported. A directory
  with neither is ignored.
- Ids must be unique across both roots. A collision is not a precedence
  rule: the library wins, the colliding plugin is dropped, and the drop is
  reported.
- Load order is **libraries first, then plugins**, alphabetical within
  each root (comparison is by file name). The dependency resolver then
  re-orders the set by declared dependencies (see
  [depend / softdepend](#depend--softdepend)); alphabetical-within-origin
  is the stable tie-break, not the final order when dependencies exist.

Every discovered plugin is also assigned a data directory the plugin owns
alone; the loader creates it (and the plugin is recorded `Failed` if it
cannot be created) before wasm is ever touched:

```text
<data_dir>/config/<plugin-id>/
```

A plugin's cache namespace lives at `cache/<plugin-id>/` (and
`temp/<plugin-id>/`), managed by the cache layer, under the quotas a
plugin may declare in `plugin.toml`.

## `plugin.toml` reference

Every plugin ships a `plugin.toml` next to its wasm module. The loader
reads it, validates it, and only then instantiates anything. A TOML
syntax error is reported as one message; every *validation* problem found
is collected into a single `MetadataError::Validation` message - fix the
whole manifest in one pass, not an edit-reload-fix loop.

Unknown keys are ignored on purpose: this file format may grow new fields
over time, and forward compatibility beats strictness for a manifest the
host and plugin authors do not upgrade in lockstep. Anything listed below
is a real, checked field today.

### `[plugin]` — actually the top level

`plugin.toml` has no `[plugin]` table. All manifest fields except
`[api]` live at the top level. An example of everything:

```toml
# identity
id = "my-plugin"                    # required
version = "1.4.0"                   # required, semver
name = "My Plugin"                  # required, human-readable
description = "What my plugin does" # optional
authors = ["Alice <alice@example.com>"] # optional, string list
website = "https://example.com/my-plugin" # optional, must start with http:// or https://
license = "GPL-3.0-only"            # optional

# host contract
plugin_main = "on_event"            # required, must be a valid export name
plugin_api = "0.1"                  # required, major.minor

# dependencies
depend = ["some-library"]           # optional, hard dependencies
softdepend = ["optional-companion"] # optional, ordering-only

# capability whitelist (see below - not enforced yet)
permissions = ["players.read"]

# cache quotas (bytes / resources, enforced by the cache layer)
cache_quota_bytes = 16777216        # optional
cache_quota_resources = 1024        # optional

[api]
exports = [{ name = "hello_world" }, { name = "ping" }]
```

Field by field:

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | `[a-z0-9_.-]{1,64}`; no leading/trailing `.`; no `..` anywhere. Used as the directory name, the thread name (`candyriya-plugin-<id>`) and in logs. |
| `version` | string | yes | Semver, e.g. `"1.4.0"`. Parsed by the `semver` crate; a non-semver value is rejected. |
| `name` | string | yes | Non-empty after trimming. |
| `description` | string | no | Free text. |
| `authors` | string list | no | Each entry must be non-empty after trimming. |
| `website` | string | no | Must start with `http://` or `https://`, or it is rejected. |
| `license` | string | no | Free text, e.g. an SPDX identifier. Not interpreted. |
| `plugin_main` | string | yes | Must match `[A-Za-z_][A-Za-z0-9_.-]{0,63}` and must exist as an export of the compiled module. The host checks its existence at load time. **It is never called today.** |
| `plugin_api` | string | yes | `major.minor`, e.g. `"0.1"`. Not `"0.1.0"`. Only shape-checked; see `ProxyVersion` below. |
| `depend` | string list | no | Hard dependency ids; see [depend / softdepend](#depend--softdepend). |
| `softdepend` | string list | no | Ordering-only companions; see below. An id in both `depend` and `softdepend` is rejected. |
| `permissions` | string list | no | Capability whitelist as opaque strings. Must be non-empty and unique, no leading/trailing whitespace. **Not enforced.** |
| `cache_quota_bytes` | integer (u64) | no | Max bytes in this plugin's cache namespace. `None` = proxy default. |
| `cache_quota_resources` | integer (u64) | no | Max cache resources in this plugin's namespace. `None` = proxy default. |
| `[api].exports` | table array | no | Exports other plugins may call. See [exports](#apiexports). |

Two naming notes that trip people up:

- The API version field is `plugin_api`, not `api`. `api` is taken by the
  `[api]` table - TOML does not allow the same key as both a string and a
  table.
- Export names and `plugin_main` match `[A-Za-z_][A-Za-z0-9_.-]{0,63}`,
  at most 64 bytes total. Ids use a different, narrower grammar
  (lowercase only). Neither allows `:` - a name like `candyriya:hello_world`
  is rejected as an export name. Colon-namespaced names belong to messaging
  channels (`candyriya:main`), not to exports.

`plugin_api` compatibility is defined by `ProxyVersion` in
`candyriya-plugin-api`: a plugin built against `0.1` runs on a host speaking
`0.1` or newer minor within major `0`. The current loader validates only
the `major.minor` *shape* of `plugin_api`; it does not call
`ProxyVersion::is_api_compatible`. That check ships with the host-function
milestone.

Minimal manifest for a plugin that just reacts to events:

```toml
id = "hello-world"
version = "0.1.0"
name = "Hello World"
plugin_main = "on_event"
plugin_api = "0.1"
```

An extended one, for a plugin that depends on a library and is callable
by peers (see the status notes before relying on the last part):

```toml
id = "candyriya-perms-sync"
version = "2.0.0"
name = "Permissions Sync"
description = "Syncs candyriya permissions to an external store."
authors = ["Alice <alice@example.com>", "Bob"]
website = "https://github.com/example/candyriya-perms-sync"
license = "GPL-3.0-only"
plugin_main = "on_event"
plugin_api = "0.1"
depend = ["http-client"]
softdepend = ["audit-log"]
permissions = ["players.read", "storage.write"]
cache_quota_bytes = 33554432
cache_quota_resources = 2048

[api]
exports = [{ name = "sync_now" }]
```

### A note on `[api].exports` and the host-side types

The `[api]` section is small on purpose: the extism ABI between host and
plugin passes bytes, and extism fixes the export signature shape at the
wasm level (exports in this ABI are effectively `() -> i32`, with the input
and output buffers handed to the guest through extism's own
alloc/output-set functions). Describing richer types here would duplicate
the host-function signatures without adding safety, so `exports` carries
names only.

The types that *do* cross the ABI, on the host side, are plain Rust structs
from `candyriya-plugin-api`, all `Serialize + Deserialize`:

```text
ProxyVersion            { api_version: String, implementation: String }
PluginMetadata          the parsed manifest itself
ApiInfo                 { exports: Vec<ApiExport> }
ApiExport               { name: String }
EventEnvelope           { id: u64, <kind fields...>, occurred_at_unix_ms: u64 }
OutcomeReport           { cancelled: bool, cancel_reason: Option<String> }
EventCancellation       { uuid: String, event_id: u64, cancelled: bool }
```

Your module sees exactly one flat JSON document per call. On the plugin
side you mirror these shapes by hand (or generate them); there is no
published Rust SDK for these types yet - see
[Authoring in Rust](#authoring-in-rust).

The runtime entry points those shapes belong to:

```text
PluginInstance::call(&self, export: &str, input: &[u8]) -> Result<Vec<u8>, PluginError>
ProxyEventDispatcher::dispatch_cancellable(&self, event: &ProxyEvent) -> DispatchResult
```

## depend / softdepend

Both lists carry **any plugin or library id discovered in this load run** -
libraries and plugins are the same namespace for dependency purposes.

**Hard dependencies (`depend`)** must load before you, and must load,
period. If a hard dependency is missing - or was dropped earlier in the same
pass because its own hard dependency is missing - your plugin is dropped.
The drop is recursive: `c` gone, `b` dropped, `a` dropped. A dropped plugin
does not get a record in the resolved set at all - the drop is reported
through the load report's `issues` (`plugin `a` cannot load: missing hard
dependency `b``), and that is the only trace it leaves.

**Soft dependencies (`softdepend`)** likewise load first *when present*,
and are ignored when absent. A missing companion never blocks you; it means
the optional integration stays off. Which features stay off is your
plugin's business - the host reports the companion's presence to the proxy
(`SoftDependency { companion, present }`) for the features that know about
it.

Resolution algorithm (exact):

1. The loader discovers candidates in a stable order: libraries first,
   then plugins, alphabetical within each.
2. Duplicate ids across the roots are dropped with a report; first in
   discovery order wins.
3. Hard dependencies that are unsatisfiable mark the requesting plugin
   (and recursively its hard dependents) as dropped.
4. A Kahn topological sort orders the survivors. All edges - hard and
   present soft ones - constrain the order. Whenever several nodes are
   ready to load, the tie-break is `(origin, id)` where libraries rank
   before plugins: that is where the "libraries first, alphabetical
   within each" rule actually comes from.
5. If nodes remain that Kahn cannot order, there is a dependency cycle.
   Resolution fails as a whole with `PluginError::DependencyCycle` whose
   diagnosis lines name every cycle found, one per line:
   `` `a` depends on `b`; `b` depends on `a` ``. A self-cycle reports as
   `` `a` depends on `a` ``. Nodes blocked downstream of a cycle (but not
   on a cycle themselves) are named on a separate line:
   `blocked by the cycle(s) above: `downstream``. Cycles through soft
   dependencies are reported with `soft-depends` in place of `depends`.

In the keep-going view (`resolve_plugins`, what the composition root
uses), drops become issues in the load report and the survivors load. In
the strict view (`resolve_plugin_order`), any unsatisfiable hard
dependency fails the whole call with `PluginError::UnresolvedDependencies`,
which carries *every* failure plus the plugins that would load, in load
order - resolution never stops at the first missing package.

Worked example:

```toml
# plugins/importer/plugin.toml
id = "importer"
version = "1.0.0"
name = "Importer"
plugin_main = "on_event"
plugin_api = "0.1"
depend = ["http-client"]          # must exist and load first
softdepend = ["audit-log"]        # used if present
```

With `libraries/http-client/` present and `libraries/audit-log/` absent,
`importer` loads after `http-client`, fine with audit logging disabled.
With `http-client` missing, `importer` is dropped and the report names
both the missing id and the plugin that could not load.

## `[api].exports`

The `[api]` section declares which of your module's exports other plugins
are allowed to invoke through the host. The rules:

```toml
[api]
exports = [{ name = "sync_now" }]
```

- Each `exports` entry is exactly one name, and the name must match
  `[A-Za-z_][A-Za-z0-9_.-]{0,63}` - no colons. `candyriya:hello_world` is
  **not** a valid export name; `hello_world` is.
- Nothing else is declared about the export: no argument types, no return
  types, no documentation. The ABI is "bytes in, bytes out", fixed by
  extism; the host does not type-check cross-plugin calls.
- Only listed names are considered callable. An unlisted export of your
  module is invisible to peers - it may still be called by the host itself
  (e.g. `on_event`).
- Missing `[api]`, or an empty `exports` list, means the plugin exports
  nothing to its peers. Empty is the default and the safe one.

**Status, plainly: nothing can call your exports today.** The list is
parsed, validated (names and duplicates are checked, invalid entries are
rejected) and indexed by the dependency resolver into a per-plugin
`exports` table - that index exists so the planned `call_api(plugin_id,
fn_name, args)` host function has something to consult. There is no
`call_api` yet, and there are no host functions registered at all. An
export you list gets you a validated line in an unused index. Do not ship
a plugin that *depends* on being called by a peer until the host-function
milestone lands; declaring `[api].exports` is correct preparation, not a
working API.

What you *should* do: pick your export names now, keep them stable, and
document their JSON shapes in your plugin's own README. When the host
functions land, the names you have already shipped are what peers will
call.

## Implementing a plugin

A minimal event-reactive plugin needs exactly two things: a wasm module
exporting `on_event`, and a `plugin.toml` next to it. Walk through it.

### 1. The event contract

The host (once events are wired - see
[Events](#events)) calls the export `on_event` on every loaded plugin,
one event at a time, with the envelope as a flat JSON document:

```json
{"id":1,"kind":"login","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","protocol_version":769,"occurred_at_unix_ms":1730000000000}
```

Your answer is read back as bytes and parsed as an `OutcomeReport`:

```json
{"cancelled":true,"cancel_reason":"banned from this server"}
```

`cancel_reason` is optional; `{"cancelled":false}` is the answer to "let
it through". Anything that is not a valid `OutcomeReport` is recorded as
`Outcome::Failed`, never as a cancellation - garbage is not a crash and is
not a veto.

### 2. The minimal module

The smallest honest example is in Rust (see
[Authoring in Rust](#authoring-in-rust) for the tooling). The host knows
nothing about Rust; it only knows the Extism ABI, so an equivalent plugin
in WAT also works - the repository's own test fixtures are raw WAT modules
that import `extism:host/env`'s `alloc`/`store_u8`/`output_set` and are
compiled at test time (`crates/plugin/candyriya-plugin/tests/common/mod.rs`
is the reference). A plugin that answers `{"cancelled":false}` to every
event is a handful of WAT lines on that pattern.

Neither variant needs imports from the proxy: a minimal module does not
import any candyriya-provided host function. It exports `memory` and
`on_event` and nothing else.

### 3. The `plugin.toml`

```toml
id = "hello-world"
version = "0.1.0"
name = "Hello World"
plugin_main = "on_event"
plugin_api = "0.1"
```

`plugin_main` is checked for existence when the plugin loads - the wasm
must export it, or the plugin is recorded as `Failed` with the reason.
Note carefully: **the host never calls `plugin_main` today.** The check is
the whole of the lifecycle integration right now. The name exists to give
the plugin one well-known entry point; when the host-function milestone
lands, the host will call it during plugin startup. Write it as if it will
be called (keep it cheap), but know that today only its existence matters.

There is **no `metadata` export** and no `metadata(plugin_id)` host
function today. The manifest on disk *is* the metadata - that is what the
loader parses and what gets validated. If you want your plugin's
version/description in tooling, the source of truth is your `plugin.toml`.
A host function exposing metadata to peers (`metadata(plugin_id)` in the
design notes) is planned with the host-function milestone and does not
exist yet. Any doc you see describing a `metadata` export as currently
supported is wrong.

### 4. Drop it in and verify

```text
<data_dir>/plugins/hello-world/plugin.toml
<data_dir>/plugins/hello-world/hello-world.wasm
```

Start the proxy. At startup you will see:

- a line per issue the loader found (`plugin: <where>: <why>`) for anything
  broken in your directory - bad TOML, id/dir mismatch, missing export;
- a final `plugin startup load complete` info line with `loaded`/`total`
  counts and the id list;
- your plugin's `Failed(reason)` record visible to host code via the
  service registry (`plugins.set`) - it is never silent, but it never runs
  either.

That is all. **`/candyriya` does not list plugins today** - the command
set is `info|servers|switch|end|reload`, and the plugin set is not surfaced
through it. `/candyriya reload plugins` is the way to (re)load a plugin
without a full restart; see [Hot reload](#hot-reload). If a plugin fails to
load, fix the files (or, for a new plugin, place them) and reload - you will
see either the issue line, or `loaded` increment.

## Events

The host can dispatch five kinds of game events to plugins. Each maps to
an `EventKind` variant in `candyriya-plugin-api`; the payload travels
*inside* the variant, so the `kind` tag and the data are one flat JSON
object on the wire. Ids come from the host, never from the plugin.

| `kind` | Wire name | Fields |
|---|---|---|
| Connection accepted | `connection_allowed` | `uuid`, `name`, `protocol` (u32), `address` (`ip:port` as seen by the proxy) |
| Connection rejected/kicked | `connection_kicked` | `uuid`, `name`, `reason` |
| Login completed | `login` | `uuid`, `name`, `protocol_version` (u32) |
| Player moved backends | `switch_server` | `uuid`, `name`, `from_server`, `to_server` |
| Disconnected | `disconnect` | `uuid`, `name`, `server`, `reason` |

Wire examples, as seen from your plugin's `on_event`:

```json
{"id":4,"kind":"connection_allowed","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","protocol":769,"address":"203.0.113.7:54412","occurred_at_unix_ms":1730000001000}
{"id":7,"kind":"switch_server","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","from_server":"lobby","to_server":"pvp","occurred_at_unix_ms":1730000009000}
{"id":9,"kind":"disconnect","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","server":"pvp","reason":"You were kicked by Notch","occurred_at_unix_ms":1730000123000}
```

Details:

- `id` is monotonic from 1, allocated by the host. `occurred_at_unix_ms` is
  the wall-clock time the host stamped the dispatch. Both are opaque facts
  to you.
- UUIDs are hyphenated canonical form; names are the game profile names.
- `disconnect`'s `reason` is a plain, carrier-free string - not a chat
  component. The proxy renders it before showing it to anyone.
- Tags this ABI version does not know survive round-trips as
  `EventKind::Unknown(tag)`: your plugin does not crash on a newer event
  kind, it just cannot match on it. Unknown *fields* are dropped on the
  way in and never re-appear on the way out.

### Cancellation semantics

- A plugin answers a dispatch with an `OutcomeReport`
  (`{"cancelled":true,"cancel_reason":"..."}` or `{"cancelled":false}`),
  or with the compact `EventCancellation` shape
  (`{"uuid":"...","event_id":7,"cancelled":true}`).
- **Every loaded plugin is asked, in load order, even after a plugin has
  cancelled.** The per-plugin outcomes are kept for debugging.
- **A cancellation wins**: if any answer has `cancelled: true`, the
  aggregate result is cancelled regardless of where in the order it
  happened or what earlier plugins said.
- `cancel_reason` carries the reason given by the *first* cancelling
  plugin in load order.
- A plugin that does not answer within the per-plugin timeout (10 s
  default for `on_event`) is logged as an error and recorded as `Timeout`;
  it does **not** cancel. The same holds for `Failed` (broken export,
  undecodable answer).
- `Disabled` and `Failed` plugins are not dispatched to and do not appear
  in the outcome list.

The result the host works with is a `DispatchResult`:

```text
DispatchResult {
    cancelled: bool,
    cancel_reason: Option<String>,   // first cancelling plugin in load order
    per_plugin_outcomes: Vec<(String, Outcome)>,  // every loaded plugin, load order
}
```

**Status, plainly:** none of the five events are wired into the live
connection pipeline yet. The dispatcher (`ProxyEventDispatcher`) is a
tested library - the tests prove the fan-out, the timeout behavior, the
load-order accounting and the cancellation aggregation with real modules -
but no production call site fires an event today, so today your `on_event`
never runs. When the wiring lands (connection admission, kick paths,
login completion, backend switching, disconnect), the contract above is
the one it will use: cancelled means the action does not happen and
`cancel_reason` is what the client is told.

## I18n

The proxy's user-facing strings live in TOML locale files with MiniMessage
values. Two locales are built in: `en` (default) and `ex_girl` (Russian,
one very opinionated voice).

### Core layout

```text
<data_dir>/locales/en/common.toml        # mandatory file name
<data_dir>/locales/en/candyriya.toml     # core proxy strings
<data_dir>/locales/ex_girl/common.toml
<data_dir>/locales/ex_girl/candyriya.toml
```

Embedded copies are the source of truth; a data directory can override
them per file. Missing files are fine; a file that fails to parse is a
warn and the embedded defaults are used **for that file only**. Both
`common.toml` files must exist in the *embedded* set - they do, always;
on disk they are optional.

Lookup chain (in order, first hit wins):

1. `<active>/candyriya.toml` (core), then each registered plugin file, in
   registration order;
2. `en/candyriya.toml`, then each registered plugin file;
3. `<active>/common.toml`;
4. `en/common.toml`;
5. the key itself, with a one-time `warn` per missing key.

Placeholders are `{name}` (named) and `{0}`, `{1}` (positional). The
lookup never panics and a missing key is never an error.

### Plugin locale files

The host-side hook for plugin locales is
`I18n::register_plugin_locale(plugin_id, dir)` - it loads
`<dir>/<active-locale>/<plugin_id>.toml` and, when the active locale is
not English, `<dir>/en/<plugin_id>.toml`. There is no `common.toml` for
plugins; your file is your file.

**Status, plainly: this is not wired into the proxy yet.** The plugin
loader does not call `register_plugin_locale`, and no plugin locale files
are loaded at proxy startup today - the only call sites are in the i18n
crate's own tests. The expected layout, for when the wiring lands, is a
`locales/` directory in your plugin's own directory:

```text
<data_dir>/plugins/my-plugin/locales/en/my-plugin.toml
<data_dir>/plugins/my-plugin/locales/ex_girl/my-plugin.toml
```

Name the file after your plugin id. Values are MiniMessage, flat keys
quoted as strings:

```toml
"plugin.my-plugin.connected" = "<gray>Connected to <server></gray>"
"plugin.my-plugin.restarting" = "<red>The server is restarting</red>"
```

Rules for plugin authors:

- **Namespace your keys as `plugin.<your-id>.*`.** That is the convention
  from the project's i18n design notes - core keys use their own
  prefixes (`proxy.*`, `common.*`), plugin keys use `plugin.*`. Be aware
  it is **not enforced by the code today** - nothing stops you from using
  any flat key, and nothing catches a collision for you. Keep to the
  namespace anyway; colliding keys will bite you the moment enforcement
  lands.
- **Do not shadow core keys, and do not try to shadow other plugins'
  keys.** Plugin files are checked *after* the core files in the lookup
  chain, so core strings are not overridable. Within the plugin tier,
  earlier-registered files are checked first and win, so you can only
  shadow a plugin registered after you - and everyone will hate you for
  it. Keep your file to your own namespaced keys.
- Values are MiniMessage, parsed with `lobsterchat` at the point of use.
  The i18n layer only stores and interpolates strings; it deliberately
  does not parse values at lookup time, so a bad value in your file is
  not caught until someone renders it (use `parse_minimessage` yourself
  if you want the strict gate).
- Template files with the same key set for every locale you ship; adding
  a key without both locales is a bug, same rule as core.

There is one more piece of this that does not exist yet: the WASM host
functions `i18n.t` / `i18n.locale`. They are planned for the
host-function milestone, and they are the only way a plugin will ever
read proxy strings from inside wasm. **Today your plugin cannot call the
i18n service at all** - no host functions are registered. If you put a
user-facing string in your Rust/WAT source today, it will be hardcoded
English (or whatever you write); when the host functions land, move it into
your `locales/` files and call `i18n.t`.

## Limits

Every plugin runs under a sandbox. Operator-facing knobs live in the
`[plugins]` section of `candyriya.toml`; the remaining caps live in the
runtime's defaults (`Limits::default`) and are not exposed in the config
file today.

| `[plugins]` key | Default | Meaning | `0` means |
|---|---|---|---|
| `max-memory-pages` | `64` (4 MiB) | Max linear-memory pages summed across all memories of the module; checked statically **before compile** (initial size may not exceed the cap; declared maxima may, growth is clamped at runtime) | rejected by `[plugins]` validation; the load-time static check also skips a zero cap |
| `max-fuel` | `0` | Fuel per plugin call; enforced per call via extism's fuel metering | **unlimited** (metering disabled entirely) |
| `max-calls` | `2000` | Max calls per plugin per 5 s window (`CALL_WINDOW`), denied calls consume no quota | **unlimited** |
| `max-call-timeout` | `"5s"` | Wall-clock deadline of one call; enforced with the wasmtime epoch deadline (`Manifest::with_timeout`). Accepts `"250ms"`, `"5s"`, `"2m"`, `"1h"`; must be ≥ 1 ms | rejected by validation |
| `max-output-bytes` | `1048576` (1 MiB) | Max bytes one call may return; checked when the output is read back | zero: only a zero-byte output passes |
| `max-input-bytes` | `16777216` (16 MiB) | Max bytes the host may send into one call; checked first, before anything is copied | zero: only empty calls pass |
| `max-log-bytes` | `1048576` (1 MiB) | Max bytes of plugin log output per invocation; lines that do not fit are dropped | silence |

Runtime-only caps (defaults, not in `[plugins]`):

| Cap | Default | `0` means |
|---|---|---|
| `max-table-elements` | `16384` | static check skipped - no cap enforced |
| `max-functions` | `65536` | static check skipped - no cap enforced |
| `max-instances` | `1` | **unlimited** (live instances per plugin id; staging for hot reload uses a private registry and is never rejected by this) |
| `max-open-handles` | `64` | **unlimited** |

Two notes on zeroes to keep you out of trouble. First, the `[plugins]`
validator refuses the zeros it considers meaningless (`max-memory-pages =
0`, `max-call-timeout = "0s"`), and `max-output-bytes`/`max-input-bytes`
are hard caps of the stated size. Second, the static (parse-time) checks
for memory, functions and tables skip their caps entirely when a cap is
`0` - the runtime code treats zero there as "no cap" - so a manually built
`Limits` with zeroes behaves more permissively than the `[plugins]`
validation would suggest. Do not set zero except where it means unlimited
(fuel, calls, handles, instances).

One more wrinkle: the generated `[plugins]` template pins
`max-call-timeout = "5s"` and that wins when the proxy wires limits from
the config. `Limits::default()` on its own would give you 30 s; the
configuration value is what a real deployment gets.

Notes worth internalizing:

- `0` is "unlimited" only for `max-fuel`, `max-calls`, `max-open-handles`
  and `max-instances`. For the byte caps it is a hard cap of the stated
  size (zero bytes), and for the static caps and `max-memory-pages` the
  zero behaves as "no cap" at load time while being refused by `[plugins]`
  validation. Pick the documented defaults and move on.
- The table cap is enforced **statically only**; growth of a table with no
  declared maximum at runtime is bounded by the guest alone.
- The mailbox is bounded at 16; a full mailbox is `PluginError::Busy`,
  never a blocked caller.
- There is **no per-plugin opt-out** and no "context map" in `plugin.toml`
  for declaring different limits for different invocations.
  `cache_quota_bytes` / `cache_quota_resources` are the only per-plugin
  quotas you can declare in the manifest, and they cover the cache
  namespace, nothing else. Raising the limits above requires the operator
  to edit `[plugins]`.

### What happens when a limit fires

- **Memory**: a module whose declared initial pages exceed the cap never
  reaches the compiler - `PluginError::LimitExceeded { kind:
  "max-memory-pages" }`. Growth beyond the runtime budget traps the call.
- **Fuel**: when `max-fuel` is non-zero, each call gets that fuel. An
  infinite loop exhausts it; extism surfaces the trap and the host maps it
  to `PluginError::FuelExhausted`. **The plugin cannot observe or handle
  this** - it is not a guest-visible condition, it is a trap; the guest is
  stopped, the host records the error. Fuel re-arms per call, so a fuel
  kill does not wedge the plugin forever (an operator raising `max-fuel`
  and reloading gets it back alive). When `max-fuel` is `0`, fuel is not
  metered at all - the only thing stopping a stuck call then is
  `max-call-timeout`.
- **Timeout**: the call surfaces as `PluginError::Timeout` after the
  deadline; the plugin keeps whatever state it had (the call is not
  cancelled).
- **Call rate**: `max-calls` cap reached → `PluginError::RateLimited`;
  denied calls cost no quota; the window is 5 s.
- **Panic**: a host-side panic while the plugin runs is caught at the FFI
  boundary; the call reports `PluginError::CallFailed` with "plugin
  panicked during the call". The worker thread lives on and keeps serving.
- **Bytes**: oversized input is rejected before anything else; oversized
  output is rejected after the fact with `LimitExceeded
  { kind: "max-output-bytes", .. }`.

## Hot reload

**Status, plainly: `/candyriya reload` exists and works.** It runs the
config phase and the plugin phase (either alone: `/candyriya reload
config`, `/candyriya reload plugins`; default is both, config first).
The plugin phase re-reads every plugin from disk without a restart and
reports a summary to the invoker: `N plugins reloaded, M failed` (green on
full success, red otherwise), plus one line per problem. The command never
crashes the host - a failed reload leaves the running plugin set in place.

The reload command drives the staging-first-then-swap machinery from
`candyriya-plugin`, in two passes:

- **Pass A — running plugins.** Every id already in the set is reloaded
  from disk exactly as `PluginReloader::reload(id)` does for one plugin:

  1. Re-read and re-validate the manifest (the id must still match the
     directory); find `<id>.wasm` or `lib.wasm`. No staging of anything →
     running plugin untouched.
  2. Build a *fresh, private* instance registry for the replacement - the
     outgoing and staged instances are briefly both alive, and
     `max-instances` must not reject a legitimate stage.
  3. Compile and instantiate the module under the configured limits.
  4. Check the module exports `plugin_main`. A module that does not is
     rejected here - still nothing is touched.
  5. **Only now** swap the record in place: load order and the
     `Loaded`/`Disabled` switch are preserved; plugin-owned services in
     the service registry are revoked in bulk (a dead plugin must not
     leave live handles); core services are never touched. A lost stage is
     an error and the old plugin keeps serving - the swap itself is
     infallible.
  6. The old instance is never joined. Its mailbox is closed; a healthy
     worker exits on its own; a plugin stuck in a wasm call keeps its
     thread until the call returns, but never blocks the reloader or
     anyone else. Dropping the instance never waits.

  A plugin in `Failed` state whose files are fixed is resurrected the same
  way. Deleting a plugin's files does **not** unload the running plugin -
  deleting only ever affects the next load, never unloads a live one; on
  reload such a plugin is reported as kept, not as failed.

- **Pass B — newcomers.** The `plugins/` and `libraries/` roots are
  re-scanned. Any discovered id that is not already in the set is carried
  through the dependency resolver and registered, libraries first, in
  resolved order; newcomers are appended after the existing set. A broken
  new plugin lands in the report as failed (never replacing a running
  instance — there was none) and its issues are logged. A discovery or
  dependency-resolution failure registers *nothing* new; the running set
  keeps whatever pass A produced. See `PluginReloader::reload_all` for the
  exact report shape (`reloaded` / `loaded` / `failed` / `new_failed` /
  `missing` / `issues`).

- **`Watch`** is a recursive `notify` watcher over `plugins/`. Any
  create/modify/remove turns into one `plugin-reload-<id>` task on the
  scheduler per affected id (250 ms debounce per id; bursts are dropped).
  Each task calls `PluginReloader::reload` and logs the outcome - a failed
  stage logs an error and the running plugin stays untouched. Note the CLI
  does not arm a `Watch` today; reload is operator-triggered.

- Quotas and plugin data directories live outside the swap: the plugin's
  `config/<id>/` directory and its cache namespace survive a reload, as
  does its disabled state.

Failure modes you can rely on: a mid-edit save keeps the old instance; a
broken wasm fails the stage and the old plugin serves on; the hot-reloaded
plugin must re-register anything it exported (host functions notwithstanding
- plugin-owned services are revoked and are not auto-restored; that is the
plugin's job at startup, when the host-function milestone lands).

## Authoring in Rust

The host knows only the Extism ABI, not Rust - a Rust plugin is just the
easiest way to produce one. The official SDK is
[`extism-pdk`](https://crates.io/crates/extism-pdk); keep it brief and treat
this as orientation, not a tutorial.

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
extism-pdk = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

```rust
use extism_pdk::*;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct EventEnvelope {
    id: u64,
    kind: String,
    #[serde(flatten)]
    fields: serde_json::Value, // kind-specific keys land here
    occurred_at_unix_ms: u64,
}

#[derive(Serialize)]
struct OutcomeReport {
    cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cancel_reason: Option<String>,
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<String> {
    let event: EventEnvelope = serde_json::from_str(&input)?;
    let _ = event; // react to the event here
    Ok(serde_json::to_string(&OutcomeReport {
        cancelled: false,
        cancel_reason: None,
    })?)
}
```

Build with `cargo build --target wasm32-unknown-unknown` (add the target
with rustup) and ship `target/wasm32-unknown-unknown/release/<crate>.wasm`
as `plugins/<id>/<id>.wasm`. Raw ABI is also fair game: exports are
`(func (result i32))` at the wasm level, with input and output handed
around through extism's buffer functions (`alloc`, `output_set`,
`store_u8`, `input_load_u8`, ...); the repository's test fixtures compile
raw WAT on that pattern and are a known-good reference.

Two things to not expect:

- **No published Rust crate for the Candyriya plugin types yet.**
  `candyriya-plugin-api` is a host-side crate (`#![forbid(unsafe_code)]`,
  no runtime). You mirror `EventEnvelope` / `OutcomeReport` and friends in
  your own code as above. The wire format is JSON and built for stability
  (the ABI compatibility rule is same-major, equal-or-older-minor) - but
  expect additive changes and pin what you rely on.
- **No host functions to call yet.** Your plugin cannot read the i18n
  service, query the player roster, log through the proxy, schedule tasks,
  or call peers. All of that is the host-function milestone. Write the
  plugin so it does useful, self-contained work (answer events, read its
  own data directory) and the rest will plug in.
