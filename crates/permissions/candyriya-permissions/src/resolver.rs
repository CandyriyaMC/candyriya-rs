//! Resolution: turning grants into a decision.
//!
//! This module documents the matching rules **as implemented** — these are the
//! crux of the engine, so they are spelled out in full and exercised by the
//! integration tests in `tests/`.
//!
//! ## Effective grant list
//!
//! The resolver resolves **only the flat list of grants it is given**. It never
//! expands nested groups; flattening a subject's group tree (subject's own grants
//! ∪ grants of every group the subject holds, transitively) is the provider's
//! concern. [`crate::InMemoryProvider::effective_grants`] handles the one-level
//! flat case and is what tests and the config provider use today.
//!
//! ## Applicability
//!
//! A grant `g` is **applicable** to node `N` iff both hold:
//!
//! 1. `N` is matched by `g.node`:
//!    - `*` matches any node;
//!    - an exact string match matches;
//!    - a trailing-wildcard pattern `prefix.*` matches any `N` that starts with
//!      `prefix.` and has at least one more segment. `foo.*` matches `foo.bar`
//!      and `foo.bar.baz`, never `foo` itself;
//!    - anything else does not match.
//! 2. Context subset match: for **every** context on the grant, the subject has
//!    a context with the same name and the same value. A grant with no contexts
//!    applies everywhere.
//!
//! ## Precedence
//!
//! Applicable grants produce candidate decisions; the winner is chosen as follows:
//!
//! 1. **Specificity.** Each candidate's specificity is the number of *literal*
//!    dot-separated segments in its pattern (a trailing wildcard counts as
//!    zero). The highest specificity wins. A longer pattern therefore always
//!    beats a shorter one, regardless of allow/deny and regardless of where the
//!    grant came from (subject's own grants, group grants — it is all one flat
//!    list at this point).
//! 2. **Same-length tie.** Among the top-length candidates, an explicit **deny
//!    wins** over an allow. If every top candidate allows, the result is allow.
//! 3. **No candidates.** Fall back to the registered default for `N`
//!    ([`PermissionRegistry::default_value`]); if the node is not registered,
//!    the result is **deny** (default-deny).
//!
//! Two consequences worth stating out loud:
//!
//! - Specificity outranks allow/deny: a 3-segment allow beats a 2-segment deny,
//!   and a 3-segment deny beats a 2-segment allow. Longer always wins.
//! - Same-pattern denies in a group shadow same-pattern allows in another group
//!   and on the subject itself (rule 2); nothing special is needed, and none is
//!   implemented.
//!
//! Exact matches are not artificially preferred over wildcard matches of equal
//! literal length — ties at the top length go to deny (rule 2), period. For a
//! `*`-free query node (the normal case), two distinct applicable patterns of
//! equal specificity are always identical strings, so ties arise from the same
//! pattern granted by different sources (subject vs groups). A query node that
//! itself contains `*` (legal at the type level) can see cross-pattern ties;
//! rule 2 handles them the same way — deny wins, and the reported
//! [`BestMatch`] is the first grant in provider order with the winning value.
//!
//! Case-sensitivity: nodes are lowercase by construction ([`PermissionNode`]),
//! and matching is case-sensitive. Two nodes that differ only by case can never
//! both be valid, so the question cannot arise on validated input.

use std::sync::Arc;

use crate::context::Subject;
use crate::grant::{Grant, PermissionValue};
use crate::node::PermissionNode;
use crate::registry::PermissionRegistry;

/// The grant that decided a resolution.
///
/// Present when at least one grant was applicable; `None` when the decision fell
/// back to the registry default or to default-deny. The winner is an
/// arbitrary-but-deterministic representative: the first grant in the flattened
/// list (the order the provider produced it) whose value equals the winning
/// value at the winning specificity. Use it for debugging and audits, not for
/// priority logic — its identity carries no resolution semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BestMatch {
    /// The grant that won.
    pub grant: Grant,
}

/// The outcome of resolving a node against a subject's grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Whether the subject may use the node.
    pub allowed: bool,
    /// The winning grant, if any; `None` when the registry default or
    /// default-deny decided.
    pub best: Option<BestMatch>,
}

/// The permission engine's decision maker.
///
/// Holds only a shared registry for default lookups; grants are supplied per
/// call as a flat [`Grant`] slice, e.g. from [`crate::InMemoryProvider::effective_grants`].
/// This keeps the resolver free of provider-specific storage concerns and makes
/// resolution a pure function apart from the registry fallback.
#[derive(Debug)]
pub struct PermissionResolver {
    registry: Arc<PermissionRegistry>,
}

impl PermissionResolver {
    /// Creates a resolver that falls back to `registry` for unmatched nodes.
    pub fn new(registry: Arc<PermissionRegistry>) -> Self {
        Self { registry }
    }

    /// Resolves `node` for `subject` against the flat `grants` list.
    ///
    /// See the module docs for the full precedence rules. `grants` must already
    /// be the effective list (subject grants ∪ group grants); order only
    /// influences which grant is reported as [`BestMatch::grant`], never the
    /// decision itself.
    pub fn resolve(
        &self,
        subject: &Subject,
        node: &PermissionNode,
        grants: &[Grant],
    ) -> Resolution {
        let winning_specificity = grants
            .iter()
            .filter(|grant| grant.node.matches(node) && contexts_match(grant, subject))
            .map(|grant| grant.node.specificity())
            .max();

        let Some(winning_specificity) = winning_specificity else {
            let allowed = self
                .registry
                .default_value(node)
                .is_some_and(PermissionValue::is_allow);
            return Resolution {
                allowed,
                best: None,
            };
        };

        let top: Vec<&Grant> = grants
            .iter()
            .filter(|grant| {
                grant.node.specificity() == winning_specificity
                    && grant.node.matches(node)
                    && contexts_match(grant, subject)
            })
            .collect();
        // Rule 2: at the top length, an explicit deny wins over allow.
        let allowed = top.iter().all(|grant| grant.value != PermissionValue::Deny);
        let winner_value = if allowed {
            PermissionValue::Allow
        } else {
            PermissionValue::Deny
        };
        // Deterministic representative: first grant in provider order with the winning value.
        let best = top
            .into_iter()
            .find(|grant| grant.value == winner_value)
            .cloned()
            .map(|grant| BestMatch { grant });

        Resolution { allowed, best }
    }

    /// Returns `true` iff the subject is allowed the node.
    ///
    /// Shorthand for `self.resolve(subject, node, grants).allowed`.
    pub fn has(&self, subject: &Subject, node: &PermissionNode, grants: &[Grant]) -> bool {
        self.resolve(subject, node, grants).allowed
    }
}

/// Subset context match: every grant context must be present on the subject.
fn contexts_match(grant: &Grant, subject: &Subject) -> bool {
    grant.contexts.iter().all(|context| {
        subject
            .contexts
            .get(context.name())
            .is_some_and(|value| value == context.value())
    })
}
