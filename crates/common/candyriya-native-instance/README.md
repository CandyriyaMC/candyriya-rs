# candyriya-native-instance

The dynamic-library boundary for candyriya native plugins. It opens a
plugin's `.dll` / `.so` / `.dylib`, resolves the frozen ABI exports, and runs
the fail-closed init handshake. It is one of exactly two crates in the
workspace with a scoped `unsafe` opt-out (the other is
[`candyriya-native-abi`], the contract crate).

This crate depends on `candyriya-native-abi` for every ABI type it touches;
it never redefines a contract type, never adds an export name that is not
documented below, and never calls a plugin function on its own.

## Unsafe surface

This crate carries the workspace's scoped `unsafe` opt-out for the plugin
boundary. Every `unsafe fn` and every `unsafe` block in it documents the
invariant that makes it sound; auditing those invariants is the review that
comes with this opt-out. The unsafe surface is deliberately small: opening
the library, reinterpreting a probe as an ABI signature, and the init call
itself — nothing else.

## When to use it

Use it when you are building the layer that loads native plugins — the
supervisor/controller side of the native plugin machinery. It gives you:

- [`LibraryLoader`] — open a library (`LibraryLoader::open`), probe an export
  (`LibraryLoader::probe_export`), unload it cleanly (`LibraryLoader::close`);
- [`validate_handshake`] / [`validate_init_output`] — fail-closed checks of
  both sides of the init handshake;
- [`initialize`] — call the plugin's `candyriya_plugin_init` and copy the
  reported identity out before the buffers stop being valid;
- [`init_plugin`] — the same handshake, table by shared reference, for
  callers that carry no unsafe;
- [`on_load`] / [`on_enable`] — call the two mandatory lifecycle exports
  with the same pre-flight validation, equally safe for non-unsafe crates;
- [`export_exists`] — answer "does this library export X?" without unsafe
  at the call site;
- [`PluginToken`] — mint the opaque per-instance token the V1 table hands
  out, keeping the backing allocation alive for the plugin's lifetime.

Do **not** use it to:

- run plugin lifecycle logic beyond the init call — threads, mailboxes,
  deadlines, reload staging and instance teardown are built *on top* of this
  crate, not inside it;
- call any export whose signature you have not pinned — this crate hands out
  opaque [`libloading::Symbol`] handles and the typed cast happens at the call
  site, visibly;
- load plugins for plugin authors — authors depend on the SDK and never link
  this crate.

## ABI versioning

- The ABI V1 is **frozen**: `candyriya-native-abi` defines it and this crate
  never extends it. `abi_version` must be `1` everywhere.
- `candyriya_plugin_init`, `candyriya_plugin_on_load` and
  `candyriya_plugin_on_enable` are **mandatory** exports; a library missing
  `candyriya_plugin_init` is not loadable at all.
- `candyriya_plugin_on_disable` and `candyriya_plugin_on_unload` are
  **optional, probed** exports: they are looked up at load time and a library
  without them is still accepted. Callers treat a failed probe as "this
  plugin has no disable/unload callback" and must not call anything it did not
  find. V1 stays frozen; these two symbols become contractual only in a
  future ABI version.
- A plugin that answers the handshake with anything other than
  `status == Ok` and an echo of `abi_version == 1` is refused. There is no
  compatibility shim for that — fail closed is the design.

## Testing

The integration tests in `tests/loader.rs` dlopen the real reference plugin.
They need the cdylib to exist first and are therefore `#[ignore]`d by default:

```sh
cargo build --release -p greeting
cargo test -p candyriya-native-instance -- --ignored
```

Plain `cargo test -p candyriya-native-instance` stays green without that
build. The example below is compiled on every `cargo test --doc` run.

## Example flow

The full open → probe → init → unload flow. `no_run` because it needs the
reference cdylib to exist on disk:

```rust,no_run
use std::path::Path;

use candyriya_native_abi::{CandyriyaPluginApiV1, CandyriyaPluginHandle, PluginInitArgs};
use candyriya_native_instance::{LibraryLoader, OpenFlags, PLUGIN_ON_DISABLE_EXPORT, initialize};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let artifact = if cfg!(windows) {
    "greeting.dll"
} else if cfg!(target_os = "macos") {
    "libgreeting.dylib"
} else {
    "libgreeting.so"
};
let path = Path::new("target/release").join(artifact);

// 1. map the library (RTLD_NOW + RTLD_LOCAL on unix, LoadLibraryW on Windows)
let library = LibraryLoader::open(&path, OpenFlags::Now)?;

// 2. optional probed export: absence is fine, calling it would not be
let on_disable = unsafe { library.probe_export(PLUGIN_ON_DISABLE_EXPORT.as_ptr()) };
match on_disable {
    Ok(_symbol) => println!("plugin ships a disable callback"),
    Err(_) => println!("no disable callback (fine for V1 plugins)"),
}

// 3. the proxy mints one opaque handle per plugin instance
//    (a raw token only the proxy interprets; one allocation per plugin,
//    dropped at unload time)
// SAFETY: the pointer denotes this plugin instance's token for the proxy's lifetime;
// the plugin only echoes it back and never dereferences it.
let handle = unsafe { CandyriyaPluginHandle::from_raw(Box::into_raw(Box::new(())) as *mut core::ffi::c_void) };
let table = CandyriyaPluginApiV1::new(handle);
let args = PluginInitArgs::new(&table);

// 4. fail-closed handshake: probe init, call it, check status + echo + identity
let plugin = unsafe { initialize(&library, &args) }?;
println!(
    "loaded {} v{}",
    String::from_utf8_lossy(&plugin.identity.name),
    String::from_utf8_lossy(&plugin.identity.version),
);

// 5. drop or explicit close unloads the mapping
library.close()?;
# Ok(())
# }
```

[`candyriya-native-abi`]: https://github.com/CandyriyaMC/candyriya-rs/tree/experiment/native-plugins/crates/common/candyriya-native-abi
[`LibraryLoader`]: https://docs.rs/candyriya-native-instance
[`validate_handshake`]: https://docs.rs/candyriya-native-instance
[`validate_init_output`]: https://docs.rs/candyriya-native-instance
[`initialize`]: https://docs.rs/candyriya-native-instance
[`init_plugin`]: https://docs.rs/candyriya-native-instance
[`on_load`]: https://docs.rs/candyriya-native-instance
[`on_enable`]: https://docs.rs/candyriya-native-instance
[`export_exists`]: https://docs.rs/candyriya-native-instance
[`PluginToken`]: https://docs.rs/candyriya-native-instance
[`libloading::Symbol`]: https://docs.rs/libloading
