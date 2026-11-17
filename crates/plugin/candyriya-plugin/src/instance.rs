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
//! This module implements loading, calling and unloading only. Host
//! functions and resource limits (fuel, memory, WASI) are deliberately out of
//! scope here; they land with the task layer.
//!
//! # Panics and drops
//!
//! The worker wraps every wasm call in `catch_unwind`, so a host-side panic
//! while a plugin runs is reported as a normal error instead of killing the
//! thread silently. Dropping a [`PluginInstance`] closes the mailbox; the
//! worker finishes its current command (if any) and terminates. A plugin
//! stuck in an infinite wasm loop keeps its thread — fuel/epoch limits are
//! tracked separately — but no other plugin and no caller are ever blocked
//! by it, and [`PluginInstance::unload`] documents that joining may wait on
//! the stuck call.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::PluginError;
use crate::host_state::HostState;

/// Number of pending commands the mailbox holds before callers get `Busy`.
const MAILBOX_CAPACITY: usize = 16;

/// How long a caller waits for the plugin to answer a command.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

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
/// Construct with [`PluginInstance::new`]. Constructing compiles and
/// instantiates the module on the calling thread (so syntax/instantiation
/// errors are reported synchronously) and then moves it onto a dedicated,
/// named `candyriya-plugin-<id>` thread.
pub struct PluginInstance {
    plugin_id: String,
    command_tx: Option<SyncSender<PluginCommand>>,
    thread: Option<JoinHandle<()>>,
    data_dir: PathBuf,
}

impl PluginInstance {
    /// Compile and instantiate `wasm_path` and spawn its dedicated thread.
    ///
    /// The module bytes are read and passed to extism inline; the extism
    /// plugin is created with no host functions and WASI disabled.
    /// Compilation happens on the calling thread; on success the
    /// plugin moves onto a freshly spawned `candyriya-plugin-<plugin_id>`
    /// thread which serves all subsequent calls.
    ///
    /// `plugin_id` must be a validated plugin id (the loader guarantees this
    /// for manifest-derived ids; tests pass their own ids). It is used for
    /// the thread name and error messages only.
    ///
    /// # Errors
    ///
    /// [`PluginError::WasmLoad`] when the file cannot be read or is not a
    /// valid wasm module extism accepts, [`PluginError::PluginThread`] when
    /// the worker cannot be spawned.
    pub fn new(
        wasm_path: &Path,
        host_state: HostState,
        plugin_id: &str,
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

        let manifest = extism::Manifest::new([extism::Wasm::data(wasm_bytes)]);
        let plugin =
            extism::Plugin::new(&manifest, [], false).map_err(|e| PluginError::WasmLoad {
                path: wasm_path.to_path_buf(),
                message: format!("{e:#}"),
            })?;

        let (command_tx, command_rx) = sync_channel::<PluginCommand>(MAILBOX_CAPACITY);
        let thread_name = format!("candyriya-plugin-{plugin_id}");
        let data_dir = host_state.data_dir.clone();
        let plugin_id_owned = plugin_id.to_owned();
        let worker_plugin_id = plugin_id.to_owned();
        // move the host state in too; host functions will close over it later
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
        })
    }

    /// The plugin id this instance was created with.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// The plugin's data directory (owned by this plugin alone).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The name of the dedicated thread this plugin runs on.
    pub fn worker_thread_name(&self) -> Result<String, PluginError> {
        let (tx, rx) = sync_channel(1);
        self.send(PluginCommand::WorkerThreadName { respond: tx })?;
        rx.recv_timeout(CALL_TIMEOUT).map_err(|_| self.gone())
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
        rx.recv_timeout(CALL_TIMEOUT).map_err(|_| self.gone())
    }

    /// Call an export with raw bytes.
    ///
    /// The extism ABI passes input and output as bytes; typed
    /// (de)serialization is the caller's job (JSON via extism-convert on the
    /// guest side). This waits at most five seconds and never blocks the
    /// calling thread beyond that.
    pub fn call(&self, export: &str, input: &[u8]) -> Result<Vec<u8>, PluginError> {
        let (tx, rx) = sync_channel(1);
        self.send(PluginCommand::Call {
            export: export.to_owned(),
            input: input.to_vec(),
            respond: tx,
        })?;
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(PluginError::Timeout {
                plugin_id: self.plugin_id.clone(),
                timeout_ms: CALL_TIMEOUT.as_millis() as u64,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(self.gone()),
        }
    }

    /// Ask the worker to shut down and wait for it to terminate.
    ///
    /// Closing the mailbox drains any commands already queued; the worker
    /// exits once it sees the closed channel (or a pending `Shutdown`). Joining
    /// waits for the plugin's current call (if any) to return — a plugin
    /// stuck in an infinite wasm loop keeps this call pending; fuel limits
    /// are tracked separately. Closing never blocks the caller.
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
            TrySendError::Full(_) => PluginError::Busy {
                plugin_id: self.plugin_id.clone(),
            },
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
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    plugin
                        .call::<&[u8], Vec<u8>>(&export, &input)
                        .map_err(|e| e.to_string())
                }));
                let result = match result {
                    Ok(Ok(bytes)) => Ok(bytes),
                    Ok(Err(message)) => Err(PluginError::CallFailed {
                        plugin_id: plugin_id.clone(),
                        export,
                        message,
                    }),
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
