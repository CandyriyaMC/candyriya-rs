//! Sandbox limits for plugins: memory, fuel, handles, byte caps, the
//! per-window call rate limit and the `[plugins]` config mapping.
//!
//! Fixtures are real wasm modules compiled from WAT at test time — no
//! network, no mocks; every assertion goes through the public API.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use candyriya_plugin::{
    HostState, InstanceSlots, LimitBuilder, Limits, PluginError, PluginInstance,
};

/// Write a bare `.wasm` file (no manifest) into `dir` and return its path.
fn write_wasm(dir: &Path, name: &str, wat_source: &str) -> PathBuf {
    let bytes = wat::parse_str(wat_source).expect("fixture wat compiles");
    let path = dir.join(name);
    fs::write(&path, bytes).expect("write wasm fixture");
    path
}

/// A module requesting 32 MiB of initial linear memory (512 pages), far
/// beyond the default 64-page cap.
const MEMORY_512_WAT: &str = r#"
(module
  (memory 512)
  (func (export "on_event") (result i32)
    (i32.const 0)))
"#;

/// A module with one page that tries to grow by 256 more pages at call
/// time. Growth beyond the 64-page budget traps via the store's memory
/// limiter.
const MEMORY_GROW_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (memory.grow (i32.const 256))))
"#;

/// A module that never returns; used with a finite fuel budget.
const INFINITE_LOOP_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (loop $spin
      (br $spin))
    (i32.const 0)))
"#;

/// A module declaring a table far beyond the default cap.
const TABLE_20000_WAT: &str = r#"
(module
  (table 20000 funcref)
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (i32.const 0)))
"#;

/// A module declaring exactly one function.
const ONE_FUNCTION_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (i32.const 0)))
"#;

/// A module declaring two functions.
const TWO_FUNCTIONS_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (i32.const 0))
  (func (export "other") (result i32)
    (i32.const 1)))
"#;

/// A module returning `size` bytes of uninitialized output.
fn output_wat(size: u64) -> String {
    let pages = (size / 65536 + 2).max(2);
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (memory (export "memory") {pages})
  (func (export "out") (result i32)
    (local $p i64)
    (local.set $p (call $alloc (i64.const {size})))
    (call $output_set (local.get $p) (i64.const {size}))
    (i32.const 0)))
"#
    )
}

fn expect_err(result: Result<PluginInstance, PluginError>) -> PluginError {
    match result {
        Ok(_) => panic!("expected plugin construction to fail"),
        Err(error) => error,
    }
}

#[test]
fn module_requesting_32mib_of_memory_fails_with_default_limits() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(temp.path(), "greedy.wasm", MEMORY_512_WAT);
    let dir = temp.path().join("data");

    let error = expect_err(PluginInstance::new(
        &wasm_path,
        HostState::new(dir),
        "greedy",
        &Limits::new(),
    ));
    assert!(
        matches!(
            &error,
            PluginError::LimitExceeded {
                kind: "max-memory-pages",
                limit: 64,
                requested: 512,
                ..
            }
        ),
        "expected max-memory-pages limit error, got: {error:?}"
    );
}

#[test]
fn memory_growth_beyond_the_cap_traps_at_runtime() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(temp.path(), "grow.wasm", MEMORY_GROW_WAT);
    let dir = temp.path().join("data");

    let instance = PluginInstance::new(&wasm_path, HostState::new(dir), "grower", &Limits::new())
        .expect("module with one initial page loads");

    let result = instance.call("on_event", b"");
    assert!(
        result.is_err(),
        "a growth of 256 pages beyond the 64-page budget must fail"
    );
}

#[test]
fn fuel_limited_infinite_loop_times_out() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(temp.path(), "loop.wasm", INFINITE_LOOP_WAT);
    let dir = temp.path().join("data");
    let limits = Limits::builder().max_fuel(1_000_000).build();

    let instance = PluginInstance::new(&wasm_path, HostState::new(dir), "fuel-loop", &limits)
        .expect("module compiles under fuel metering");

    let start = Instant::now();
    let error = instance
        .call("on_event", b"")
        .expect_err("an infinite loop with 1_000_000 fuel must error, not hang");
    let elapsed = start.elapsed();

    assert!(
        matches!(&error, PluginError::FuelExhausted { plugin_id, .. } if plugin_id == "fuel-loop"),
        "expected FuelExhausted, got: {error:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "fuel exhaustion must stop the loop promptly, took {elapsed:?}"
    );
}

#[test]
fn fuel_zero_is_unlimited_and_normal_calls_work() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(
        temp.path(),
        "normal.wasm",
        &common::response_plugin_wat("ok"),
    );
    let dir = temp.path().join("data");

    // Limits::default() carries max_fuel = 0 (unlimited)
    let instance = PluginInstance::new(
        &wasm_path,
        HostState::new(dir),
        "normal",
        &Limits::default(),
    )
    .expect("normal module loads");

    for _ in 0..4 {
        let out = instance.call("on_event", b"").expect("unlimited fuel call");
        assert_eq!(out, b"ok");
    }
}

#[test]
fn table_beyond_the_cap_is_rejected_at_load() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(temp.path(), "table.wasm", TABLE_20000_WAT);
    let dir = temp.path().join("data");

    let error = expect_err(PluginInstance::new(
        &wasm_path,
        HostState::new(dir),
        "table",
        &Limits::new(),
    ));
    assert!(
        matches!(
            &error,
            PluginError::LimitExceeded {
                kind: "max-table-elements",
                limit: 16384,
                requested: 20000,
                ..
            }
        ),
        "expected max-table-elements limit error, got: {error:?}"
    );
}

#[test]
fn function_cap_is_enforced() {
    let temp = tempfile::tempdir().expect("tempdir");
    let two = write_wasm(temp.path(), "two.wasm", TWO_FUNCTIONS_WAT);
    let one = write_wasm(temp.path(), "one.wasm", ONE_FUNCTION_WAT);

    // a cap of 1 rejects a two-function module
    let error = expect_err(PluginInstance::new(
        &two,
        HostState::new(temp.path().join("d1")),
        "two",
        &Limits::builder().max_functions(1).build(),
    ));
    assert!(
        matches!(
            &error,
            PluginError::LimitExceeded {
                kind: "max-functions",
                limit: 1,
                requested: 2,
                ..
            }
        ),
        "expected max-functions limit error, got: {error:?}"
    );

    // the same module loads untouched under the default cap
    PluginInstance::new(
        &one,
        HostState::new(temp.path().join("d2")),
        "one",
        &Limits::default(),
    )
    .expect("module within the default cap loads");
}

#[test]
fn input_beyond_the_cap_is_rejected_before_the_call() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(
        temp.path(),
        "normal.wasm",
        &common::response_plugin_wat("ok"),
    );
    let dir = temp.path().join("data");
    let limits = Limits::builder().max_input_bytes(16).build();

    let instance = PluginInstance::new(&wasm_path, HostState::new(dir), "input-cap", &limits)
        .expect("module loads");

    let error = instance
        .call("on_event", &[0u8; 17])
        .expect_err("17 bytes of input must be rejected with a 16-byte cap");
    assert!(
        matches!(
            &error,
            PluginError::LimitExceeded {
                kind: "max-input-bytes",
                limit: 16,
                requested: 17,
                ..
            }
        ),
        "expected max-input-bytes limit error, got: {error:?}"
    );

    // exactly at the cap passes
    assert_eq!(
        instance.call("on_event", &[0u8; 16]).expect("at cap"),
        b"ok"
    );
}

#[test]
fn output_beyond_the_cap_fails_and_at_cap_passes() {
    let temp = tempfile::tempdir().expect("tempdir");
    // 2 MiB of output
    let big = write_wasm(temp.path(), "big.wasm", &output_wat(2 * 1024 * 1024));
    let instance = PluginInstance::new(
        &big,
        HostState::new(temp.path().join("big-data")),
        "big",
        &Limits::new(), // default cap: 1 MiB
    )
    .expect("big-output module loads");

    let error = instance
        .call("out", b"")
        .expect_err("2 MiB of output must not pass a 1 MiB cap");
    assert!(
        matches!(
            &error,
            PluginError::LimitExceeded {
                kind: "max-output-bytes",
                limit: 1_048_576,
                requested: 2_097_152,
                ..
            }
        ),
        "expected max-output-bytes limit error, got: {error:?}"
    );

    // exactly at the cap passes
    let exact = write_wasm(temp.path(), "exact.wasm", &output_wat(2048));
    let exact_instance = PluginInstance::new(
        &exact,
        HostState::new(temp.path().join("exact-data")),
        "exact",
        &Limits::builder().max_output_bytes(2048).build(),
    )
    .expect("exact-output module loads");
    let out = exact_instance
        .call("out", b"")
        .expect("output at the cap passes");
    assert_eq!(out.len(), 2048);
}

#[test]
fn rate_limit_blocks_2000_calls_in_one_window() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(
        temp.path(),
        "normal.wasm",
        &common::response_plugin_wat("ok"),
    );
    let dir = temp.path().join("data");
    let limits = Limits::builder().max_calls(2000).build();

    let instance = PluginInstance::new(&wasm_path, HostState::new(dir), "rate", &limits)
        .expect("module loads");

    for _ in 0..2000 {
        instance
            .call("on_event", b"")
            .expect("first 2000 calls fit the window");
    }
    let error = instance
        .call("on_event", b"")
        .expect_err("the 2001st call in the window must be rate-limited");
    assert!(
        matches!(
            &error,
            PluginError::RateLimited {
                max_calls: 2000,
                window_secs: 5,
                ..
            }
        ),
        "expected RateLimited, got: {error:?}"
    );
}

#[test]
fn rate_limit_zero_is_unlimited() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(
        temp.path(),
        "normal.wasm",
        &common::response_plugin_wat("ok"),
    );
    let dir = temp.path().join("data");
    let limits = Limits::builder().max_calls(0).build();

    let instance = PluginInstance::new(&wasm_path, HostState::new(dir), "unlimited", &limits)
        .expect("module loads");
    for _ in 0..30 {
        instance
            .call("on_event", b"")
            .expect("max_calls = 0 must never limit");
    }
}

#[test]
fn open_handles_beyond_64_fail() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(
        temp.path(),
        "normal.wasm",
        &common::response_plugin_wat("ok"),
    );
    let dir = temp.path().join("data");

    // the default cap: 64 open handles
    let instance = PluginInstance::new(&wasm_path, HostState::new(dir), "handles", &Limits::new())
        .expect("module loads");
    let table = instance.handle_table();

    let handles: Vec<_> = (0..64)
        .map(|_| table.open().expect("64 handles fit"))
        .collect();
    assert_eq!(table.open_handles(), 64);
    assert!(table.is_at_capacity());

    let error = table
        .open()
        .expect_err("the 65th open handle must be rejected");
    assert!(
        matches!(
            &error,
            PluginError::LimitExceeded {
                kind: "max-open-handles",
                limit: 64,
                ..
            }
        ),
        "expected max-open-handles limit error, got: {error:?}"
    );

    // dropping one frees a slot
    drop(handles.into_iter().next().expect("at least one handle"));
    table.open().expect("a dropped handle frees a slot");
}

#[test]
fn instance_limit_default_is_a_single_instance_per_plugin() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(
        temp.path(),
        "normal.wasm",
        &common::response_plugin_wat("ok"),
    );
    let slots = Arc::new(InstanceSlots::new());

    let first = HostState::with_instance_slots(temp.path().join("d1"), Arc::clone(&slots));
    let second = HostState::with_instance_slots(temp.path().join("d2"), Arc::clone(&slots));
    let third = HostState::with_instance_slots(temp.path().join("d3"), Arc::clone(&slots));

    let instance = PluginInstance::new(&wasm_path, first, "same-id", &Limits::new())
        .expect("first instance fits the default single slot");
    let error = expect_err(PluginInstance::new(
        &wasm_path,
        second,
        "same-id",
        &Limits::new(),
    ));
    assert!(
        matches!(
            &error,
            PluginError::LimitExceeded {
                kind: "max-instances",
                limit: 1,
                requested: 2,
                ..
            }
        ),
        "expected max-instances limit error, got: {error:?}"
    );

    // a different plugin id is unaffected
    PluginInstance::new(
        &wasm_path,
        HostState::with_instance_slots(temp.path().join("d4"), Arc::clone(&slots)),
        "other-id",
        &Limits::new(),
    )
    .expect("a different id gets its own slot");

    // dropping the first instance releases the slot
    drop(instance);
    PluginInstance::new(&wasm_path, third, "same-id", &Limits::new())
        .expect("released slot accepts a new instance");
}

#[test]
fn instance_limit_zero_is_unlimited() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = write_wasm(
        temp.path(),
        "normal.wasm",
        &common::response_plugin_wat("ok"),
    );
    let slots = Arc::new(InstanceSlots::new());

    let mut instances = Vec::new();
    for index in 0..3 {
        let state = HostState::with_instance_slots(
            temp.path().join(format!("d{index}")),
            Arc::clone(&slots),
        );
        instances.push(
            PluginInstance::new(
                &wasm_path,
                state,
                "many",
                &Limits::builder().max_instances(0).build(),
            )
            .expect("max_instances = 0 must allow any number of instances"),
        );
    }
    assert_eq!(slots.live_count("many"), 3);
    drop(instances);
    assert_eq!(slots.live_count("many"), 0);
}

#[test]
fn from_config_maps_the_plugins_section() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("candyriya.toml");
    std::fs::write(
        &path,
        "[plugins]\nmax-memory-pages = 32\nmax-fuel = 1000000\nmax-calls = 100\nmax-call-timeout = \"7s\"\nmax-output-bytes = 2048\nmax-input-bytes = 4096\nmax-log-bytes = 128\n",
    )
    .expect("write config");

    let config = candyriya_config::Config::load(&path).expect("config loads");
    let limits = LimitBuilder::from_config(&config).build();

    assert_eq!(limits.max_memory_pages, 32);
    assert_eq!(limits.max_fuel, 1_000_000);
    assert_eq!(limits.max_calls, 100);
    assert_eq!(limits.max_timeout, Duration::from_secs(7));
    assert_eq!(limits.max_output_bytes, 2048);
    assert_eq!(limits.max_input_bytes, 4096);
    assert_eq!(limits.max_log_bytes, 128);
    // knobs the [plugins] section does not carry keep their defaults
    assert_eq!(
        limits.max_table_elements,
        Limits::default().max_table_elements
    );
    assert_eq!(limits.max_instances, Limits::default().max_instances);
    assert_eq!(limits.max_functions, Limits::default().max_functions);
    assert_eq!(limits.max_open_handles, Limits::default().max_open_handles);
}

#[test]
fn from_config_defaults_match_the_plugins_defaults() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("candyriya.toml");
    std::fs::write(&path, "").expect("write config");

    let config = candyriya_config::Config::load(&path).expect("config loads");
    let limits = LimitBuilder::from_config(&config).build();

    assert_eq!(limits.max_memory_pages, 64);
    assert_eq!(limits.max_fuel, 0);
    assert_eq!(limits.max_calls, 2000);
    assert_eq!(limits.max_timeout, Duration::from_secs(5));
    assert_eq!(limits.max_output_bytes, 1_048_576);
    assert_eq!(limits.max_input_bytes, 16_777_216);
    assert_eq!(limits.max_log_bytes, 1_048_576);
}
