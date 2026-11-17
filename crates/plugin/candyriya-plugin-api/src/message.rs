//! Chat and message-level building blocks.
//!
//! This module is the host-side 90% of everything the proxy ever sends to a
//! player: chat lines, kick/disconnect screens, titles and the actionbar.
//! It is deliberately thin over [lobsterchat](https://docs.rs/lobsterchat);
//! nothing here reimplements component logic, it only makes the common
//! shapes fluent and impossible to misuse.
//!
//! What lives here:
//!
//! - [`text`] and [`translatable`] — the two component constructors the
//!   plugin code generator targets. Everything else in the crate composes
//!   these.
//! - [`ComponentBuilder`] — a fluent wrapper around lobsterchat's component
//!   construction, covering the formatting/color/event combinations that
//!   show up in real messages. Components can still be built by hand; the
//!   builder exists so call sites read left to right.
//! - [`MessageMode`] — how the host should deliver a message to the player
//!   (plain chat, system message or the actionbar overlay).
//!
//! For parsing MiniMessage input coming from plugins or config, see
//! [`crate::parse_minimessage`]; for kick/disconnect reasons, see
//! [`crate::KickReason`].

use lobsterchat::component::{ClickEvent, Colored, Component, HoverEvent, NamedColor, TextColor};
use serde::{Deserialize, Serialize};

/// Builds a literal text component.
///
/// This is the canonical constructor plugin code generators target.
///
/// # Examples
///
/// ```
/// use candyriya_plugin_api::text;
///
/// let line = text("Hello, world!");
/// ```
pub fn text(s: &str) -> Component {
    Component::text(s)
}

/// Builds a translatable component with no or some positional arguments.
///
/// `args` become the `with` list of the resulting translatable. An empty
/// list is serialized as no `with` field at all, which is what the client
/// expects for argument-free translations.
///
/// # Examples
///
/// ```
/// use candyriya_plugin_api::{text, translatable};
///
/// let player = text("Alice");
/// let line = translatable("multiplayer.player.joined", &[player]);
/// ```
pub fn translatable(key: &str, args: &[Component]) -> Component {
    let placeholders = if args.is_empty() {
        None
    } else {
        Some(args.to_vec())
    };
    Component::translate(key, placeholders)
}

/// How the host should deliver a chat message to the player.
///
/// Mirrors the delivery modes Minecraft itself knows about: a regular
/// player-chat line, a system message, or the `game_info` overlay shown
/// above the hotbar (what most people call the actionbar).
///
/// Serializes as a snake_case string (`"chat"`, `"system"`,
/// `"game_info"`), which is the wire convention the host uses.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageMode {
    /// A regular chat message, shown in the player's chat history.
    Chat,
    /// A system message, styled as such by the client (vanilla `system`
    /// chat type). Used for server-driven notices that should not look
    /// like they came from another player.
    System,
    /// The `game_info` overlay: a single line above the hotbar, best for
    /// transient information ("This server will restart in 5 minutes").
    GameInfo,
}

impl MessageMode {
    /// The stable wire name of the mode (`"chat"`, `"system"`,
    /// `"game_info"`), matching the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::System => "system",
            Self::GameInfo => "game_info",
        }
    }
}

impl Default for MessageMode {
    /// Chat is the default delivery mode.
    fn default() -> Self {
        Self::Chat
    }
}

/// Fluent builder for literal components.
///
/// Wraps lobsterchat's by-value component modifiers, so a component reads
/// the way it is meant to be: `text("...").color(...).bold().append(...)`.
/// Every method consumes and returns the builder, and the whole thing
/// finally turns into a [`Component`] with [`ComponentBuilder::build`] (or
/// `Component::from`).
///
/// This intentionally does not cover every lobsterchat capability — no
/// scoreboard/entity/NBT components, no translations object — just the
/// formatting, color, event and append shapes that cover the typical
/// message the proxy sends. Drop down to lobsterchat directly for the
/// exotic cases.
///
/// # Examples
///
/// ```
/// use candyriya_plugin_api::{ComponentBuilder, HoverEvent};
/// use lobsterchat::component::{Component, NamedColor};
///
/// let line = ComponentBuilder::text("Hover me")
///     .color(NamedColor::Gold)
///     .bold()
///     .hover_event(HoverEvent::show_text(Component::text("You made it")))
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct ComponentBuilder {
    inner: Component,
}

impl ComponentBuilder {
    /// Starts a builder from a literal text component. This is the entry
    /// point for almost everything; other component kinds (translatable,
    /// keybind, ...) can be appended or used as the base via `build` and
    /// then combined by hand.
    pub fn text(s: &str) -> Self {
        Self {
            inner: Component::text(s),
        }
    }

    /// Sets a named color. Replaces any color set before.
    pub fn color(mut self, color: NamedColor) -> Self {
        Self {
            inner: self.inner.color(color),
        }
    }

    /// Sets a 24-bit hex color from the lower 24 bits of `rgb`.
    pub fn hex_color(mut self, rgb: u32) -> Self {
        Self {
            inner: self.inner.color(TextColor::Hex(format!("#{rgb:06X}"))),
        }
    }

    /// Enables **bold**. Replaces any previous bold flag.
    pub fn bold(mut self) -> Self {
        Self {
            inner: self.inner.bold(true),
        }
    }

    /// Enables *italic*. Replaces any previous italic flag.
    pub fn italic(mut self) -> Self {
        Self {
            inner: self.inner.italic(true),
        }
    }

    /// Enables underline. Replaces any previous underlined flag.
    pub fn underlined(mut self) -> Self {
        Self {
            inner: self.inner.underlined(true),
        }
    }

    /// Enables strikethrough. Replaces any previous strikethrough flag.
    pub fn strikethrough(mut self) -> Self {
        Self {
            inner: self.inner.strikethrough(true),
        }
    }

    /// Enables obfuscated (the "magic" random-character effect).
    pub fn obfuscated(mut self) -> Self {
        Self {
            inner: self.inner.obfuscated(true),
        }
    }

    /// Clears all inherited formatting for this component — used for
    /// separators and for ending a formatted span.
    pub fn reset(mut self) -> Self {
        Self {
            inner: self.inner.reset(true),
        }
    }

    /// Appends a child component. Children render after the parent's own
    /// content, in the order they were appended.
    pub fn append(mut self, child: impl Into<Component>) -> Self {
        Self {
            inner: self.inner.append(child),
        }
    }

    /// Appends a separator: the literal `symbol` wrapped in a formatting
    /// reset, so e.g. `" | "` does not inherit the surrounding color or
    /// bold. This is the "pipe between name and badge" pattern.
    pub fn separator(self, symbol: &str) -> Self {
        self.append(Component::text(symbol).reset(true))
    }

    /// Sets the hover event, e.g.
    /// `HoverEvent::show_text(Component::text("..."))`.
    pub fn hover_event(mut self, event: HoverEvent) -> Self {
        Self {
            inner: self.inner.hover_event(event),
        }
    }

    /// Sets the click event, e.g. `ClickEvent::open_url("https://...")`.
    pub fn click_event(mut self, event: ClickEvent) -> Self {
        Self {
            inner: self.inner.click_event(event),
        }
    }

    /// Sets the text inserted into the player's chat box when this
    /// component is shift-clicked.
    pub fn insert(mut self, text: &str) -> Self {
        Self {
            inner: self.inner.insert_text(text),
        }
    }

    /// Finishes the builder and returns the assembled component.
    pub fn build(self) -> Component {
        self.inner
    }
}

impl From<ComponentBuilder> for Component {
    fn from(builder: ComponentBuilder) -> Self {
        builder.inner
    }
}

impl Default for ComponentBuilder {
    /// An empty literal component.
    fn default() -> Self {
        Self {
            inner: Component::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_text_hex_color_is_zero_padded() {
        let component = ComponentBuilder::text("x").hex_color(0x0000_00FF).build();
        let json = serde_json::to_value(&component).expect("serialize");
        assert_eq!(json["color"], "#0000FF");
    }

    #[test]
    fn message_mode_wire_names() {
        let cases = [
            (MessageMode::Chat, "chat"),
            (MessageMode::System, "system"),
            (MessageMode::GameInfo, "game_info"),
        ];
        for (mode, wire) in cases {
            assert_eq!(mode.as_str(), wire);
            let json = serde_json::to_string(&mode).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""));
            let back: MessageMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, mode);
        }
        assert_eq!(MessageMode::default(), MessageMode::Chat);
    }
}
