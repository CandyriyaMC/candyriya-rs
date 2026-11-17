//! A wasm plugin instance living on its own dedicated OS thread.
//!
//! Extism plugins are synchronous and may run arbitrary wasm; they must
//! never execute on the proxy's async workers. Every plugin therefore gets a
//! dedicated, named OS thread (`candyriya-plugin-<id>`) which owns the
//! [`extism::Plugin`] and serves commands from a bounded mailbox
//! ([`std::sync::mpsc::sync_channel`]). Callers get a `Result`, never a
//! blocked thread: a full mailbox is [`PluginError::Busy`], a silent plugin
//! is [`PluginError::Timeout`], a dead one is [`PluginError::PluginGone`].
//!
//! # Sandbox limits
//!
//! Construction enforces the static caps on the module (memory, tables,
//! functions) before anything is compiled, acquires one slot of the
//! `max-instances` budget, and configures extism with the memory page cap,
//! the manifest timeout and, when [`Limits::max_fuel`] is non-zero, the
//! per-call fuel limit. The per-call input/output byte caps and the
//! per-window call rate limit ([`Limits::max_calls`]) are enforced on the
//! host-side call path; handles and per-invocation log bytes are accounted
//! through [`HandleTable`] and [`LogBudget`] — the host-function layer
//! consumes both.
//!
//! # Panics and drops
//!
//! The worker wraps every wasm call in `catch_unwind`, so a host-side panic
//! while a plugin runs is reported as a normal error instead of killing the
//! thread silently. Dropping a [`PluginInstance`] closes the mailbox; the
//! worker finishes its current command (if any) and terminates. A plugin
//! stuck in an infinite wasm loop is stopped by the fuel limit or the
//! manifest timeout, losing at most one in-flight call; no other plugin and
//! no caller are ever blocked by it, and [`PluginInstance::unload`]
//! documents that joining may wait on a stuck call.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;

use candyriya_plugin_api::OutcomeReport;

use crate::error::PluginError;
use crate::host_functions;
use crate::host_state::HostState;
use crate::limits::{
    CALL_WINDOW, CallRateLimiter, HandleTable, InstanceSlots, Limits, LogBudget, check_module,
};

/// Number of pending commands the mailbox holds before callers get `Busy`.
const MAILBOX_CAPACITY: usize = 16;

/// How long a caller waits for the control-plane commands (thread name,
/// export existence); plugin calls use the configured timeout instead.
const MAILBOX_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Commands the worker thread understands.
enum PluginCommand {
    /// Call an export with raw bytes. The extism ABI makes exports
    /// `() -> i32`; the input and output are raw byte handles.
    Call {
        /// Export name, validated by the plugin's manifest validation
        /// (`is_valid_export_name`) but checked for existence here.
        export: String,
        /// Raw input bytes.
        input: Vec<u8>,
        /// Where to send the result; the receiver disappears if the plugin
        /// died mid-call, which the caller maps to `PluginGone`.
        respond: SyncSender<Result<Vec<u8>, PluginError>>,
    },
    /// Does the module export `name`?
    HasExport {
        /// Export name to look up.
        name: String,
        /// Where to send the answer.
        respond: SyncSender<bool>,
    },
    /// Debug/test helper: the name of the worker thread.
    WorkerThreadName {
        /// Where to send the answer.
        respond: SyncSender<String>,
    },
    /// Ask the worker to terminate after finishing its current command.
    Shutdown,
}

/// A wasm plugin running on its dedicated thread.
///
/// Construct with [`PluginInstance::new`]. Constructing validates the module
/// against the sandbox [`Limits`], compiles and instantiates it on the
/// calling thread (so syntax/instantiation errors are reported
/// synchronously) and then moves it onto a dedicated, named
/// `candyriya-plugin-<id>` thread which serves all subsequent calls.
pub struct PluginInstance {
    plugin_id: String,
    command_tx: Option<SyncSender<PluginCommand>>,
    thread: Option<JoinHandle<()>>,
    data_dir: PathBuf,
    limits: Limits,
    slots: Option<Arc<InstanceSlots>>,
    rate_limiter: CallRateLimiter,
    handles: Arc<HandleTable>,
    subscribers: Arc<Mutex<std::collections::HashSet<String>>>,
    caller_plugin_id: Arc<Mutex<Option<String>>>,
    pending_outcome: Arc<Mutex<Option<OutcomeReport>>>,
}

impl PluginInstance {
    /// Compile and instantiate `wasm_path` under the given sandbox limits
    /// and spawn its dedicated thread.
    ///
    /// The module bytes are read and passed to extism inline; the extism
    /// plugin is created with the `candyriya:*` host functions registered
    /// (see `host_functions`) and WASI disabled. The module is first
    /// checked against the static [`Limits`] caps (memory, tables,
    /// functions) — a module over the caps is rejected before any
    /// compilation happens. The manifest carries the configured memory page
    /// cap and, when set, the call timeout and per-call fuel limit.
    ///
    /// Compilation happens on the calling thread; on success the plugin
    /// moves onto a freshly spawned `candyriya-plugin-<plugin_id>` thread
    /// which serves all subsequent calls.
    ///
    /// `plugin_id` must be a validated plugin id (the loader guarantees this
    /// for manifest-derived ids; tests pass their own ids). It is used for
    /// the thread name and error messages only.
    ///
    /// # Errors
    ///
    /// [`PluginError::LimitExceeded`] when the module breaks a sandbox cap,
    /// [`PluginError::WasmLoad`] when the file cannot be read or is not a
    /// valid wasm module extism accepts, [`PluginError::PluginThread`] when
    /// the worker cannot be spawned.
    pub fn new(
        wasm_path: &Path,
        host_state: HostState,
        plugin_id: &str,
        limits: &Limits,
    ) -> Result<Self, PluginError> {
        if !wasm_path.is_file() {
            return Err(PluginError::Io {
                path: wasm_path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "wasm module not found"),
            });
        }
        let wasm_bytes = std::fs::read(wasm_path).map_err(|e| PluginError::Io {
            path: wasm_path.to_path_buf(),
            source: e,
        })?;

        // parse-time caps: an oversized module never reaches the compiler
        check_module(wasm_path, plugin_id, &wasm_bytes, limits)?;

        // one live-instance slot per plugin, shared across the scope the
        // host state belongs to (the loader shares one registry per run)
        let slots = host_state.instance_slots();
        slots.try_acquire(plugin_id, limits.max_instances)?;

        match Self::spawn(
            wasm_path,
            wasm_bytes,
            host_state,
            plugin_id,
            limits,
            Arc::clone(&slots),
        ) {
            Ok(instance) => {
                crate::metrics::plugin_loaded(plugin_id);
                Ok(instance)
            }
            Err(error) => {
                slots.release(plugin_id);
                Err(error)
            }
        }
    }

    fn spawn(
        wasm_path: &Path,
        wasm_bytes: Vec<u8>,
        host_state: HostState,
        plugin_id: &str,
        limits: &Limits,
        slots: Arc<InstanceSlots>,
    ) -> Result<Self, PluginError> {
        let manifest = extism::Manifest::new([extism::Wasm::data(wasm_bytes)])
            .with_memory_max(limits.max_memory_pages)
            .with_timeout(limits.max_timeout);

        // 0 = unlimited: no fuel metering at all
        let builder = extism::PluginBuilder::new(&manifest);
        let builder = match limits.fuel_per_call() {
            Some(fuel) => builder.with_fuel_limit(fuel),
            None => builder,
        };
        // host functions close over their own clones of the shared state;
        // the worker keeps the original one for caller identity bookkeeping
        let builder = host_functions::register_host_functions(builder, host_state.clone());
        let plugin = builder.build().map_err(|e| PluginError::WasmLoad {
            path: wasm_path.to_path_buf(),
            message: format!("{e:#}"),
        })?;

        let subscribers = Arc::clone(&host_state.subscribers);
        let caller_plugin_id = Arc::clone(&host_state.caller_plugin_id);
        let pending_outcome = Arc::clone(&host_state.pending_outcome);

        let (command_tx, command_rx) = sync_channel::<PluginCommand>(MAILBOX_CAPACITY);
        let thread_name = format!("candyriya-plugin-{plugin_id}");
        let data_dir = host_state.data_dir.clone();
        let plugin_id_owned = plugin_id.to_owned();
        let worker_plugin_id = plugin_id.to_owned();
        // move the host state in too: the worker needs the caller identity
        // bookkeeping; the host functions hold their own clones inside the plugin
        let worker = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || worker_loop(plugin, host_state, command_rx, worker_plugin_id))
            .map_err(|e| PluginError::PluginThread {
                plugin_id: plugin_id_owned,
                reason: e.to_string(),
            })?;

        Ok(Self {
            plugin_id: plugin_id.to_owned(),
            command_tx: Some(command_tx),
            thread: Some(worker),
            data_dir,
            limits: limits.clone(),
            slots: Some(slots),
            rate_limiter: CallRateLimiter::new(limits.max_calls),
            handles: Arc::new(HandleTable::new(
                limits.max_open_handles,
                plugin_id.to_owned(),
            )),
            subscribers,
            caller_plugin_id,
            pending_outcome,
        })
    }

    /// The plugin id this instance was created with.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// The sandbox limits this instance was created under.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The plugin's data directory (owned by this plugin alone).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The host handle accounting for this plugin. Host functions use it to
    /// bound what they open on the plugin's behalf; dropping the returned
    /// [`HandleTable`] does not reset the counters — cloned tables share
    /// them.
    pub fn handle_table(&self) -> Arc<HandleTable> {
        Arc::clone(&self.handles)
    }

    /// The event kinds this plugin opted into via the
    /// `candyriya:subscribeToEvents` host function. Shared with the host
    /// functions, which replace it wholesale on subscribe.
    pub fn subscribers(&self) -> Arc<Mutex<std::collections::HashSet<String>>> {
        Arc::clone(&self.subscribers)
    }

    /// Whether `kind` should be dispatched to this plugin.
    ///
    /// An empty allowlist means every kind — until a plugin subscribes, it
    /// receives the whole stream.
    pub fn subscribes_to(&self, kind: &str) -> bool {
        let subscribers = self.subscribers.lock();
        subscribers.is_empty() || subscribers.contains(kind)
    }

    /// Set the caller identity for the export about to run and return the
    /// previous value. The host restores the previous value after the
    /// call; `None` means the proxy itself is the caller.
    pub fn set_caller_plugin_id(&self, caller: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.caller_plugin_id.lock(), caller)
    }

    /// The caller identity for the call currently in flight, if any.
    pub fn caller_plugin_id(&self) -> Option<String> {
        self.caller_plugin_id.lock().clone()
    }

    /// Take (and clear) the cancellation the plugin recorded through the
    /// `candyriya:cancelEvent` host function for the call in flight.
    pub fn take_pending_outcome(&self) -> Option<OutcomeReport> {
        self.pending_outcome.lock().take()
    }

    /// A fresh log byte budget for one invocation, capped by
    /// [`Limits::max_log_bytes`]. Callers that emit plugin log lines create
    /// one per plugin call and spend from it; when it runs out, lines are
    /// dropped (the host may log that once, host-side).
    pub fn log_budget(&self) -> LogBudget {
        LogBudget::new(self.limits.max_log_bytes)
    }

    /// The name of the dedicated thread this plugin runs on.
    pub fn worker_thread_name(&self) -> Result<String, PluginError> {
        let (tx, rx) = sync_channel(1);
        self.send(PluginCommand::WorkerThreadName { respond: tx })?;
        rx.recv_timeout(MAILBOX_REPLY_TIMEOUT)
            .map_err(|_| self.gone())
    }

    /// Does the wasm module export `name`?
    ///
    /// Used by the loader to enforce the manifest's `plugin_main` before a
    /// plugin is marked `Loaded`.
    pub fn function_exists(&self, name: &str) -> Result<bool, PluginError> {
        let (tx, rx) = sync_channel(1);
        self.send(PluginCommand::HasExport {
            name: name.to_owned(),
            respond: tx,
        })?;
        rx.recv_timeout(MAILBOX_REPLY_TIMEOUT)
            .map_err(|_| self.gone())
    }

    /// Call an export with raw bytes.
    ///
    /// The extism ABI passes input and output as bytes; typed
    /// (de)serialization is the caller's job (JSON via extism-convert on the
    /// guest side). Waits at most `max_timeout` from the instance's
    /// [`Limits`] and never blocks the calling thread beyond that.
    ///
    /// `candyriya_plugin_calls_total` is recorded exactly once per call, at
    /// the call site: this method maps the result mechanically (`ok`,
    /// `timeout`, `failed`, `fuel_exhausted`). The event dispatcher uses
    /// [`PluginInstance::call_with_timeout`] directly and records its own,
    /// cancellation-aware outcome instead.
    pub fn call(&self, export: &str, input: &[u8]) -> Result<Vec<u8>, PluginError> {
        let result = self.call_with_timeout(export, input, self.limits.max_timeout);
        crate::metrics::record_result(&self.plugin_id, &result);
        result
    }

    /// Call an export with a caller-chosen timeout.
    ///
    /// Same semantics as [`PluginInstance::call`] but with an explicit
    /// deadline. The input is checked against `max-input-bytes` and the
    /// per-window `max-calls` quota before anything touches the plugin
    /// thread; the output is checked against `max-output-bytes` before it is
    /// handed back. The event dispatcher uses this to enforce its own
    /// per-plugin timeout instead of the configured one. Exceeding the
    /// deadline is reported as [`PluginError::Timeout`]; the plugin keeps
    /// whatever state it had (the call is not cancelled, it just stops
    /// being awaited).
    ///
    /// Metrics: this is the instrumented boundary — the duration histogram,
    /// the active-call gauge and (on the plugin thread) the wasm thread
    /// time are recorded here. `candyriya_plugin_calls_total` is NOT
    /// recorded here; the caller that owns the outcome label records it
    /// (see [`crate::metrics`]).
    pub fn call_with_timeout(
        &self,
        export: &str,
        input: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, PluginError> {
        // byte caps first, before anything is copied into the mailbox
        if input.len() > self.limits.max_input_bytes {
            return Err(PluginError::LimitExceeded {
                plugin_id: self.plugin_id.clone(),
                kind: "max-input-bytes",
                limit: self.limits.max_input_bytes as u64,
                requested: input.len() as u64,
            });
        }
        if !self.rate_limiter.check(std::time::Instant::now()) {
            return Err(PluginError::RateLimited {
                plugin_id: self.plugin_id.clone(),
                max_calls: self.limits.max_calls,
                window_secs: CALL_WINDOW.as_secs(),
            });
        }

        let started = std::time::Instant::now();
        // decrements the active-call gauge on drop, whatever happens below
        let _active = crate::metrics::active_call(&self.plugin_id);
        let (tx, rx) = sync_channel(1);
        if let Err(error) = self.send(PluginCommand::Call {
            export: export.to_owned(),
            input: input.to_vec(),
            respond: tx,
        }) {
            // a full mailbox already bumped busy_total inside send(); any
            // other failure (dead mailbox) is the call sites' business, so
            // they can label it properly
            return Err(error);
        }
        let result = match rx.recv_timeout(timeout) {
            Ok(Ok(bytes)) => {
                if bytes.len() > self.limits.max_output_bytes {
                    Err(PluginError::LimitExceeded {
                        plugin_id: self.plugin_id.clone(),
                        kind: "max-output-bytes",
                        limit: self.limits.max_output_bytes as u64,
                        requested: bytes.len() as u64,
                    })
                } else {
                    Ok(bytes)
                }
            }
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Err(PluginError::Timeout {
                plugin_id: self.plugin_id.clone(),
                timeout_ms: timeout.as_millis() as u64,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(self.gone()),
        };
        // only calls that actually reached the plugin get a duration sample
        crate::metrics::observe_call_duration(&self.plugin_id, started.elapsed());
        result
    }

    /// Ask the worker to shut down and wait for it to terminate.
    ///
    /// Closing the mailbox drains any commands already queued; the worker
    /// exits once it sees the closed channel (or a pending `Shutdown`). Joining
    /// waits for the plugin's current call (if any) to return; a caller that
    /// cannot wait that long should simply drop the instance instead of
    /// calling this. Dropping never blocks the caller.
    pub fn unload(mut self) -> Result<(), PluginError> {
        close_mailbox(self.command_tx.take());
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread.join().map_err(|_| PluginError::PluginThread {
            plugin_id: self.plugin_id.clone(),
            reason: "worker thread panicked".to_owned(),
        })
    }

    fn send(&self, command: PluginCommand) -> Result<(), PluginError> {
        let tx = self.command_tx.as_ref().ok_or_else(|| self.gone())?;
        tx.try_send(command).map_err(|e| match e {
            TrySendError::Full(_) => {
                crate::metrics::record_busy(&self.plugin_id);
                PluginError::Busy {
                    plugin_id: self.plugin_id.clone(),
                }
            }
            TrySendError::Disconnected(_) => self.gone(),
        })
    }

    fn gone(&self) -> PluginError {
        PluginError::PluginGone {
            plugin_id: self.plugin_id.clone(),
        }
    }
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        // release the live-instance slot for this plugin (the registry this
        // state was created against — the loader's shared one)
        if let Some(slots) = self.slots.take() {
            slots.release(&self.plugin_id);
        }
        crate::metrics::plugin_unloaded(&self.plugin_id);
        // Close the mailbox so the worker terminates after its current
        // command. The join is deliberately *not* done here: dropping must
        // never wait on a stuck wasm call. The worker thread is detached
        // and dies with the process at the latest.
        close_mailbox(self.command_tx.take());
        let _ = self.thread.take();
    }
}

/// Close a command channel without ever blocking the caller.
///
/// If a plugin is mid-call the `Shutdown` may not fit the bounded mailbox;
/// dropping the sender is the real shutdown signal — the worker finishes its
/// current command and then exits when `recv` reports the channel closed.
fn close_mailbox(tx: Option<SyncSender<PluginCommand>>) {
    if let Some(tx) = tx {
        let _ = tx.try_send(PluginCommand::Shutdown);
        drop(tx);
    }
}

fn worker_loop(
    mut plugin: extism::Plugin,
    _host_state: HostState,
    command_rx: std::sync::mpsc::Receiver<PluginCommand>,
    plugin_id: String,
) {
    while let Ok(command) = command_rx.recv() {
        match command {
            PluginCommand::Call {
                export,
                input,
                respond,
            } => {
                let wasm_started = std::time::Instant::now();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    plugin
                        .call::<&[u8], Vec<u8>>(&export, &input)
                        .map_err(|e| e.to_string())
                }));
                // wasm thread time, not caller-observed wall time: a hung
                // run only lands here once the runtime interrupts it
                crate::metrics::record_thread_time(&plugin_id, wasm_started.elapsed());
                let result = match result {
                    Ok(Ok(bytes)) => Ok(bytes),
                    Ok(Err(message)) => {
                        // extism surfaces a drained fuel store as an error
                        // whose message contains this string (via
                        // catch_out_of_fuel! and raw_call); the epoch timer
                        // surfaces timeouts the same way with "timeout"
                        if message.contains("ran out of fuel") {
                            Err(PluginError::FuelExhausted {
                                plugin_id: plugin_id.clone(),
                                export,
                            })
                        } else {
                            Err(PluginError::CallFailed {
                                plugin_id: plugin_id.clone(),
                                export,
                                message,
                            })
                        }
                    }
                    Err(_) => Err(PluginError::CallFailed {
                        plugin_id: plugin_id.clone(),
                        export,
                        message: "plugin panicked during the call".to_owned(),
                    }),
                };
                // if the caller gave up waiting, nobody reads this — fine
                let _ = respond.send(result);
            }
            PluginCommand::HasExport { name, respond } => {
                let _ = respond.send(plugin.function_exists(&name));
            }
            PluginCommand::WorkerThreadName { respond } => {
                let name = std::thread::current()
                    .name()
                    .unwrap_or("unnamed")
                    .to_owned();
                let _ = respond.send(name);
            }
            PluginCommand::Shutdown => break,
        }
    }
    // `plugin` drops here, releasing the wasm instance and its memory
}
