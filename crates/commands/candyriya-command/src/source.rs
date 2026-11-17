//! Who invoked a command, and what they may do.
//!
//! [`CommandSource`] is the concrete `S` type parameter of
//! `azalea_brigadier::CommandDispatcher<S>`. Azalea Brigadier monomorphizes the
//! dispatcher over a single source type, so this crate ships exactly one
//! concrete source instead of a `Box<dyn ...>` or a Java-style hierarchy.
//!
//! The read-only surface is intentionally small:
//!
//! - [`source_kind`](CommandSource::source_kind) — console, player or backend;
//! - [`player_uuid`](CommandSource::player_uuid) / [`player_name`](CommandSource::player_name);
//! - [`locale`](CommandSource::locale);
//! - [`has_permission`](CommandSource::has_permission) — backed by the real
//!   `candyriya-permissions` engine (registry default-deny + wildcard matching +
//!   allow/deny precedence), never a shortcut.
//!
//! ## Permission policy per origin
//!
//! - [`CommandSource::console`] is granted `*` (the conventional console
//!   default, matching the default config grants) so every command passes;
//! - [`CommandSource::player`] and [`CommandSource::backend`] start with **no**
//!   grants: every permission check falls through to the registry default,
//!   which is deny. Commands that require a node are invisible and cannot be
//!   executed until a grant is wired in (see [`CommandSource::with_grant`]).

use std::fmt;
use std::sync::Arc;

use candyriya_permissions::{
    Grant, InMemoryProvider, PermissionNode, PermissionRegistry, PermissionResolver, Subject,
    SubjectKind,
};
use uuid::Uuid;

/// A reference to a player, copied out of the session layer.
///
/// Carries just enough to identify the player in logs, permission subjects and
/// error messages. The live session lives elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlayerRef {
    /// The player's unique id.
    pub uuid: Uuid,
    /// The player's name, as provided by the session layer (not validated here).
    pub name: String,
}

impl PlayerRef {
    /// Creates a player reference from its parts.
    pub fn new(uuid: Uuid, name: impl Into<String>) -> Self {
        Self {
            uuid,
            name: name.into(),
        }
    }
}

impl fmt::Display for PlayerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.uuid)
    }
}

/// What kind of thing invoked the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// The console (stdin/CLI).
    Console,
    /// A player connected through the proxy.
    Player,
    /// A backend server, when the backend issues a command through the proxy.
    Backend,
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Console => f.write_str("console"),
            Self::Player => f.write_str("player"),
            Self::Backend => f.write_str("backend"),
        }
    }
}

/// Identifies who/what invoked the command.
///
/// Carries the payload needed to name the invoker; the permission state lives
/// in [`CommandSource`], not here, because the same origin may hold different
/// grants depending on wiring.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CommandOrigin {
    /// The console.
    Console,
    /// A player connected through the proxy.
    Player(PlayerRef),
    /// A backend server, identified by its configured name.
    Backend {
        /// The configured server name (as in `[servers]`).
        name: String,
    },
}

impl fmt::Display for CommandOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Console => f.write_str("console"),
            Self::Player(player) => write!(f, "player {player}"),
            Self::Backend { name } => write!(f, "backend {name}"),
        }
    }
}

impl CommandOrigin {
    /// The kind of this origin, without the payload.
    pub fn kind(&self) -> SourceKind {
        match self {
            Self::Console => SourceKind::Console,
            Self::Player(_) => SourceKind::Player,
            Self::Backend { .. } => SourceKind::Backend,
        }
    }
}

/// Who invoked the command. The `S` of `azalea_brigadier::CommandDispatcher<S>`.
///
/// Construct with [`CommandSource::console`], [`CommandSource::player`] or
/// [`CommandSource::backend`]; tweak with [`CommandSource::with_locale`] and
/// [`CommandSource::with_grant`].
#[derive(Debug)]
pub struct CommandSource {
    origin: CommandOrigin,
    locale: Option<String>,
    permissions: PermissionState,
}

/// The permission engine state carried by a source.
#[derive(Debug)]
struct PermissionState {
    subject: Subject,
    provider: InMemoryProvider,
    resolver: PermissionResolver,
}

impl CommandSource {
    /// A console source with the conventional `*` allow grant.
    pub fn console() -> Self {
        let subject = Subject::new(SubjectKind::Console, "console");
        let mut provider = InMemoryProvider::new();
        // console semantics: may do everything, same spirit as the default
        // config grant ("default" => ["*"]).
        provider.grant(&subject, Grant::allow(star_node()));
        Self::from_parts(CommandOrigin::Console, subject, provider)
    }

    /// A player source. Starts with no grants: default-deny on every node.
    ///
    /// Use [`CommandSource::with_grant`] to wire real grants in.
    pub fn player(player: PlayerRef) -> Self {
        let subject = Subject::new(SubjectKind::Player, player.uuid.to_string());
        Self::from_parts(
            CommandOrigin::Player(player),
            subject,
            InMemoryProvider::new(),
        )
    }

    /// A backend source, identified by its configured server name. Starts with
    /// no grants: default-deny on every node.
    pub fn backend(name: impl Into<String>) -> Self {
        let name = name.into();
        let subject = Subject::new(SubjectKind::Other("backend".to_owned()), &name);
        Self::from_parts(
            CommandOrigin::Backend { name },
            subject,
            InMemoryProvider::new(),
        )
    }

    fn from_parts(origin: CommandOrigin, subject: Subject, provider: InMemoryProvider) -> Self {
        // An empty registry: unregistered nodes fall back to deny. All the
        // permissiveness a source has comes from explicit grants.
        let resolver = PermissionResolver::new(Arc::new(PermissionRegistry::new()));
        Self {
            origin,
            locale: None,
            permissions: PermissionState {
                subject,
                provider,
                resolver,
            },
        }
    }

    /// The origin this source invoked with.
    pub fn origin(&self) -> &CommandOrigin {
        &self.origin
    }

    /// The kind of invoker, without the payload.
    pub fn source_kind(&self) -> SourceKind {
        self.origin.kind()
    }

    /// The player's uuid, when invoked by a player.
    pub fn player_uuid(&self) -> Option<Uuid> {
        match &self.origin {
            CommandOrigin::Player(player) => Some(player.uuid),
            _ => None,
        }
    }

    /// The player's name, when invoked by a player.
    pub fn player_name(&self) -> Option<&str> {
        match &self.origin {
            CommandOrigin::Player(player) => Some(player.name.as_str()),
            _ => None,
        }
    }

    /// The backend's configured name, when invoked by a backend.
    pub fn backend_name(&self) -> Option<&str> {
        match &self.origin {
            CommandOrigin::Backend { name } => Some(name.as_str()),
            _ => None,
        }
    }

    /// The invoker's locale tag (e.g. `en`, `ex_girl`), if known.
    pub fn locale(&self) -> Option<&str> {
        self.locale.as_deref()
    }

    /// Sets the invoker's locale tag. Empty or whitespace-only becomes `None`.
    pub fn with_locale(mut self, locale: impl Into<String>) -> Self {
        let locale = locale.into();
        let trimmed = locale.trim();
        self.locale = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        };
        self
    }

    /// Adds a grant to this source's provider.
    ///
    /// Grants are resolved by the `candyriya-permissions` engine: specificity
    /// first (a longer pattern always wins, allow or deny), then deny at equal
    /// length, then the registry default, then deny.
    pub fn with_grant(mut self, grant: Grant) -> Self {
        self.permissions
            .provider
            .grant(&self.permissions.subject, grant);
        self
    }

    /// Whether this source is allowed the permission node.
    ///
    /// An invalid node (grammar violation) is denied. Resolution runs the real
    /// engine resolver against this source's effective grants: subject grants ∪
    /// group grants, specificity first, deny wins at equal length, registry
    /// default-deny as the floor.
    pub fn has_permission(&self, node: &str) -> bool {
        let Ok(node) = PermissionNode::new(node) else {
            // invalid node = deny; never a parse-failure-turned-allow
            return false;
        };
        let state = &self.permissions;
        let grants = state.provider.effective_grants(&state.subject);
        state.resolver.has(&state.subject, &node, &grants)
    }
}

impl fmt::Display for CommandSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.locale {
            Some(locale) => write!(f, "{origin} (locale: {locale})", origin = self.origin),
            None => write!(f, "{}", self.origin),
        }
    }
}

/// The `*` grant used by the console. Cannot fail to construct (grammar allows it).
fn star_node() -> PermissionNode {
    PermissionNode::new("*").expect("`*` is a valid permission node")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::parse_str("b50ad385-829d-3141-a216-7e7d7539ba7f").expect("fixed uuid parses")
    }

    #[test]
    fn console_allows_everything() {
        let console = CommandSource::console();
        assert_eq!(console.source_kind(), SourceKind::Console);
        assert_eq!(console.player_uuid(), None);
        assert_eq!(console.player_name(), None);
        assert!(console.has_permission("candyriya.command.stop"));
        assert!(console.has_permission("*"));
        assert_eq!(console.to_string(), "console");
    }

    #[test]
    fn player_is_denied_by_default() {
        let player = CommandSource::player(PlayerRef::new(uuid(), "Notch"));
        assert_eq!(player.source_kind(), SourceKind::Player);
        assert_eq!(player.player_uuid(), Some(uuid()));
        assert_eq!(player.player_name(), Some("Notch"));
        assert!(!player.has_permission("candyriya.command.stop"));
        assert!(!player.has_permission("*"));
    }

    #[test]
    fn backend_is_denied_by_default() {
        let backend = CommandSource::backend("lobby");
        assert_eq!(backend.source_kind(), SourceKind::Backend);
        assert_eq!(backend.backend_name(), Some("lobby"));
        assert_eq!(backend.player_uuid(), None);
        assert!(!backend.has_permission("candyriya.command.stop"));
    }

    #[test]
    fn with_grant_allows() {
        let player = CommandSource::player(PlayerRef::new(uuid(), "Notch")).with_grant(
            Grant::allow(PermissionNode::new("candyriya.command.server").expect("valid node")),
        );
        assert!(player.has_permission("candyriya.command.server"));
        assert!(!player.has_permission("candyriya.command.stop"));
    }

    #[test]
    fn more_specific_deny_beats_wildcard_allow() {
        let player = CommandSource::player(PlayerRef::new(uuid(), "Notch"))
            .with_grant(Grant::allow(
                PermissionNode::new("candyriya.*").expect("valid node"),
            ))
            .with_grant(Grant::deny(
                PermissionNode::new("candyriya.command.stop").expect("valid node"),
            ));
        assert!(!player.has_permission("candyriya.command.stop"));
        assert!(player.has_permission("candyriya.command.server"));
    }

    #[test]
    fn deny_wins_at_equal_specificity_and_length() {
        // same pattern, allow and deny: the engine breaks the tie for deny
        let player = CommandSource::player(PlayerRef::new(uuid(), "Notch"))
            .with_grant(Grant::allow(
                PermissionNode::new("candyriya.command.server").expect("valid node"),
            ))
            .with_grant(Grant::deny(
                PermissionNode::new("candyriya.command.server").expect("valid node"),
            ));
        assert!(!player.has_permission("candyriya.command.server"));
    }

    #[test]
    fn invalid_node_is_denied() {
        let console = CommandSource::console();
        assert!(!console.has_permission(""));
        assert!(!console.has_permission("Invalid Node!"));
        assert!(!console.has_permission("**"));
    }

    #[test]
    fn display_includes_locale() {
        let source = CommandSource::console().with_locale("ex_girl");
        assert_eq!(source.to_string(), "console (locale: ex_girl)");
        assert_eq!(source.locale(), Some("ex_girl"));
        let source = CommandSource::console().with_locale("   ");
        assert_eq!(source.locale(), None);
    }

    #[test]
    fn player_display() {
        let player = CommandSource::player(PlayerRef::new(uuid(), "Notch"));
        assert_eq!(
            player.to_string(),
            "player Notch (b50ad385-829d-3141-a216-7e7d7539ba7f)"
        );
    }
}
