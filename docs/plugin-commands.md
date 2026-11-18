---
layout: default
title: Plugin commands
---

# Plugin commands

`/candyriya plugin` is the phase-2 operator surface for native plugins ([issue #310](https://github.com/CandyriyaMC/candyriya-rs/issues/310)). Plugins are `.dll` / `.so` / `.dylib` libraries supervised as `plugin-<id>` units under `plugin-controller` (see [Architecture](architecture)). This page is v1 - output is descriptive and will evolve with the implementation. Until phase 2 lands this is an unknown command; syntax, nodes and output may change.

## Conventions

- `<plugin>`: plugin id (`[a-z0-9_.-]{1,64}`), matched exactly first, then case-insensitively. An unknown id is rejected with a line naming the known ids; the command does not run.
- States: lowercase rendering of the phase-2 machine - `discovered`, `loading`, `loaded`, `initializing`, `initialized`, `enabling`, `running`, `disabling`, `disabled`, `unloading`, `unloaded`. There is **no `failed` state** - a failed transition latches a reason; the plugin travels `unloading` -> `unloaded`.
- "Failure is reported" below means one red line naming the reason; the plugin rests where the machine left it and nothing is retried. Metrics are never printed.
- Gates are per subcommand in `candyriya.command.plugin.*`, default-deny, not implied by the root or `plugin` literal; a missing grant looks like an unknown command. The console's `*` covers the namespace. Nodes: `candyriya.command.plugin.{list,info,enable,disable,reload,unload,tree}`; `candyriya.command.plugin.*` covers all.

### `/candyriya plugin list`

One line per plugin known to the controller: id, version, wire state, outcome of the last lifecycle step (`ok`, `err`, `timeout`, or `-`), `RESTART ELECTED` (`yes` while the unit waits out a restart backoff; `no` otherwise - the recommended phase-2 native policy is `Never`, which is still an open decision tracked in issue #310), and the source library path. With nothing discovered, prints `No plugins found.`

Requires `candyriya.command.plugin.list`.
```shell
candyriya > /candyriya plugin list
ID        VERSION  STATE     LAST OUTCOME  RESTART ELECTED  SOURCE
greeting  0.1.0    running   ok            no               plugins/greeting/greeting.dll
quests    0.3.0    unloaded  err           no               plugins/quests/quests.dll
```

### `/candyriya plugin info <plugin>`

One-shot summary; the entry point is the symbol resolved at load. `Recent exits` is a snapshot of the supervision unit's recent exit statuses (bounded ring, fixed size in phase 2), oldest first, `-` when empty. A failure-latched plugin shows its reason.

Requires `candyriya.command.plugin.info`.
```shell
candyriya > /candyriya plugin info greeting
Plugin: greeting
Version: 0.1.0
Description: says hello on startup
Author: Jane Doe
Entry point: candyriya_plugin_init
State: running
Recent exits: -
```

### `/candyriya plugin enable <plugin>`

Drives the plugin to `running`: a not-mapped plugin runs the full pipeline `loading -> loaded -> initializing -> initialized -> enabling -> running`; a disabled (mapped, parked) plugin runs only `enabling -> running` and the library is not re-mapped. Enabling a running plugin is a no-op reporting success. A failure-latched plugin is not enabled - use `reload`. A failed phase (ABI handshake, `on_load`, `on_enable`) is reported with the reason; the plugin keeps its previous state.

Requires `candyriya.command.plugin.enable`.
```shell
candyriya > /candyriya plugin enable greeting
Plugin 'greeting' enabled.
```

### `/candyriya plugin disable <plugin>`

Cooperative stop only: quiesces dispatch for the id, drains in-flight calls, calls `on_disable`, waits for the acknowledgement. Disable never unmaps the library; the plugin ends `disabled`. Disabling an already disabled plugin is a no-op reporting success.

Requires `candyriya.command.plugin.disable`.
```shell
candyriya > /candyriya plugin disable greeting
Plugin 'greeting' disabled.
```

If the plugin does not acknowledge (deadline missed, `on_disable` error, thread gone) the response says so, the failure is reported with the reason, and the plugin is **not** safe to unload - it stays resident. Retry is idempotent. There is no force flag in phase 2 (force-unload is phase 3 - deferred). Unload is refused while this latch holds.

### `/candyriya plugin reload <plugin>`

Restage flow: `disable -> unload -> rescan -> enable`. Each step is a no-op when its precondition already holds; the rescan re-reads the manifest and the library, and `enable` is the full pipeline onto the new mapping. If disable is not acknowledged the sequence stops before unload and the failing step is reported with the reason; `plugin info` shows the resting state. The observable order is the contract; whether the replacement is staged beside the running instance (zero outage) or applied after the old mapping drops is an open decision tracked in issue #310.

Requires `candyriya.command.plugin.reload`.
```shell
candyriya > /candyriya plugin reload greeting
Plugin 'greeting' reloaded.
```

### `/candyriya plugin unload <plugin>`

Hard unload without reload. Requires the plugin to be in `disabled` state - to unload a running plugin, disable it first (a running plugin is rejected with `Plugin '<id>' is running; disable it first.`). Unload drops the context, closes the mailbox, joins the plugin OS thread (5 s bound) and unmaps the library; the unit exits and is reaped. A failure-latched plugin is refused - it is not safe to unload. If the thread misses the join bound it is detached and the mapping stays; the response says so.

Requires `candyriya.command.plugin.unload`.
```shell
candyriya > /candyriya plugin unload greeting
Plugin 'greeting' unloaded.
```

### `/candyriya plugin tree`

The `plugins` subtree, box-drawing like `/candyriya tree`, with pid/ppid and wire state on every line: the `plugins` group node, `plugin-controller`, then one line per `plugin-<id>` unit. No columns, no summary line; the bare `plugins` line prints when nothing is loaded.

Requires `candyriya.command.plugin.tree`.
```text
plugins [pid 5, ppid 1, running]
└── plugin-controller [pid 6, ppid 5, running]
    └── plugin-greeting [pid 7, ppid 6, running]
```
