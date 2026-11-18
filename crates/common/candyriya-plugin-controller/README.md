# candyriya-plugin-controller

Pure data model of the native plugin controller: the part of the native
plugins epic that is a contract, not a runtime.

What lives here:

- `PluginWireState` — the 11-state wire machine vocabulary (`DISCOVERED` ...
  `UNLOADED`), closed enum, no `FAILED` state; failure travels as
  `record.reason` through the `UNLOADING -> UNLOADED` teardown path.
- `PluginRecord` — the controller's view of one discovered plugin: identity
  fields, wire state, restart bookkeeping, last exit and the failure reason.
- `PluginRegistry` — the in-memory `id -> record` map with the transition
  allow-list enforced on every state change.
- `PluginMsg` / `PluginAck` / `Generation` — the type-erased wire vocabulary
  for the controller <-> plugin-unit channel.
- `machine::apply` — a stub that lands next: the real transition table with
  dep gates, deadlines and stale-generation rejection is a later phase.

What deliberately does NOT live here: the dlopen boundary, plugins threads,
the tokio runtime, unsafe code, I/O. The actual machine and the tree wiring
are layered on top by the integration agents; this crate stays a boring,
reviewable contract crate — same philosophy as `candyriya-native-abi`.
