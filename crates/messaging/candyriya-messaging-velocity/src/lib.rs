//! Velocity-style plugin messaging semantics for the candyriya proxy.
//!
//! This crate models the plugin-message machinery of a modern Minecraft
//! proxy — the parts that decide **whether** and **where** a plugin message
//! goes once it reaches the proxy. It is **not** a wire codec and not a
//! proxy implementation; it contains no networking, no async runtime, no
//! tokio and no unsafe. The byte-level format lives in
//! [`candyriya_messaging`], the BungeeCord subchannel
//! codec lives in
//! [`candyriya_messaging_bungeecord`], and
//! the config plumbing that enables or disables the compat set lives in the
//! proxy crates.
//!
//! # The moving pieces
//!
//! - [`ChannelRegistrar`] — the set of channels the proxy listens on.
//!   Registration is the permission to intercept: the proxy fires
//!   [`PluginMessageEvent`] only for registered channels.
//! - [`PluginMessageEvent`] — the one and only event kind plugin messages
//!   produce at the proxy, carrying the [`ChannelIdentifier`](candyriya_messaging::ChannelIdentifier), the
//!   [`MessageSource`] and the opaque payload.
//! - [`ForwardResult`] — the verdict handlers leave on the event: consume
//!   the message at the proxy ([`ForwardResult::Handled`]) or let it travel
//!   on as if the proxy were not present ([`ForwardResult::Forward`]).
//! - [`BungeeChannelSet`] — the built-in BungeeCord compatibility set
//!   (same two channels, same 18 subchannels), re-exported from
//!   [`candyriya_messaging_bungeecord`] rather than reimplemented.
//!
//! # The four cases
//!
//! Velocity manages connections **both** ways — towards the client and
//! towards the backend servers — which yields exactly four situations a
//! plugin must consider. They are mirrored here with the same names
//! (`direction x endpoint`) and the same security posture as the upstream
//! documentation.
//!
//! ## Case 1: A plugin message arrives from a player (client → proxy)
//!
//! A client sends a plugin message on a channel. If the channel is
//! **registered** with the [`ChannelRegistrar`], the proxy fires
//! [`PluginMessageEvent`] with [`MessageSource::Player`]; if not, the packet
//! bypasses the event machinery untouched.
//!
//! Example use case: a client-side mod reporting its enabled features to
//! the proxy or to the backend behind it.
//!
//! ```
//! use candyriya_messaging::ChannelIdentifier;
//! use candyriya_messaging_velocity::{MessageSource, PluginMessageEvent};
//! use bytes::Bytes;
//!
//! let mut event = PluginMessageEvent::new(
//!     ChannelIdentifier::new("custom:main")?,
//!     MessageSource::Player { name: "Steve".to_string(), uuid: None },
//!     Bytes::from_static(b"feature-x"),
//! );
//! // Check if the identifier matches first, no matter the source.
//! if event.identifier().as_str() == "custom:main" {
//!     // Mark as handled before parsing, so the contents are not
//!     // forwarded to their original destination.
//!     event.handled();
//! }
//! // Only now bother with the source or the payload.
//! if let MessageSource::Player { name, .. } = event.source() {
//!     assert_eq!(name, "Steve");
//! }
//! assert!(event.is_handled());
//! # Ok::<(), candyriya_messaging::ChannelIdentifierError>(())
//! ```
//!
//! ## Case 2: The proxy sends a plugin message to a backend server
//!
//! There are two distinct ways to reach a backend, and they are **not**
//! interchangeable:
//!
//! - **Using any connected player**: send on a live player's backend
//!   connection towards that player's current server. Useful for messages
//!   relevant to the whole server or derivable from their content (example:
//!   telling the backend to shut down).
//! - **Using a specific player's connection**: same delivery path, but the
//!   payload is about one particular player (example: telling that player's
//!   backend to give them an item). Extra checks that the target server
//!   handles it are on the caller.
//!
//! Both routes end in the backend, so **on your backend server only listen
//! for plugin messages if you are sure only a trusted proxy can send them**.
//! Otherwise a player can pretend to be the proxy and spoof them — a
//! backend that handles `Connect`-like messages from anyone will happily
//! disconnect players on command. There is no event for Case 2: it is the
//! outbound direction, and the proxy is the sender.
//!
//! ## Case 3: A plugin message arrives from a backend server (server → proxy)
//!
//! If the channel is registered, the proxy fires [`PluginMessageEvent`]
//! with [`MessageSource::ServerBackend`]; otherwise the packet is dropped
//! or forwarded untouched. Example use case: a backend asking the proxy to
//! transfer the player to another server.
//!
//! > **Warning (from the upstream documentation):** set the result to
//! > [`ForwardResult::Handled`] **before checking the source**, right after
//! > the identifier matches. This stops any client-originated message on the
//! > same channel from being forwarded to clients as if it came from the
//! > proxy. If the result is forwarded, players can impersonate the proxy
//! > to your backend servers — and a backend can inject arbitrary messages
//! > into the proxy-to-client stream.
//!
//! ```
//! use candyriya_messaging::ChannelIdentifier;
//! use candyriya_messaging_velocity::{MessageSource, PluginMessageEvent};
//! use bytes::Bytes;
//!
//! let mut event = PluginMessageEvent::new(
//!     ChannelIdentifier::new("custom:main")?,
//!     MessageSource::ServerBackend { name: "lobby".to_string() },
//!     Bytes::from_static(b"transfer lobby"),
//! );
//! if event.identifier().as_str() == "custom:main" {
//!     // Mark handled unconditionally, right here — before checking the
//!     // source. A client sending on this channel is discarded instead of
//!     // being forwarded as if the proxy said so.
//!     event.handled();
//! }
//! // Only now it is safe to conclude the sender is the backend.
//! if let MessageSource::ServerBackend { name } = event.source() {
//!     assert_eq!(name, "lobby");
//! }
//! assert!(event.is_handled());
//! # Ok::<(), candyriya_messaging::ChannelIdentifierError>(())
//! ```
//!
//! ## Case 4: The proxy sends a plugin message to a player (proxy → client)
//!
//! The outbound direction towards the client. This is only really useful
//! together with a client-side mod — a vanilla client is not obligated to
//! understand anything sent on a plugin-message channel. Use it to feed
//! modded clients data the mod knows how to render.
//!
//! ## The leak-prevention guideline
//!
//! > **Mark the event handled whenever the channel matches. Only messages
//! > that remain unhandled leak to the other side.**
//!
//! Set the result between the identifier check and the source check, before
//! parsing. The reason is symmetric in both directions: whatever the event
//! default is, an early-return in your handler keeps it. If your default is
//! "forward", every unmatched early return leaks a message to the client or
//! the backend; if your default is "handled" (see
//! [`PluginMessageEvent::new_for_known_channel`]), every unmatched early
//! return silently drops a message you maybe wanted delivered. Choose the
//! default deliberately at construction time, and still set the result
//! explicitly once you know the channel matches.
//!
//! # Registering is the permission to intercept
//!
//! The [`ChannelRegistrar`] is the gate between "the proxy sees a plugin
//! message" and "your plugin sees an event". Velocity fires
//! `PluginMessageEvent` only for channels registered here; unregistered
//! channels skip handlers entirely. A plugin that never registers a channel
//! cannot intercept it, and a backend server should treat registered
//! channels as reachable only by the proxy (or by whoever can reach it —
//! which is why spoofed sources matter, see the Case 3 warning).
//!
//! # The BungeeCord compatibility set
//!
//! By default, a Velocity-compatible proxy answers the BungeeCord
//! subchannel protocol on the `bungeecord:main` channel (legacy name
//! `BungeeCord` is remapped internally), controlled by the
//! `bungee-plugin-message-channel` configuration flag. This crate ships the
//! same built-in behavior as [`BungeeChannelSet`]: the two channel names as
//! constants and the exact 18 subchannels the protocol defines
//! (`Connect` … `KickPlayerRaw`), wired to the constants in the
//! `candyriya-messaging-bungeecord` codec crate — never to copies. The
//! codec crate implements the actual subchannel byte protocol; this crate
//! only documents and re-exports the set.
//!
//! # Example
//!
//! ```
//! use candyriya_messaging::ChannelIdentifier;
//! use candyriya_messaging_velocity::{BungeeChannelSet, ChannelRegistrar, MessageSource};
//!
//! let mut registrar = ChannelRegistrar::new();
//! for channel in BungeeChannelSet::channels() {
//!     registrar.register(&channel);
//! }
//! assert!(registrar.is_registered(&ChannelIdentifier::new("bungeecord:main")?));
//! # Ok::<(), candyriya_messaging::ChannelIdentifierError>(())
//! ```
//!
//! # Safety
//!
//! `#![forbid(unsafe_code)]`; no networking, no async, no threads. All
//! construction and mutation goes through ordinary safe APIs.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod compat;
mod event;
mod registrar;

pub use compat::BungeeChannelSet;
pub use event::{ForwardResult, MessageSource, PluginMessageEvent};
pub use registrar::ChannelRegistrar;
