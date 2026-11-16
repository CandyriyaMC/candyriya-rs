//! Token-bucket rate limiting and connection limiting for inbound traffic.
//!
//! This crate is a thin set of DDoS-resilience primitives for the proxy's
//! inbound path:
//!
//! - [`TokenBucket`] — a classic token bucket with lazy refill, meant for
//!   handshake/login rate limiting per IP. [`bursts_per_second`] builds a
//!   bucket whose burst and refill rate are the same value.
//! - [`RateLimiter`] — a fixed-window counter per key (typically an IP
//!   string), reporting how long to wait before retrying when denied.
//! - [`ConnectionLimiter`] — a global (and optionally per-IP) concurrent
//!   connection cap with an RAII [`ConnectionGuard`] that releases its
//!   slot on drop.
//!
//! # Time injection
//!
//! The crate never reads a clock behind your back in its *logic*. Each
//! primitive is split in two layers:
//!
//! 1. A small, pure state machine ([`BucketState`], [`RateLimiterState`])
//!    that takes `now: std::time::Instant` as an explicit argument.
//! 2. A thin wrapper ([`TokenBucket`], [`RateLimiter`]) that supplies
//!    [`Instant::now()`](std::time::Instant::now) and adds whatever
//!    synchronization the API needs.
//!
//! The proxy calls the wrappers (`limiter.check(ip)`) without threading a
//! clock through every call site, while tests exercise the pure cores with
//! artificial instants — no sleeps, no flaky timing assertions. Documented
//! rationale: the wall clock is an input to the algorithm, not a hidden
//! dependency of it.
//!
//! # Safety and panicking
//!
//! `#![forbid(unsafe_code)]` is set crate-wide. No API in this crate
//! panics on any input; invalid numeric inputs are clamped or rejected
//! according to the rules documented on each constructor. Errors are
//! reported through return values (see [`AtCapacity`]).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bucket;
mod connection;
mod ratelimit;

pub use bucket::{BucketState, TokenBucket, bursts_per_second};
pub use connection::{AtCapacity, ConnectionGuard, ConnectionLimiter};
pub use ratelimit::{Allow, RateLimiter, RateLimiterState};
