//! The channel registrar: the set of channels the proxy listens on.
//!
//! A proxy intercepts plugin messages only on channels it registered here;
//! everything else bypasses the event machinery untouched. Registering is
//! therefore the **permission to listen**: no registration, no event, no
//! inspection. Backends, conversely, should only accept messages on a
//! channel if they trust the only source that will ever send on it (their
//! proxy) — an unregistered listener behind a restrictive registrar is the
//! usual accident.
//!
//! The registrar models the actual channel set the proxy asks both sides to
//! register on join (Minecraft's `minecraft:register` on the wire). This
//! crate stores it as an ordered set: insertion order is preserved,
//! duplicates are ignored, and unregistering removes the first occurrence.

use candyriya_messaging::ChannelIdentifier;

/// The set of channels the proxy listens on, in registration order.
///
/// Duplicate registrations are idempotent; [`channels`](Self::channels)
/// returns clones in first-registration order, which is what the proxy
/// serializes into join-time `minecraft:register` packets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelRegistrar {
    channels: Vec<ChannelIdentifier>,
}

impl ChannelRegistrar {
    /// An empty registrar.
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
        }
    }

    /// Registers `identifier`.
    ///
    /// Returns `true` if the channel was newly added, `false` if it was
    /// already registered (the set is unchanged, order included).
    pub fn register(&mut self, identifier: &ChannelIdentifier) -> bool {
        if self.is_registered(identifier) {
            return false;
        }
        self.channels.push(identifier.clone());
        true
    }

    /// Unregisters `identifier`.
    ///
    /// Returns `true` if the channel was present and is now removed,
    /// `false` otherwise. Re-registering a previously unregistered channel
    /// appends it at the end of the order.
    pub fn unregister(&mut self, identifier: &ChannelIdentifier) -> bool {
        match self.channels.iter().position(|c| c == identifier) {
            Some(index) => {
                self.channels.remove(index);
                true
            }
            None => false,
        }
    }

    /// Removes every registered channel.
    pub fn clear(&mut self) {
        self.channels.clear();
    }

    /// Whether `identifier` is registered.
    pub fn is_registered(&self, identifier: &ChannelIdentifier) -> bool {
        self.channels.iter().any(|c| c == identifier)
    }

    /// The registered channels, in first-registration order.
    ///
    /// Returns clones so callers can iterate while mutating the registrar.
    pub fn channels(&self) -> Vec<ChannelIdentifier> {
        self.channels.clone()
    }

    /// How many channels are registered.
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// Whether no channels are registered.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}
