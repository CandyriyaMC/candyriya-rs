//! Errors this crate produces.

use thiserror::Error;

/// Anything that can go wrong while building or running the dispatcher.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CommandError {
    /// A command's root literal name failed validation.
    #[error("invalid command name {name:?}: {reason}")]
    InvalidName {
        /// The offending name.
        name: String,
        /// Why it was rejected (static grammar explanation).
        reason: &'static str,
    },
    /// Two commands claimed the same root literal.
    #[error("duplicate command name {0:?}")]
    DuplicateName(String),
    /// A command declared a permission node the engine rejects.
    #[error("command {command:?} declares invalid permission node {node:?}: {reason}")]
    InvalidPermissionNode {
        /// The command declaring the node.
        command: String,
        /// The offending node, verbatim.
        node: String,
        /// The engine's rejection reason.
        reason: String,
    },
    /// Brigadier rejected the input or execution.
    #[error("brigadier rejected the command: {}", .0.message())]
    Brigadier(azalea_brigadier::errors::CommandSyntaxError),
}

impl From<azalea_brigadier::errors::CommandSyntaxError> for CommandError {
    fn from(err: azalea_brigadier::errors::CommandSyntaxError) -> Self {
        Self::Brigadier(err)
    }
}
