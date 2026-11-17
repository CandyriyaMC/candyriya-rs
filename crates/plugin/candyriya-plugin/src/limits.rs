//! Sandbox limits for WASM plugins.
//!
//! Every plugin runs under [`Limits`]: caps on memory, tables, functions,
//! fuel, wall-clock time, bytes and open host handles. The operator-facing
//! subset is configurable through `candyriya-config`'s `[plugins]` section
//! (see [`LimitBuilder::from_config`]); everything else keeps the defaults
//! in [`Limits::default`].
//!
//! The knobs are enforced at three different moments, by design:
//!
//! * **compile time** — [`check_module`] parses the wasm module and rejects
//!   it before anything is compiled, sizing memory, tables and functions
//!   against the caps;
//! * **run time** — extism/wasmtime enforce the memory growth budget and
//!   per-call fuel (see `PluginBuilder::with_fuel_limit`), while the manifest
//!   timeout interrupts the store through epoch deadlines;
//! * **call time** — input/output byte caps, the per-window call rate limit
//!   and the handle/log accounting all live on the host-side call path.
//!
//! `0` means "unlimited" for `max_fuel`, `max_calls` and `max_open_handles`.
//! Every other cap treats `0` as a hard zero limit (reject any amount), which
//! is almost never what you want — `[plugins]` validation refuses the
//! zero values that make no sense.
//!
//! Table limits are enforced statically only: the `Store`'s resource limiter
//! slot is claimed by extism's own memory limiter, and there is no safe
//! chaining point in extism 1.x. A module whose declared table initial size
//! or declared table maximum exceeds the cap is rejected at load; growth of
//! an undeclared-maximum table at run time is bounded by the guest only.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wasmparser::{Parser, Payload, TypeRef};

use crate::error::PluginError;

/// Window over which [`Limits::max_calls`] is accounted per plugin.
pub const CALL_WINDOW: Duration = Duration::from_secs(5);

/// Sandbox limits for one plugin.
///
/// Construct with [`Limits::new`] (all defaults) or via [`LimitBuilder`];
/// `LimitBuilder::from_config` seeds the operator-facing subset from the
/// `[plugins]` section of `candyriya-config`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum linear-memory pages (64 KiB each) a plugin may use, summed
    /// across all memories declared by the module. Default `64` (4 MiB).
    pub max_memory_pages: u32,
    /// Maximum elements in any single table: declared initial size, and the
    /// declared maximum if the table has one. Default `16384`.
    pub max_table_elements: u32,
    /// Maximum simultaneously live instances of the same plugin id.
    /// Default `1` — transient staging for hot reload is accounted by the
    /// reloader, not here. `0` = unlimited.
    pub max_instances: u32,
    /// Maximum number of functions in a module (function imports plus
    /// declared functions). Default `65536`.
    pub max_functions: u32,
    /// Fuel per plugin call; `0` = unlimited. Default `0`.
    pub max_fuel: u64,
    /// Maximum open host handles (files, pipes, sockets — whatever the host
    /// hands out as an opaque handle) per plugin. Default `64`.
    /// `0` = unlimited.
    pub max_open_handles: u32,
    /// Maximum wall-clock time for a single plugin call. Default `30 s`.
    pub max_timeout: Duration,
    /// Maximum bytes one plugin call may return. Default `1 MiB`.
    pub max_output_bytes: usize,
    /// Maximum bytes accepted as input to one plugin call. Default `16 MiB`.
    pub max_input_bytes: usize,
    /// Maximum bytes of plugin log output per invocation. Default `1 MiB`.
    pub max_log_bytes: usize,
    /// Maximum calls per [`CALL_WINDOW`] per plugin; `0` = unlimited.
    /// Default `2000`.
    pub max_calls: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_memory_pages: 64,
            max_table_elements: 16_384,
            max_instances: 1,
            max_functions: 65_536,
            max_fuel: 0,
            max_open_handles: 64,
            max_timeout: Duration::from_secs(30),
            max_output_bytes: 1_048_576,
            max_input_bytes: 16_777_216,
            max_log_bytes: 1_048_576,
            max_calls: 2_000,
        }
    }
}

impl Limits {
    /// All defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start building a customized [`Limits`] from the defaults.
    pub fn builder() -> LimitBuilder {
        LimitBuilder::new()
    }

    /// Fuel limit as the runtime wants it: `0` maps to "no fuel metering".
    pub(crate) fn fuel_per_call(&self) -> Option<u64> {
        (self.max_fuel != 0).then_some(self.max_fuel)
    }
}

/// Chainable builder for [`Limits`].
///
/// Every knob starts from [`Limits::default`]; call the setters you need
/// and finish with [`LimitBuilder::build`]. `from_config` /
/// `from_plugins` seed the operator-facing `[plugins]` subset from
/// `candyriya-config` — the knobs that section does not carry keep their
/// defaults.
#[derive(Clone, Debug)]
pub struct LimitBuilder {
    limits: Limits,
}

impl LimitBuilder {
    pub fn new() -> Self {
        Self {
            limits: Limits::default(),
        }
    }

    /// Seed the operator-facing subset from the `[plugins]` section of a
    /// loaded `candyriya-config`.
    pub fn from_config(config: &candyriya_config::Config) -> Self {
        Self::from_plugins(&config.plugins)
    }

    /// Seed the operator-facing subset from a bare `[plugins]` section.
    pub fn from_plugins(plugins: &candyriya_config::PluginsConfig) -> Self {
        Self::new()
            .max_memory_pages(plugins.max_memory_pages)
            .max_fuel(plugins.max_fuel)
            .max_calls(plugins.max_calls)
            .max_timeout(plugins.max_call_timeout)
            .max_output_bytes(plugins.max_output_bytes)
            .max_input_bytes(plugins.max_input_bytes)
            .max_log_bytes(plugins.max_log_bytes)
    }

    pub fn max_memory_pages(mut self, pages: u32) -> Self {
        self.limits.max_memory_pages = pages;
        self
    }

    pub fn max_table_elements(mut self, elements: u32) -> Self {
        self.limits.max_table_elements = elements;
        self
    }

    pub fn max_instances(mut self, instances: u32) -> Self {
        self.limits.max_instances = instances;
        self
    }

    pub fn max_functions(mut self, functions: u32) -> Self {
        self.limits.max_functions = functions;
        self
    }

    pub fn max_fuel(mut self, fuel: u64) -> Self {
        self.limits.max_fuel = fuel;
        self
    }

    pub fn max_open_handles(mut self, handles: u32) -> Self {
        self.limits.max_open_handles = handles;
        self
    }

    pub fn max_timeout(mut self, timeout: Duration) -> Self {
        self.limits.max_timeout = timeout;
        self
    }

    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.limits.max_output_bytes = bytes;
        self
    }

    pub fn max_input_bytes(mut self, bytes: usize) -> Self {
        self.limits.max_input_bytes = bytes;
        self
    }

    pub fn max_log_bytes(mut self, bytes: usize) -> Self {
        self.limits.max_log_bytes = bytes;
        self
    }

    pub fn max_calls(mut self, calls: u32) -> Self {
        self.limits.max_calls = calls;
        self
    }

    pub fn build(self) -> Limits {
        self.limits
    }
}

impl Default for LimitBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Live-instance accounting shared by a scope (e.g. one loader run, one host
/// state). Cloned into every [`crate::HostState`] created for the scope; the
/// counts themselves live behind a single mutex.
#[derive(Debug, Default)]
pub struct InstanceSlots {
    inner: Mutex<HashMap<String, u32>>,
}

impl InstanceSlots {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many live instances of `plugin_id` the scope currently holds.
    pub fn live_count(&self, plugin_id: &str) -> u32 {
        self.inner.lock().get(plugin_id).copied().unwrap_or(0)
    }

    pub(crate) fn try_acquire(&self, plugin_id: &str, max: u32) -> Result<(), PluginError> {
        let mut map = self.inner.lock();
        let live = map.get(plugin_id).copied().unwrap_or(0);
        // `max == 0` means unlimited: still account the slot, just never reject
        if max != 0 && live >= max {
            return Err(PluginError::LimitExceeded {
                plugin_id: plugin_id.to_owned(),
                kind: "max-instances",
                limit: u64::from(max),
                requested: u64::from(live) + 1,
            });
        }
        map.insert(plugin_id.to_owned(), live + 1);
        Ok(())
    }

    pub(crate) fn release(&self, plugin_id: &str) {
        let mut map = self.inner.lock();
        match map.get_mut(plugin_id) {
            Some(count) => {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    map.remove(plugin_id);
                }
            }
            None => {
                debug_assert!(false, "release of a never-acquired slot for `{plugin_id}`")
            }
        }
    }
}

/// Accounting for open host handles (files, pipes, sockets, ...) that belong
/// to one plugin. Host code opens a handle through [`HandleTable::open`];
/// the slot is released automatically when the [`HostHandle`] is dropped.
///
/// The cap is `0` = unlimited. The table holds counters, not OS resources —
/// the lifetime of the actual resource belongs to whoever opened it; the
/// table bounds *how many* a plugin may hold at once.
#[derive(Debug)]
pub struct HandleTable {
    plugin_id: String,
    max: u32,
    inner: Mutex<HandleState>,
}

#[derive(Debug)]
struct HandleState {
    next: u64,
    open: HashMap<u64, ()>,
}

impl HandleTable {
    /// `max_open_handles` of `0` means unlimited. `plugin_id` is used in
    /// error messages when the cap rejects an open.
    pub fn new(max_open_handles: u32, plugin_id: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            max: max_open_handles,
            inner: Mutex::new(HandleState {
                next: 1,
                open: HashMap::new(),
            }),
        }
    }

    /// The configured cap (`0` = unlimited).
    pub fn max_handles(&self) -> u32 {
        self.max
    }

    /// How many handles are currently open.
    pub fn open_handles(&self) -> usize {
        self.inner.lock().open.len()
    }

    /// True when the cap is nonzero and fully used.
    pub fn is_at_capacity(&self) -> bool {
        self.max != 0 && self.open_handles() >= self.max as usize
    }

    /// Take a handle slot, or fail when the cap is already used up.
    pub fn open(self: &Arc<Self>) -> Result<HostHandle, PluginError> {
        let mut state = self.inner.lock();
        if self.max != 0 && state.open.len() >= self.max as usize {
            return Err(PluginError::LimitExceeded {
                plugin_id: self.plugin_id.clone(),
                kind: "max-open-handles",
                limit: u64::from(self.max),
                requested: u64::from(self.max) + 1,
            });
        }
        let id = state.next;
        state.next = state.next.wrapping_add(1);
        state.open.insert(id, ());
        Ok(HostHandle {
            id,
            table: Arc::clone(self),
        })
    }

    fn release(&self, id: u64) {
        self.inner.lock().open.remove(&id);
    }
}

/// RAII slot for one open host handle. Dropping it releases the slot.
#[derive(Debug)]
pub struct HostHandle {
    id: u64,
    table: Arc<HandleTable>,
}

impl HostHandle {
    /// Opaque identifier, unique within the table until wraparound (`u64`).
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.table.release(self.id);
    }
}

/// Per-invocation byte budget for plugin log output. The host creates one
/// fresh budget per plugin call ([`crate::PluginInstance::log_budget`]);
/// every host-side log line spends from it, and a line that does not fit is
/// dropped instead of letting a chatty plugin flood the proxy's logs.
#[derive(Debug)]
pub struct LogBudget {
    remaining: usize,
}

impl LogBudget {
    /// `max_bytes` from [`Limits::max_log_bytes`]; `0` silences the plugin.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            remaining: max_bytes,
        }
    }

    /// Bytes left in this invocation's budget.
    pub fn remaining(&self) -> usize {
        self.remaining
    }

    /// Spend `bytes`. Returns `false` when the line does not fit — the
    /// caller drops the line (and may log once, host-side).
    pub fn spend(&mut self, bytes: usize) -> bool {
        if bytes > self.remaining {
            return false;
        }
        self.remaining -= bytes;
        true
    }
}

/// Fixed-window call rate limiter, one per plugin instance.
///
/// The window starts at the first call after construction and resets after
/// [`CALL_WINDOW`] has elapsed. Denied calls do not consume quota.
/// `max_calls == 0` means unlimited.
#[derive(Debug)]
pub(crate) struct CallRateLimiter {
    max_calls: u32,
    state: Mutex<CallWindow>,
}

#[derive(Debug)]
struct CallWindow {
    window_start: Instant,
    count: u32,
}

impl CallRateLimiter {
    pub(crate) fn new(max_calls: u32) -> Self {
        Self {
            max_calls,
            state: Mutex::new(CallWindow {
                window_start: Instant::now(),
                count: 0,
            }),
        }
    }

    /// Try to make one call. Split out from the call path so tests can
    /// drive the window with synthetic timestamps.
    pub(crate) fn check(&self, now: Instant) -> bool {
        if self.max_calls == 0 {
            return true; // 0 = unlimited
        }
        let mut state = self.state.lock();
        if now.duration_since(state.window_start) >= CALL_WINDOW {
            state.window_start = now;
            state.count = 0;
        }
        if state.count >= self.max_calls {
            return false;
        }
        state.count += 1;
        true
    }
}

/// Static (parse-time) caps for one wasm module. Runs before compilation;
/// a module that is oversized never reaches extism.
///
/// Memory accounting sums the declared initial pages of every memory
/// (imports included, so a module importing a memory is accounted too). A
/// module's *declared maximum* may exceed the cap — growth is clamped at
/// run time by extism's memory limiter — but the initial size may not.
pub(crate) fn check_module(
    path: &Path,
    plugin_id: &str,
    wasm: &[u8],
    limits: &Limits,
) -> Result<(), PluginError> {
    let mut counts = StaticCount::default();

    let parser = Parser::new(0);
    for payload in parser.parse_all(wasm) {
        let payload = payload.map_err(|source| PluginError::WasmLoad {
            path: path.to_path_buf(),
            message: format!("wasm parse error: {source}"),
        })?;
        match payload {
            Payload::ImportSection(section) => {
                for imports in section {
                    let imports = imports.map_err(|source| PluginError::WasmLoad {
                        path: path.to_path_buf(),
                        message: format!("wasm import error: {source}"),
                    })?;
                    match imports {
                        wasmparser::Imports::Single(_, import) => {
                            counts.add_type(import.ty, 1);
                        }
                        wasmparser::Imports::Compact1 { items, .. } => {
                            for item in items {
                                let item = item.map_err(|source| PluginError::WasmLoad {
                                    path: path.to_path_buf(),
                                    message: format!("wasm compact import error: {source}"),
                                })?;
                                counts.add_type(item.ty, 1);
                            }
                        }
                        wasmparser::Imports::Compact2 { ty, names, .. } => {
                            counts.add_type(ty, names.count() as u64);
                        }
                    }
                }
            }
            Payload::FunctionSection(section) => {
                counts.declared_funcs += u64::from(section.count());
            }
            Payload::MemorySection(section) => {
                for memory in section {
                    let memory = memory.map_err(|source| PluginError::WasmLoad {
                        path: path.to_path_buf(),
                        message: format!("wasm memory error: {source}"),
                    })?;
                    counts.initial_memory_pages += memory.initial;
                }
            }
            Payload::TableSection(section) => {
                for table in section {
                    let table = table.map_err(|source| PluginError::WasmLoad {
                        path: path.to_path_buf(),
                        message: format!("wasm table error: {source}"),
                    })?;
                    counts.table_elements = counts.table_elements.max(table.ty.initial);
                    if let Some(max) = table.ty.maximum {
                        counts.table_elements = counts.table_elements.max(max);
                    }
                }
            }
            _ => {}
        }
    }

    let functions = counts.import_funcs + counts.declared_funcs;
    if limits.max_functions != 0 && functions > u64::from(limits.max_functions) {
        return Err(PluginError::LimitExceeded {
            plugin_id: plugin_id.to_owned(),
            kind: "max-functions",
            limit: u64::from(limits.max_functions),
            requested: functions,
        });
    }
    if limits.max_memory_pages != 0
        && counts.initial_memory_pages > u64::from(limits.max_memory_pages)
    {
        return Err(PluginError::LimitExceeded {
            plugin_id: plugin_id.to_owned(),
            kind: "max-memory-pages",
            limit: u64::from(limits.max_memory_pages),
            requested: counts.initial_memory_pages,
        });
    }
    if limits.max_table_elements != 0
        && counts.table_elements > u64::from(limits.max_table_elements)
    {
        return Err(PluginError::LimitExceeded {
            plugin_id: plugin_id.to_owned(),
            kind: "max-table-elements",
            limit: u64::from(limits.max_table_elements),
            requested: counts.table_elements,
        });
    }
    Ok(())
}

/// Accumulates the declared static sizes of one module for [`check_module`].
#[derive(Debug, Default)]
struct StaticCount {
    import_funcs: u64,
    declared_funcs: u64,
    initial_memory_pages: u64,
    table_elements: u64,
}

impl StaticCount {
    /// Account one import "shape" `count` times. Table checks are per
    /// table — the `max_table_elements` cap is not cumulative.
    fn add_type(&mut self, ty: TypeRef, count: u64) {
        match ty {
            TypeRef::Func(_) | TypeRef::FuncExact(_) => self.import_funcs += count,
            TypeRef::Memory(memory) => {
                self.initial_memory_pages += memory.initial.saturating_mul(count);
            }
            TypeRef::Table(table) => {
                self.table_elements = self.table_elements.max(table.initial);
                if let Some(max) = table.maximum {
                    self.table_elements = self.table_elements.max(max);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_limits_spec() {
        let limits = Limits::default();
        assert_eq!(limits.max_memory_pages, 64);
        assert_eq!(limits.max_table_elements, 16_384);
        assert_eq!(limits.max_instances, 1);
        assert_eq!(limits.max_functions, 65_536);
        assert_eq!(limits.max_fuel, 0);
        assert_eq!(limits.max_open_handles, 64);
        assert_eq!(limits.max_timeout, Duration::from_secs(30));
        assert_eq!(limits.max_output_bytes, 1_048_576);
        assert_eq!(limits.max_input_bytes, 16_777_216);
        assert_eq!(limits.max_log_bytes, 1_048_576);
        assert_eq!(limits.max_calls, 2_000);
    }

    #[test]
    fn rate_limiter_denies_after_the_window_quota_is_used() {
        let limiter = CallRateLimiter::new(2);
        let t0 = Instant::now();
        assert!(limiter.check(t0));
        assert!(limiter.check(t0));
        assert!(
            !limiter.check(t0),
            "third call in the same window must be denied"
        );
        // the next window opens after CALL_WINDOW has elapsed
        let t1 = t0 + CALL_WINDOW;
        assert!(limiter.check(t1));
    }

    #[test]
    fn rate_limiter_zero_is_unlimited() {
        let limiter = CallRateLimiter::new(0);
        let t0 = Instant::now();
        for _ in 0..10_000 {
            assert!(limiter.check(t0));
        }
    }

    #[test]
    fn rate_limiter_denied_calls_do_not_consume_quota() {
        let limiter = CallRateLimiter::new(1);
        let t0 = Instant::now();
        assert!(limiter.check(t0));
        assert!(!limiter.check(t0));
        assert!(!limiter.check(t0));
        let t1 = t0 + CALL_WINDOW;
        assert!(
            limiter.check(t1),
            "denied calls must not eat the next window"
        );
    }

    #[test]
    fn handle_table_caps_and_releases() {
        let table = Arc::new(HandleTable::new(2, "test-plugin"));
        let a = table.open().expect("first handle");
        let _b = table.open().expect("second handle");
        assert!(table.is_at_capacity());
        assert!(table.open().is_err(), "third handle must be rejected");
        drop(a);
        let _c = table.open().expect("a dropped handle frees a slot");
    }

    #[test]
    fn handle_table_zero_is_unlimited() {
        let table = Arc::new(HandleTable::new(0, "test-plugin"));
        let handles: Vec<_> = (0..100).map(|_| table.open().expect("unlimited")).collect();
        assert_eq!(table.open_handles(), 100);
        drop(handles);
        assert_eq!(table.open_handles(), 0);
    }

    #[test]
    fn log_budget_spends_and_rejects() {
        let mut budget = LogBudget::new(10);
        assert!(budget.spend(4));
        assert_eq!(budget.remaining(), 6);
        assert!(
            !budget.spend(7),
            "a line over the remainder must be dropped"
        );
        assert!(budget.spend(6));
        assert_eq!(budget.remaining(), 0);
        assert!(!budget.spend(1));
    }

    #[test]
    fn instance_slots_accounting() {
        let slots = InstanceSlots::new();
        assert_eq!(slots.live_count("a"), 0);
        slots.try_acquire("a", 1).expect("first slot");
        assert_eq!(slots.live_count("a"), 1);
        assert!(
            slots.try_acquire("a", 1).is_err(),
            "cap of 1 rejects the second"
        );
        slots.release("a");
        assert_eq!(slots.live_count("a"), 0);
        slots.try_acquire("a", 0).expect("0 = unlimited");
        slots.try_acquire("a", 0).expect("0 = unlimited, twice");
        assert_eq!(slots.live_count("a"), 2);
    }
}
