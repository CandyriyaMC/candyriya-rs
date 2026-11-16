//! # candyriya-auth
//!
//! Server-side authentication for the proxy's login flow.
//!
//! A proxy is a **server** as far as Mojang is concerned: during the login
//! sequence it verifies that a client which has already signed in with its
//! own Microsoft account actually owns the Mojang session it claims. It does
//! so with the read-only `hasJoined` call to the session server — nothing
//! stateful is mutated, no user credentials ever pass through the proxy, and
//! no Microsoft account machinery lives in this crate.
//!
//! Three moving parts:
//!
//! * [`SessionValidator`] — the contract the proxy's login flow depends on.
//! * [`OfflineSessionValidator`] — offline mode: the UUID is derived from
//!   the name (see [`candyriya_player::offline_uuid`]), no network involved.
//! * [`MojangSessionValidator`] — online mode: `GET` against the session
//!   server ([`DEFAULT_SESSION_SERVER_URL`] by default) and a strict
//!   interpretation of the response.
//!
//! [`AuthMode`] / [`AuthConfig`] capture the configured mode and endpoint;
//! [`validate_username`] implements the shared Java Edition name rule.
//!
//! # Example
//!
//! ```
//! use candyriya_auth::{OfflineSessionValidator, SessionValidator};
//!
//! let runtime = tokio::runtime::Builder::new_current_thread()
//!     .enable_all()
//!     .build()
//!     .expect("runtime");
//! runtime.block_on(async {
//!     let validator = OfflineSessionValidator;
//!     let profile = validator
//!         .verify_client("Notch", "")
//!         .await
//!         .expect("valid name");
//!     assert!(profile.properties.is_empty());
//! });
//! ```
//!
//! This crate performs no network I/O of its own beyond the session server
//! call in [`MojangSessionValidator`], contains no unsafe code, and holds no
//! secrets.
#![forbid(unsafe_code)]

mod config;
mod error;
mod username;
mod validator;

pub use config::{AuthConfig, AuthMode, DEFAULT_SESSION_SERVER_URL, DEFAULT_TIMEOUT};
pub use error::AuthError;
pub use username::{MAX_USERNAME_LEN, MIN_USERNAME_LEN, validate_username};
pub use validator::{MojangSessionValidator, OfflineSessionValidator, SessionValidator};
