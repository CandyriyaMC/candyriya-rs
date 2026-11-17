//! Our parsed view of an invocation: who invoked, and the raw arguments.
//!
//! This is deliberately **not** Azalea Brigadier's
//! `azalea_brigadier::context::CommandContext` — that is the full parse tree
//! handlers execute against. This one is the small, audit-friendly pair
//! `{origin, args_raw}` the proxy uses for logging and permission decisions
//! without depending on Brigadier internals.
//!
//! Server switch data and other resolution results do **not** live here yet;
//! when the proxy needs them, they get their own type next to this one.

use crate::source::CommandOrigin;

/// A parsed invocation: who invoked the command, and the raw argument text.
///
/// `args_raw` is everything after the root literal, verbatim — quoting, case
/// and spacing preserved — with leading whitespace stripped. It is *not* the
/// parsed argument list; handlers read typed arguments from Brigadier's own
/// `CommandContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandContext {
    origin: CommandOrigin,
    args_raw: String,
}

impl CommandContext {
    /// Builds the invocation view from the parts.
    pub fn new(origin: CommandOrigin, args_raw: impl Into<String>) -> Self {
        Self {
            origin,
            args_raw: args_raw.into(),
        }
    }

    /// Builds the invocation view from an Azalea Brigadier parse context.
    ///
    /// Extracts the origin from the source and the argument substring from the
    /// (already slash-stripped) input: everything after the first run of
    /// whitespace following the root literal. For input `server lobby`, this
    /// yields `lobby`; for `foo`, it yields the empty string.
    pub fn from_brigadier(
        ctx: &azalea_brigadier::context::CommandContext<crate::source::CommandSource>,
    ) -> Self {
        Self::new(ctx.source.origin().clone(), raw_arguments(ctx.input()))
    }

    /// Who invoked the command.
    pub fn origin(&self) -> &CommandOrigin {
        &self.origin
    }

    /// The raw argument substring, verbatim (empty when there is no argument).
    pub fn args_raw(&self) -> &str {
        &self.args_raw
    }
}

fn raw_arguments(input: &str) -> &str {
    match input.find(char::is_whitespace) {
        Some(start) => input[start..].trim_start(),
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_arguments_extraction() {
        assert_eq!(raw_arguments("server lobby"), "lobby");
        assert_eq!(
            raw_arguments("server   lobby   skyblock"),
            "lobby   skyblock"
        );
        assert_eq!(raw_arguments("server"), "");
        assert_eq!(raw_arguments(""), "");
        // quoting and case are preserved
        assert_eq!(
            raw_arguments(r#"tell Notch \"hi there\""#),
            r#"Notch \"hi there\""#
        );
    }
}
