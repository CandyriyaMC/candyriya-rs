//! # candyriya-permissions-config
//!
//! The **default permission provider** for candyriya-rs: it sources grants from
//! the TOML configuration the proxy already owns (`[permissions]` in
//! `candyriya.toml`) and materializes [`Grant`] instances for the
//! [`candyriya_permissions`] engine. It is what runs on networks that do not
//! install a LuckPerms-like plugin.
//!
//! This crate does **not** resolve permissions. Resolution lives in
//! [`candyriya_permissions::PermissionResolver`]; the provider only assembles
//! the flat, effective grant list the resolver consumes — exactly like
//! [`candyriya_permissions::InMemoryProvider::effective_grants`], but backed by
//! validated config data instead of ad-hoc in-memory setup.
//!
//! ## Where the config lives
//!
//! `candyriya-config` ships a `[permissions]` section:
//!
//! ```toml
//! [permissions]
//! provider = "config"
//! default = "default"
//!
//! [permissions.grants]
//! default = ["candyriya.ping"]            # implicit all-subject group
//! admin = ["*"]                           # group "admin"
//! moderator = ["candyriya.command.*"]     # group "moderator"
//! ```
//!
//! The config crate validates that table **structurally** (keys are group
//! names, values are lists of strings) and nothing more. The semantics — which
//! string is a node, who holds which group, what "default" means — are defined
//! here, not there. Group assignment lives in a sibling table that this
//! provider consumes but `candyriya-config` does not yet model (pass it through
//! [`ConfigPermissionProvider::new`]):
//!
//! ```toml
//! # consumed by this crate via ConfigPermissionProvider::new; not (yet) a
//! # first-class candyriya-config field
//! [permissions.players]
//! "069a79f4-44e9-4726-a5be-fca90e38aaf5" = ["admin"]
//! ```
//!
//! ## Semantics (the contract)
//!
//! 1. **Nodes are validated, never trusted.** Every node string is parsed with
//!    [`PermissionNode::new`]. Plain (unsigned) entries mean **allow**; this
//!    provider has no deny syntax and no contexts in v1 — see "No deny, no
//!    contexts" below.
//! 2. **The `default` key is the implicit all-subject group.** Its nodes are
//!    handed to **every subject of kind Console and Player**, unconditionally.
//!    It is not handed to Group subjects (a group's grants are its own table
//!    entry) nor to `Other` subjects. There is no opt-out; if you do not want
//!    a node for everyone, do not put it under `default`.
//! 3. **Group nodes apply to players that hold the group.** A player holds a
//!    group when the group name appears either in the provider's own
//!    `player_groups` map (keyed by the subject id, i.e. the uuid string) or
//!    in the flat `subject.groups` list the proxy put on the subject. Both
//!    sources are unioned (config order first, then subject order), duplicates
//!    removed. Group assignment is a **player-only** concern: Console and
//!    Group subjects never pull in group grants through this path. Nesting
//!    (group-in-group) is **not** expanded — flattening is the caller's job,
//!    same rule as the rest of the engine.
//! 4. **A Group subject gets its own grants and nothing else.**
//!    `grants_for(SubjectKind::Group, "admin")` returns the `admin` bucket
//!    (plus dedupe against itself), never the `default` grants.
//! 5. **Grants dedupe.** Identical grants (same node, same value) are emitted
//!    once, even if they appear in several buckets (e.g. a node present in
//!    both `default` and `admin`) or twice inside one list. Allow-vs-deny at
//!    the same specificity is *not* collapsed — precedence is the resolver's
//!    concern.
//! 6. **Wildcards flow through verbatim.** `candyriya.command.*` is stored as
//!    the pattern `candyriya.command.*`; matching is the resolver's job and no
//!    expansion happens here.
//! 7. **Errors are total.** [`ConfigPermissionProvider::new`] and
//!    [`ConfigPermissionProvider::load_from_config`] collect **every** invalid
//!    node in one pass and report them together, each with its offending
//!    table key, its 0-based index and the validation error — never a single
//!    first-failure-stops-all surprise.
//! 8. **Deterministic output.** Groups are iterated in sorted key order
//!    (backed by [`BTreeMap`]); grants keep config order within a bucket; the
//!    union is stable for identical inputs. Dedupe keeps the first occurrence.
//! 9. **No resolution, no caching, no I/O, no clock.** This crate assembles
//!    grant lists only. The resolver is in `candyriya-permissions`.
//!
//! ## No deny, no contexts (v1)
//!
//! A network that needs deny grants or per-context grants out of the box must
//! install a plugin-backed provider; this one is deliberately minimal. Plain
//! strings cannot express denial (a leading `-` is a valid node prefix in the
//! node grammar, so overloading it would be ambiguous), and a deny-by-default
//! escape hatch would surprise operators. `Other` subjects receive no grants
//! at all — give them explicit grants via a different provider.
//!
//! ## Example
//!
//! ```
//! use std::collections::HashMap;
//! use candyriya_permissions::{Subject, SubjectKind};
//! use candyriya_permissions_config::{ConfigPermissionProvider, PermissionProvider};
//!
//! let mut groups = HashMap::new();
//! groups.insert("admin".to_string(), vec!["*".to_string()]);
//! let mut players = HashMap::new();
//! players.insert("069a79f4-44e9-4726-a5be-fca90e38aaf5".to_string(), vec!["admin".to_string()]);
//!
//! let provider = ConfigPermissionProvider::new(
//!     vec!["candyriya.ping".to_string()],
//!     groups,
//!     players,
//! )
//! .expect("valid nodes");
//!
//! let console = Subject::new(SubjectKind::Console, "console");
//! let player = Subject::new(SubjectKind::Player, "069a79f4-44e9-4726-a5be-fca90e38aaf5");
//! assert_eq!(provider.grants_for(&console).len(), 1); // default only
//! assert_eq!(provider.grants_for(&player).len(), 2); // default + admin's "*"
//! ```
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use candyriya_permissions::{Grant, PermissionNode, PermissionNodeError, Subject, SubjectKind};
use thiserror::Error;

/// The table key that names the implicit all-subject group.
///
/// In [`ConfigPermissionProvider::load_from_config`], an entry under this key
/// feeds the provider-wide default grants; every other key names a group.
pub const DEFAULT_GROUP: &str = "default";

/// One raw `(group, node)` pair, as it appears in the config table.
///
/// This is the flattened row shape of `[permissions.grants]`: the key is the
/// group name ([`DEFAULT_GROUP`] for the all-subject bucket), the node is the
/// unparsed config string. It exists so callers and this crate can talk about
/// individual entries without touching the whole table; [`GrantInput::into_grant`]
/// parses one node into a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantInput {
    /// Group name the node belongs to ([`DEFAULT_GROUP`] for the all-subject bucket).
    pub group: String,
    /// The raw node text; not yet validated.
    pub node: String,
}

impl GrantInput {
    /// Parses [`GrantInput::node`] into an allow grant.
    ///
    /// The grant's `source` is set to `config:default` for the all-subject
    /// bucket and `config:group:<name>` otherwise — diagnostics only, no
    /// resolution semantics.
    ///
    /// # Errors
    ///
    /// Returns the [`PermissionNodeError`] from [`PermissionNode::new`] when
    /// the node text is invalid.
    pub fn into_grant(self) -> Result<Grant, PermissionNodeError> {
        Ok(Grant::allow(PermissionNode::new(&self.node)?).with_source(grant_source(&self.group)))
    }
}

fn grant_source(group: &str) -> String {
    if group == DEFAULT_GROUP {
        "config:default".to_string()
    } else {
        format!("config:group:{group}")
    }
}

/// One invalid node entry, with its position in the config table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidNode {
    /// The table key the entry was found under ([`DEFAULT_GROUP`] for the
    /// all-subject bucket).
    pub key: String,
    /// 0-based index of the entry within the key's node list.
    pub index: usize,
    /// The offending node text.
    pub node: String,
    /// Why the node failed validation.
    pub source: PermissionNodeError,
}

impl fmt::Display for InvalidNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "at {key}[{index}] = {node:?}: {source}",
            key = self.key,
            index = self.index,
            node = self.node,
            source = self.source,
        )
    }
}

/// Every invalid node found in a config pass, collected in one error.
///
/// [`ConfigPermissionProvider::new`] and [`ConfigPermissionProvider::load_from_config`]
/// parse the whole table before failing, so an operator fixing a config file
/// can fix all typos in one round-trip instead of one error per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidNodes(Vec<InvalidNode>);

impl InvalidNodes {
    /// The offending entries, in table order (buckets sorted by key, entries
    /// in list order within a bucket).
    pub fn entries(&self) -> &[InvalidNode] {
        &self.0
    }
}

impl fmt::Display for InvalidNodes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid permission node(s) in permissions config:")?;
        for entry in &self.0 {
            writeln!(f, "  {entry}")?;
        }
        Ok(())
    }
}

// Present so InvalidNodes can act as a #[source] link inside ProviderError.
impl std::error::Error for InvalidNodes {}

/// Errors returned when building a provider from raw config data.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// One or more node strings in the config failed validation. Carries every
    /// offending entry, not just the first.
    #[error("{0}")]
    InvalidNodes(#[from] InvalidNodes),
}

/// The provider contract the proxy consumes.
///
/// This trait is deliberately narrow: the only question an executor may ask a
/// provider is for the flat, effective grant list for a subject, ready to hand
/// to [`candyriya_permissions::PermissionResolver::resolve`]. Group
/// bookkeeping, storage and validation are the provider's own business.
///
/// `candyriya-permissions` intentionally ships its [`candyriya_permissions::InMemoryProvider`]
/// without a trait (one known shape, no abstraction); this crate defines the
/// trait so the proxy can hold "some provider" and pass it down while the
/// concrete wiring (this config provider, or a plugin-backed one) is decided
/// at startup. Resolution semantics do not depend on the trait.
pub trait PermissionProvider {
    /// Returns the flat, effective grant list for `subject`.
    ///
    /// The list is already flattened (subject's own grants ∪ grants of every
    /// group the subject holds, one level) and deduped; hand it to the
    /// resolver as-is. Order is provider-defined and stable for identical
    /// inputs; it affects only which grant is reported as
    /// [`candyriya_permissions::BestMatch`], never the decision.
    fn grants_for(&self, subject: &Subject) -> Vec<Grant>;
}

/// The default, config-backed permission provider.
///
/// See the [crate-level semantics](crate#semantics-the-contract) for the full
/// contract. Backed by [`BTreeMap`] for deterministic iteration. Cheap to
/// clone; grants are shared by value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigPermissionProvider {
    default_grants: Vec<Grant>,
    group_grants: BTreeMap<String, Vec<Grant>>,
    player_groups: BTreeMap<String, Vec<String>>,
}

impl ConfigPermissionProvider {
    /// Builds a provider from raw, unvalidated config data.
    ///
    /// * `default_nodes` — nodes granted to every Console and Player subject
    ///   (the `default` bucket).
    /// * `group_nodes` — group name → raw node strings. Keys are opaque,
    ///   case-sensitive group ids; empty keys are tolerated (they simply can
    ///   never be matched by a sensible subject) and whitespace in keys is
    ///   the config layer's concern, not this crate's.
    /// * `player_groups` — player subject id (uuid string) → group names the
    ///   player holds. Unknown group names are ignored at resolution time.
    ///
    /// Every node is validated before anything is stored; **all** invalid
    /// entries are collected and returned as one [`ProviderError::InvalidNodes`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidNodes`] listing every invalid node with
    /// its table key, 0-based index and validation error.
    pub fn new(
        default_nodes: Vec<String>,
        group_nodes: HashMap<String, Vec<String>>,
        player_groups: HashMap<String, Vec<String>>,
    ) -> Result<Self, ProviderError> {
        let mut errors = Vec::new();

        let default_grants = collect_grants(DEFAULT_GROUP, &default_nodes, &mut errors);

        let mut sorted_groups: Vec<(String, Vec<String>)> = group_nodes.into_iter().collect();
        sorted_groups.sort_by(|a, b| a.0.cmp(&b.0));

        let mut group_grants = BTreeMap::new();
        for (group, nodes) in sorted_groups {
            let grants = collect_grants(&group, &nodes, &mut errors);
            group_grants.insert(group, grants);
        }

        if !errors.is_empty() {
            return Err(InvalidNodes(errors).into());
        }

        let player_groups = player_groups
            .into_iter()
            .map(|(player, groups)| (player, deduped_strings(groups)))
            .collect();

        Ok(Self {
            default_grants,
            group_grants,
            player_groups,
        })
    }

    /// Builds a provider from the flat `[permissions.grants]` table alone.
    ///
    /// `cfg` is the grants table as a list of `(key, nodes)` pairs (a
    /// `HashMap` iterated into a slice, or built by hand). The literal key
    /// [`DEFAULT_GROUP`] feeds the all-subject bucket; every other key names
    /// a group. Player assignments cannot be expressed in this flat shape, so
    /// the resulting provider has no `player_groups` entries — use
    /// [`ConfigPermissionProvider::new`] for that. Duplicate keys in the slice
    /// are merged in slice order.
    ///
    /// # Errors
    ///
    /// Same as [`ConfigPermissionProvider::new`]: every invalid node is
    /// reported at once.
    pub fn load_from_config(cfg: &[(String, Vec<String>)]) -> Result<Self, ProviderError> {
        let mut default_nodes = Vec::new();
        let mut group_nodes: HashMap<String, Vec<String>> = HashMap::new();
        for (key, nodes) in cfg {
            if key == DEFAULT_GROUP {
                default_nodes.extend(nodes.iter().cloned());
            } else {
                group_nodes
                    .entry(key.clone())
                    .or_default()
                    .extend(nodes.iter().cloned());
            }
        }
        Self::new(default_nodes, group_nodes, HashMap::new())
    }

    /// Grants stored under a group's own key, deduplicated.
    ///
    /// Returns an empty slice for unknown groups. Does **not** include the
    /// all-subject default grants.
    pub fn grants_for_group(&self, group: &str) -> &[Grant] {
        self.group_grants
            .get(group)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The all-subject (`default`) grants, deduplicated.
    pub fn default_grants(&self) -> &[Grant] {
        &self.default_grants
    }

    /// The group names assigned to `player` via the provider's own
    /// `player_groups` map, in assignment order. Empty for unknown players.
    pub fn player_groups(&self, player: &str) -> &[String] {
        self.player_groups
            .get(player)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Total number of stored grants across all buckets (after per-bucket
    /// dedupe). Not context-aware — config grants carry no contexts.
    pub fn len(&self) -> usize {
        self.default_grants.len() + self.group_grants.values().map(Vec::len).sum::<usize>()
    }

    /// Whether the provider holds no grants at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Computes the flat effective grant list for `subject`.
    ///
    /// Equivalent to the [`PermissionProvider::grants_for`] implementation;
    /// see the crate-level semantics for the union rules.
    pub fn effective_grants(&self, subject: &Subject) -> Vec<Grant> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut push_all = |grants: &[Grant]| {
            for grant in grants {
                if seen.insert((grant.node.clone(), grant.value)) {
                    out.push(grant.clone());
                }
            }
        };

        match &subject.kind {
            SubjectKind::Console | SubjectKind::Player => push_all(&self.default_grants),
            SubjectKind::Group => {
                if let Some(grants) = self.group_grants.get(&subject.id) {
                    push_all(grants);
                }
            }
            SubjectKind::Other(_) => {}
        }

        if subject.kind == SubjectKind::Player {
            // Provider-owned assignments first (config order), then the flat
            // subject.groups list the proxy put on the subject (its order).
            let owned = self.player_groups.get(&subject.id).map(Vec::as_slice);
            for group in owned.into_iter().flatten().chain(subject.groups.iter()) {
                if let Some(grants) = self.group_grants.get(group) {
                    push_all(grants);
                }
            }
        }

        out
    }
}

impl PermissionProvider for ConfigPermissionProvider {
    fn grants_for(&self, subject: &Subject) -> Vec<Grant> {
        self.effective_grants(subject)
    }
}

/// Parses `nodes` under `key`, appending invalid entries to `errors`.
///
/// Valid grants are deduped per bucket (same node, same value) keeping the
/// first occurrence, so a node listed twice in one bucket is stored once.
fn collect_grants(key: &str, nodes: &[String], errors: &mut Vec<InvalidNode>) -> Vec<Grant> {
    let mut seen = HashSet::new();
    let mut grants = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        let input = GrantInput {
            group: key.to_string(),
            node: node.clone(),
        };
        match input.into_grant() {
            Ok(grant) => {
                if seen.insert((grant.node.clone(), grant.value)) {
                    grants.push(grant);
                }
            }
            Err(source) => errors.push(InvalidNode {
                key: key.to_string(),
                index,
                node: node.clone(),
                source,
            }),
        }
    }
    grants
}

/// Dedupes a group-name list, keeping first occurrence order.
fn deduped_strings(strings: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    strings
        .into_iter()
        .filter(|s| seen.insert(s.clone()))
        .collect()
}
