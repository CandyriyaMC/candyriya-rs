//! Backend resolution for the proxy: who serves what, and in which order.
//!
//! Three pieces do the work:
//!
//! * [`BackendRegistry`] — the set of backends (name → address), the ordered
//!   *try list* for the login fallback chain, and health bookkeeping;
//! * [`ForcedHosts`] — the `host → backends` table (`[forced-hosts]` in the
//!   configuration) with single-label wildcard matching, documented on the
//!   type and in the module docs;
//! * a real Minecraft status ping (private `ping` module) used by
//!   [`BackendRegistry::health_check`] to tell online from offline backends.
//!
//! What this crate does not do: DNS resolution (addresses arrive as
//! [`std::net::SocketAddr`], the configuration layer resolves them), task
//! scheduling (the health checker never spawns tasks; the caller invokes it,
//! e.g. from the scheduler), and packet forwarding (that is the proxy's
//! pipeline).
//!
//! # The `route` unit
//!
//! The [`unit`] module carries the supervision-tree contract for this
//! crate: the `RouteMsg`/`RouteResp` protocol, the `route` and
//! `route.health` unit specs ([`route_spec`]/[`health_spec`]), and the
//! shared store type the composition root and the reload path use. See the
//! module docs there for the reply semantics and the capability model.
//!
//! ```text
//! client -> handshake -> Login Start
//!     forced host match? -> those backends, in order
//!     else               -> the try list, in order
//!     kick from backend  -> next untried backend
//!     chain exhausted    -> disconnect the player
//! ```

#![forbid(unsafe_code)]

mod backend;
mod error;
mod forced;
mod ping;
mod registry;
mod unit;

pub use backend::{Backend, BackendState};
pub use error::RouteError;
pub use forced::{ForcedHost, ForcedHosts};
pub use ping::MOTD_PREVIEW_BYTES;
pub use registry::{BackendRegistry, DEFAULT_HEALTH_CHECK_TIMEOUT, HealthCheckOutcome};
pub use unit::{
    BackendRef, BackendSnapshot, DEFAULT_HEALTH_CHECK_INTERVAL, RouteMsg, RouteResp, RouterStore,
    health_spec, route_call, route_spec, router_store,
};
