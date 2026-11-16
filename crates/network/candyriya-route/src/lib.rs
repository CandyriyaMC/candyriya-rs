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

pub use backend::{Backend, BackendState};
pub use error::RouteError;
pub use forced::{ForcedHost, ForcedHosts};
pub use ping::MOTD_PREVIEW_BYTES;
pub use registry::{BackendRegistry, DEFAULT_HEALTH_CHECK_TIMEOUT, HealthCheckOutcome};
