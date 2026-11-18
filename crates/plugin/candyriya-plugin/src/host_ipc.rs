//! The late-bound link from plugin host functions to the supervised `host` unit.
//!
//! Plugin instances are built during the plugin load, before the
//! composition root has constructed the shared proxy state — and the
//! `host` unit needs that state at spawn time. The link therefore cannot
//! be handed to plugin instances eagerly; instead every instance shares
//! one [`HostIpc`] slot, the composition root fills it once the unit
//! exists, and host functions read it per call.
//!
//! The call path: a host function builds its message, [`HostIpc::call`]
//! parks the plugin thread on a `block_on` and sends the message through
//! the tree attributed to [`Pid::INIT`] — plugin threads are not tree
//! units, and the root is an ancestor of everything, so the subtree
//! rule authorizes the call and no per-instance grant is needed.
//! `block_on` is safe here because plugin threads never enter the
//! runtime; the thread parks until the unit answers or the call deadline
//! fires.
//!
//! # Failure vocabulary
//!
//! Two honest answers, never a panic and never a fabricated success:
//! the link was never installed ([`HostIpcError::NotLinked`]), or the
//! tree refused the call ([`HostIpcError::Call`] carrying the tree's
//! [`CallError`]). Callers decide their own fallback; the message is
//! always logged by them with the plugin id, without payloads.

use std::sync::{Arc, OnceLock};

use thiserror::Error;

use candyriya_init::{CallError, Init, Pid};

/// The shared slot every plugin instance carries to the `host` unit.
///
/// Cheap to clone (an `Arc` plus a one-shot cell). Created empty before
/// the plugin load; the composition root installs the handles once the
/// unit is spawned. The cell is one-shot on purpose: a re-installation
/// would silently re-point every host function, and the process has
/// exactly one `host` unit per lifetime of the tree.
#[derive(Clone, Default)]
pub struct HostIpc {
    slot: Arc<OnceLock<HostIpcInner>>,
}

/// The handles the host functions need, installed together and read
/// atomically after that.
struct HostIpcInner {
    /// The tree the call goes through.
    init: Init,
    /// The `host` unit's pid.
    host_pid: Pid,
    /// The runtime whose reactor drives the call's timeout and wakeup.
    runtime: tokio::runtime::Handle,
}

impl HostIpc {
    /// An empty slot; host functions called before installation answer
    /// with [`HostIpcError::NotLinked`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the link. Returns `false` when it was already installed;
    /// the first installation wins.
    #[must_use]
    pub fn install(&self, init: Init, host_pid: Pid, runtime: tokio::runtime::Handle) -> bool {
        self.slot
            .set(HostIpcInner {
                init,
                host_pid,
                runtime,
            })
            .is_ok()
    }

    /// Whether the link has been installed. Diagnostics and tests only.
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.slot.get().is_some()
    }

    /// One round trip to the `host` unit.
    ///
    /// Attributed to [`Pid::INIT`]: the calling thread is a plugin
    /// thread, not a tree unit, and the root is an ancestor of every
    /// unit. Blocks the calling thread until the unit answers or the
    /// call deadline fires; callers on async threads must not use it.
    ///
    /// # Errors
    ///
    /// [`HostIpcError::NotLinked`] when the link was never installed, or
    /// [`HostIpcError::Call`] carrying the tree's [`CallError`] (timeout,
    /// busy mailbox, dead unit, ...).
    pub fn call<Req, Resp>(&self, msg: Req) -> Result<Resp, HostIpcError>
    where
        Req: Send + 'static,
        Resp: Send + 'static,
    {
        let Some(inner) = self.slot.get() else {
            return Err(HostIpcError::NotLinked);
        };
        inner
            .runtime
            .block_on(inner.init.call(Pid::INIT, inner.host_pid, msg))
            .map_err(HostIpcError::Call)
    }
}

impl std::fmt::Debug for HostIpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostIpc")
            .field("linked", &self.is_linked())
            .finish()
    }
}

/// Why a host function could not reach the `host` unit.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HostIpcError {
    /// The composition root never installed the link (a standalone host
    /// state, a unit test, or an aborted boot).
    #[error("host unit link is not installed")]
    NotLinked,

    /// The tree refused the call; the payload is the tree's own error.
    #[error("{0}")]
    Call(#[from] CallError),
}
