//! Permission registry: declared nodes and their defaults.
//!
//! The registry is where plugins (later) declare the nodes they understand, along
//! with the default value the engine falls back to when no grant applies and a
//! human-readable description for tooling. Registration is additive: committing a
//! duplicate node is an error, never a silent overwrite.
//!
//! Lookups by [`PermissionRegistry::default_value`] and [`PermissionRegistry::get`]
//! are **exact** — the registry does not interpret wildcards; wildcard semantics
//! live in grant matching, not in node documentation.

use std::collections::BTreeMap;

use parking_lot::Mutex;
use thiserror::Error;

use crate::grant::PermissionValue;
use crate::node::PermissionNode;

/// Errors returned by [`PermissionRegistry::register`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegistryError {
    /// A node was registered twice.
    #[error("permission node {0:?} is already registered")]
    DuplicateNode(String),
}

/// A node declared in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredNode {
    /// The declared node.
    pub node: PermissionNode,
    /// Value applied when no grant matches this node.
    pub default_value: PermissionValue,
    /// Human-readable description for tooling and docs.
    pub description: String,
    /// Where the registration came from (plugin id, config path, ...).
    pub source: Option<String>,
}

/// Registry of permission nodes and their defaults.
///
/// Cheap to share: pass an `Arc<PermissionRegistry>` to
/// [`crate::PermissionResolver::new`]. Registration happens through `&self` and is
/// safe for use after the `Arc` exists, so plugins can declare nodes long after
/// the resolver is built.
#[derive(Debug, Default)]
pub struct PermissionRegistry {
    nodes: Mutex<BTreeMap<PermissionNode, RegisteredNode>>,
}

impl PermissionRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a node with a default value, description and optional source.
    ///
    /// Returns [`RegistryError::DuplicateNode`] if the node is already registered.
    pub fn register(
        &self,
        node: PermissionNode,
        default_value: PermissionValue,
        description: impl Into<String>,
        source: Option<String>,
    ) -> Result<(), RegistryError> {
        let mut nodes = self.nodes.lock();
        if nodes.contains_key(&node) {
            return Err(RegistryError::DuplicateNode(node.as_str().to_owned()));
        }
        nodes.insert(
            node.clone(),
            RegisteredNode {
                node,
                default_value,
                description: description.into(),
                source,
            },
        );
        Ok(())
    }

    /// Returns the registered default for `node`, if any (exact match).
    pub fn default_value(&self, node: &PermissionNode) -> Option<PermissionValue> {
        self.nodes
            .lock()
            .get(node)
            .map(|registered| registered.default_value)
    }

    /// Returns the registered entry for `node`, if any (exact match).
    pub fn get(&self, node: &PermissionNode) -> Option<RegisteredNode> {
        self.nodes.lock().get(node).cloned()
    }

    /// Whether `node` is registered (exact match).
    pub fn is_registered(&self, node: &PermissionNode) -> bool {
        self.nodes.lock().contains_key(node)
    }

    /// Number of registered nodes.
    pub fn len(&self) -> usize {
        self.nodes.lock().len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.lock().is_empty()
    }
}
