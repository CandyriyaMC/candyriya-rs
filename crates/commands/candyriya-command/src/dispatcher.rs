//! The dispatcher: our thin adapter over Azalea Brigadier.
//!
//! Design: one thin adapter, no Java-style class hierarchy. Brigadier already
//! gives us literals, arguments, requirements and redirects; this module adds
//! exactly what the proxy needs on top:
//!
//! - [`build`] — a [`CommandDispatcher`] from a slice of [`CandyCommand`]s,
//!   with root-literal validation, duplicate-name rejection and optional
//!   permission gating on each command's root literal;
//! - [`Dispatcher`] — the concrete wrapper the proxy uses: dispatch, tab
//!   completion and access to the underlying Azalea Brigadier dispatcher.
//!   Commands are added one at a time with [`Dispatcher::register`], or all
//!   at once as an atomic batch with a crate-internal register call, when
//!   the tree is assembled by the built-in command registry.

use std::collections::HashSet;
use std::sync::Arc;

use azalea_brigadier::builder::literal_argument_builder::literal;
use azalea_brigadier::command_dispatcher::CommandDispatcher;
use azalea_brigadier::suggestion::Suggestions;

use candyriya_permissions::PermissionNode;

use crate::command::CandyCommand;
use crate::error::CommandError;
use crate::source::CommandSource;

/// Validates and builds a Brigadier dispatcher from command implementations.
///
/// For each command, in order:
///
/// 1. [`CandyCommand::name`] is validated (lowercase ASCII letter first, then
///    lowercase ASCII / digits / `_` / `-`, at most 64 bytes);
/// 2. duplicate names are rejected — Brigadier silently merges
///    same-name literals, which would hide registration bugs;
/// 3. when [`CandyCommand::permission`] is set, the node is validated against
///    the `candyriya-permissions` grammar and a `requires` clause is attached
///    to the command's root literal, so an unauthorized source cannot even
///    parse the command;
/// 4. the command assembles its subtree via [`CandyCommand::build`] and it is
///    registered on the dispatcher.
///
/// # Errors
///
/// Returns [`CommandError::InvalidName`], [`CommandError::DuplicateName`] or
/// [`CommandError::InvalidPermissionNode`] on the offending command. The
/// dispatcher is fully built or the error is returned; partial state never
/// escapes.
pub fn build(
    commands: &[&dyn CandyCommand],
) -> Result<CommandDispatcher<CommandSource>, CommandError> {
    let mut dispatcher = CommandDispatcher::new();
    register_commands(&mut dispatcher, commands)?;
    Ok(dispatcher)
}

/// Validates a batch of commands and registers them on `dispatcher`.
///
/// Validation (names, duplicates against both the batch and the existing
/// tree, permission nodes) happens for the whole batch before the tree is
/// mutated, so on error the dispatcher is left unchanged — incremental
/// registration is all-or-nothing just like [`build`].
pub(crate) fn register_commands(
    dispatcher: &mut CommandDispatcher<CommandSource>,
    commands: &[&dyn CandyCommand],
) -> Result<(), CommandError> {
    let mut seen = HashSet::with_capacity(commands.len());
    let mut builders = Vec::with_capacity(commands.len());
    for command in commands {
        let name = command.name();
        validate_command_name(name)?;
        if !seen.insert(name.to_owned()) {
            return Err(CommandError::DuplicateName(name.to_owned()));
        }
        // brigadier silently merges same-name literals — treat a pre-existing
        // root literal as a duplicate instead of letting the trees merge
        if dispatcher.find_node(&[name]).is_some() {
            return Err(CommandError::DuplicateName(name.to_owned()));
        }

        let mut lit = literal::<CommandSource>(name);
        if let Some(node) = command.permission() {
            let parsed =
                PermissionNode::new(node).map_err(|err| CommandError::InvalidPermissionNode {
                    command: name.to_owned(),
                    node: node.to_owned(),
                    reason: err.to_string(),
                })?;
            // capture the raw string for the closure; engine revalidates on
            // every check anyway (defense in depth, not a hot path)
            let raw = parsed.as_str().to_owned();
            lit = lit.requires(move |source: &CommandSource| source.has_permission(&raw));
        }
        builders.push(command.build(lit));
    }
    for builder in builders {
        dispatcher.register(builder);
    }
    Ok(())
}

/// A pre-dispatch hook for commands that are not part of the brigadier
/// tree at all.
///
/// Implementations see the slash-stripped input and the source before
/// parsing. Returning `Some(code)` means "handled" — the input never reaches
/// the command tree. Returning `None` passes the input through untouched.
/// The hidden command has no node in the tree, no permission requirement,
/// no suggestion entries and no usage listings; it is not discoverable
/// through tab completion, `get_all_usage` or `find_node`.
pub(crate) trait HiddenRunner: Send + Sync {
    /// Attempt to handle `input`. See [`Dispatcher::execute`].
    fn run(&self, input: &str, source: &CommandSource) -> Option<i32>;
}

/// The command dispatcher wrapper the proxy drives.
///
/// Construct with [`Dispatcher::new`]; dispatch with [`Dispatcher::execute`];
/// tab-complete with [`Dispatcher::completions`]. Commands are added one at a
/// time with [`Dispatcher::register`], or as an all-or-nothing batch when
/// the tree is assembled by the built-in command registry.
pub struct Dispatcher {
    inner: CommandDispatcher<CommandSource>,
    hidden: Option<Arc<dyn HiddenRunner>>,
}

impl Dispatcher {
    /// Builds the dispatcher from the given commands; see [`build`].
    ///
    /// # Errors
    ///
    /// See [`build`].
    pub fn new(commands: &[&dyn CandyCommand]) -> Result<Self, CommandError> {
        Ok(Self {
            inner: build(commands)?,
            hidden: None,
        })
    }

    /// An empty dispatcher to register commands onto one by one.
    ///
    /// Register with [`Dispatcher::register`]; completions and execution
    /// behave exactly as on a from-scratch build.
    pub fn empty() -> Self {
        Self {
            inner: CommandDispatcher::new(),
            hidden: None,
        }
    }

    /// Validates and registers a single command on this dispatcher.
    ///
    /// Same rules as [`build`]: name and permission-node validation, and a
    /// root literal already present in the tree is rejected as a duplicate
    /// instead of being silently merged.
    ///
    /// # Errors
    ///
    /// See [`build`].
    pub fn register(&mut self, command: &dyn CandyCommand) -> Result<(), CommandError> {
        register_commands(&mut self.inner, std::slice::from_ref(&command))
    }

    /// Validates and registers a batch of commands, all-or-nothing.
    ///
    /// The batch is validated before the tree is touched; on error this
    /// dispatcher is left exactly as it was.
    ///
    /// # Errors
    ///
    /// See [`build`].
    pub(crate) fn register_slice(
        &mut self,
        commands: &[&dyn CandyCommand],
    ) -> Result<(), CommandError> {
        register_commands(&mut self.inner, commands)
    }

    pub(crate) fn attach_hidden(&mut self, runner: impl HiddenRunner + 'static) {
        self.hidden = Some(Arc::new(runner));
    }

    /// Dispatches `input` as the given source.
    ///
    /// Accepts input with or without a leading `/`; exactly one leading slash
    /// is stripped before handing the input to Brigadier (Minecraft clients
    /// send the slash; Brigadier literals must not have it).
    ///
    /// Returns the command's return code (Brigadier semantics: number depends
    /// on the command and may not be meaningful).
    ///
    /// # Errors
    ///
    /// [`CommandError::Brigadier`] when the input does not parse, when no
    /// registered command matches, when the source fails a permission
    /// requirement, or when execution fails inside the command.
    pub fn execute(&self, input: &str, source: CommandSource) -> Result<i32, CommandError> {
        let input = strip_leading_slash(input);
        if let Some(hidden) = &self.hidden
            && let Some(code) = hidden.run(input, &source)
        {
            return Ok(code);
        }
        Ok(self.inner.execute(input, source)?)
    }

    /// Computes the tab-completion suggestions for `input` at the cursor.
    ///
    /// The returned suggestions replace the text range [`Suggestions::range`]
    /// describes. Input handling matches [`Dispatcher::execute`] (one leading
    /// slash is stripped).
    pub fn completions(&self, input: &str, source: CommandSource) -> Suggestions {
        let input = strip_leading_slash(input);
        let parse = self.inner.parse(input.into(), source);
        CommandDispatcher::get_completion_suggestions(parse)
    }

    /// The underlying Azalea Brigadier dispatcher.
    ///
    /// Useful for introspection (`get_all_usage`, `find_node`, ...); not meant
    /// for bypassing permission checks — those live on the nodes themselves.
    pub fn dispatcher(&self) -> &CommandDispatcher<CommandSource> {
        &self.inner
    }
}

fn strip_leading_slash(input: &str) -> &str {
    input.strip_prefix('/').unwrap_or(input)
}

/// Root literals must be unambiguous in Brigadier's parser: lowercase, no
/// spaces, at most one leading alphabetic character class.
fn validate_command_name(name: &str) -> Result<(), CommandError> {
    if name.is_empty() {
        return Err(CommandError::InvalidName {
            name: name.to_owned(),
            reason: "must not be empty",
        });
    }
    if name.len() > 64 {
        return Err(CommandError::InvalidName {
            name: name.to_owned(),
            reason: "must be at most 64 bytes",
        });
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_lowercase() {
        return Err(CommandError::InvalidName {
            name: name.to_owned(),
            reason: "must start with a lowercase ASCII letter",
        });
    }
    if let Some(bad) = name
        .chars()
        .find(|ch| !matches!(ch, 'a'..='z' | '0'..='9' | '_' | '-'))
    {
        return Err(CommandError::InvalidName {
            name: name.to_owned(),
            reason: if bad == '/' {
                "must not contain a slash (commands are dispatched with or without it)"
            } else {
                "must contain only lowercase ASCII letters, digits, '_' and '-'"
            },
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        for ok in ["server", "glist", "goto", "candyriya", "a", "srv-2_x"] {
            assert!(validate_command_name(ok).is_ok(), "{ok} must be valid");
        }
        for (name, reason) in [
            ("", "must not be empty"),
            ("Server", "must start with a lowercase ASCII letter"),
            ("2fast", "must start with a lowercase ASCII letter"),
            (
                "srv/x",
                "must not contain a slash (commands are dispatched with or without it)",
            ),
            (
                "my command",
                "must contain only lowercase ASCII letters, digits, '_' and '-'",
            ),
            (
                "srvöhl",
                "must contain only lowercase ASCII letters, digits, '_' and '-'",
            ),
            (&"x".repeat(65), "must be at most 64 bytes"),
        ] {
            match validate_command_name(name) {
                Err(CommandError::InvalidName { reason: actual, .. }) => {
                    assert_eq!(actual, reason, "{name:?}")
                }
                other => panic!("expected InvalidName for {name:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn slash_stripping() {
        assert_eq!(strip_leading_slash("/server"), "server");
        assert_eq!(strip_leading_slash("server"), "server");
        assert_eq!(strip_leading_slash("//server"), "/server");
    }
}
