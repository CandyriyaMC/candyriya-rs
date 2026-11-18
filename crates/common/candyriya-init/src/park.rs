//! Cooperative park gates: how a unit is frozen.
//!
//! A gate is bound to a unit at spawn time and handed to the body's run
//! through a task-local, exactly like the caller pid ([`crate::current_unit`]).
//! The gate is a `watch` channel whose value is `true` while the unit is
//! parked. Flipping it happens through [`crate::Init::freeze`] /
//! [`crate::Init::unfreeze`]; only the tree holds the sending half, so a
//! body can observe the gate but can never toggle it.
//!
//! Semantics are deliberately cooperative: the gate does nothing to a unit
//! that never checks it. A conforming body stops at its next loop boundary —
//! it parks on [`ParkGate::wait_open`] before the next read, so nothing is
//! torn mid-frame, mid-cipher or mid-await; in-flight awaits complete. TCP
//! buffers fill on their own and the peer's writes stall: backpressure is
//! the mechanism, not a pause of the whole process. A parked unit is still
//! cancellable: the wrapper drops the body future at its parked await and
//! records `Cancelled`, exactly like any other requested stop.
//!
//! This is not SIGSTOP. There is no magic "stop the task wherever it is";
//! there is a gate the body chooses to honor, placed at the boundaries that
//! must not tear a session.

use tokio::sync::watch;

/// The park gate bound to one run of a unit body.
///
/// Handed out via [`park_gate`]. Cheap to clone (a receiver clone on the
/// same channel). The cloned gate tracks the same underlying state; it is
/// a view, not a second gate.
#[derive(Clone)]
pub struct ParkGate {
    rx: watch::Receiver<bool>,
}

impl ParkGate {
    pub(crate) fn new(rx: watch::Receiver<bool>) -> Self {
        Self { rx }
    }

    /// Whether the gate is closed (the unit is frozen).
    #[must_use]
    pub fn is_parked(&self) -> bool {
        *self.rx.borrow()
    }

    /// Wait until the gate is open, returning immediately when it already is.
    ///
    /// Suspends the caller until a parked gate is opened; a gate that stays
    /// closed parks the caller indefinitely, which is exactly what freeze
    /// means. If the sender is gone (the unit's node was reaped — the body
    /// run is over by then), the wait resumes rather than parks forever.
    ///
    /// The park consumes nothing: the caller holds no permit, no slot and no
    /// lock while waiting, and [`crate::Init::cancel`] still drops the
    /// waiting future at the unit's next parked await.
    pub async fn wait_open(&mut self) {
        // borrow_and_update marks the closed value seen; a plain borrow()
        // leaves it unseen and busy-loops this function
        while *self.rx.borrow_and_update() {
            if self.rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// Pend while the gate is open; resolve when it closes.
    ///
    /// Intended as a `select!` arm raced against a pending I/O future: a
    /// read that is already in flight when the unit is frozen must not
    /// consume the bytes that arrive after the flip, or freeze has no
    /// effect on an idle connection (the common case). While the gate is
    /// open this future pends forever; when the gate closes it resolves,
    /// and the caller re-parks at its next [`ParkGate::wait_open`].
    ///
    /// If the sender is gone (the unit's node was reaped — the body run
    /// is over by then), this pends forever instead of resolving.
    pub async fn wait_closed(&mut self) {
        loop {
            if *self.rx.borrow_and_update() {
                return;
            }
            if self.rx.changed().await.is_err() {
                // nothing will ever flip the gate again; pend forever so a
                // select arm does not spin on a dead channel
                std::future::pending::<()>().await;
            }
        }
    }
}

impl std::fmt::Debug for ParkGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkGate")
            .field("parked", &self.is_parked())
            .finish()
    }
}

tokio::task_local! {
    /// The gate bound to the unit run currently executing (if any).
    pub(crate) static PARK: ParkGate;
}

/// The park gate bound to the unit whose body is executing.
///
/// `None` outside unit bodies (tests, the composition root's non-unit
/// phases) and for code that is not running as one. Callers that want
/// freeze support should treat `None` as "ungated" and run normally.
#[must_use]
pub fn park_gate() -> Option<ParkGate> {
    PARK.try_with(ParkGate::clone).ok()
}
