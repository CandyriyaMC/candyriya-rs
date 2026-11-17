//! End-to-end dispatcher tests: parse, execute, permissions and completions.
//!
//! Pure parsing tests, no network, no servers. Test-only commands live at the
//! bottom of the file.

use std::sync::{Arc, Mutex};

use azalea_brigadier::builder::argument_builder::ArgumentBuilder;
use azalea_brigadier::context::CommandContext as BrigadierContext;
use azalea_brigadier::errors::BuiltInError;
use azalea_brigadier::prelude::*;
use azalea_brigadier::suggestion::{Suggestions, SuggestionsBuilder};

use candyriya_command::{
    CandyCommand, CommandContext, CommandError, CommandSource, Dispatcher, PlayerRef, SourceKind,
    argument_with_suggestions, build,
};
use candyriya_permissions::{Grant, PermissionNode};

fn uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("b50ad385-829d-3141-a216-7e7d7539ba7f").expect("fixed uuid parses")
}

#[test]
fn build_on_empty_dispatcher_errors() {
    assert!(build(&[]).is_ok(), "empty slice builds");
    let dispatcher = Dispatcher::new(&[]).expect("empty slice builds");
    let err = dispatcher
        .execute("/foo", CommandSource::console())
        .expect_err("no commands registered");
    let CommandError::Brigadier(syntax) = err else {
        panic!("expected Brigadier error, got {err:?}");
    };
    assert_eq!(syntax.kind(), &BuiltInError::DispatcherUnknownCommand);
}

#[test]
fn dispatch_simple_command() {
    let dispatcher = Dispatcher::new(&[&FooCommand]).expect("builds");
    let result = dispatcher
        .execute("/foo", CommandSource::console())
        .expect("dispatch succeeds");
    assert_eq!(result, 42);
    // without the slash also works
    let result = dispatcher
        .execute("foo", CommandSource::console())
        .expect("dispatch succeeds");
    assert_eq!(result, 42);
}

#[test]
fn duplicate_names_are_rejected() {
    let Err(err) = build(&[&FooCommand, &FooCommand]) else {
        panic!("duplicate must fail");
    };
    assert!(matches!(err, CommandError::DuplicateName(name) if name == "foo"));
}

#[test]
fn invalid_root_literal_is_rejected() {
    let Err(err) = build(&[&BadNameCommand]) else {
        panic!("invalid name must fail");
    };
    assert!(matches!(err, CommandError::InvalidName { .. }));
}

#[test]
fn invalid_permission_node_is_rejected() {
    let Err(err) = build(&[&BadPermissionCommand]) else {
        panic!("invalid permission node must fail");
    };
    assert!(matches!(err, CommandError::InvalidPermissionNode { .. }));
}

#[test]
fn console_can_execute_permissioned_command() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Dispatcher::new(&[&ObservedServerCommand { captured }]).expect("builds");
    // console holds the `*` grant, so the command is visible
    let result = dispatcher
        .execute("/server lobby", CommandSource::console())
        .expect("console may execute");
    assert_eq!(result, 7);
}

#[test]
fn deny_all_player_cannot_see_or_run_permissioned_command() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Dispatcher::new(&[&ObservedServerCommand {
        captured: captured.clone(),
    }])
    .expect("builds");
    let player = CommandSource::player(PlayerRef::new(uuid(), "Notch")); // deny-all
    let err = dispatcher
        .execute("/server lobby", player)
        .expect_err("player without grants must not run");
    let CommandError::Brigadier(syntax) = err else {
        panic!("expected Brigadier error, got {err:?}");
    };
    // a command whose requirement fails is invisible to the parser
    assert_eq!(syntax.kind(), &BuiltInError::DispatcherUnknownCommand);
    assert!(captured.lock().expect("lock").is_empty());
}

#[test]
fn player_with_grant_can_execute() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Dispatcher::new(&[&ObservedServerCommand {
        captured: captured.clone(),
    }])
    .expect("builds");
    let player = CommandSource::player(PlayerRef::new(uuid(), "Notch")).with_grant(Grant::allow(
        PermissionNode::new("candyriya.command.server").expect("valid node"),
    ));
    let result = dispatcher
        .execute("/server lobby", player)
        .expect("granted player may execute");
    assert_eq!(result, 7);
}

#[test]
fn more_specific_deny_beats_wildcard_allow() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Dispatcher::new(&[&ObservedServerCommand {
        captured: captured.clone(),
    }])
    .expect("builds");
    // engine rule: specificity wins regardless of allow/deny, so the 3-segment
    // deny shadows the 0-segment allow — the command is invisible to the parser
    let player = CommandSource::player(PlayerRef::new(uuid(), "Notch"))
        .with_grant(Grant::allow(PermissionNode::new("*").expect("valid node")))
        .with_grant(Grant::deny(
            PermissionNode::new("candyriya.command.server").expect("valid node"),
        ));
    let err = dispatcher
        .execute("/server lobby", player)
        .expect_err("more specific deny must win");
    let CommandError::Brigadier(syntax) = err else {
        panic!("expected Brigadier error, got {err:?}");
    };
    assert_eq!(syntax.kind(), &BuiltInError::DispatcherUnknownCommand);
}

#[test]
fn argument_parsing_end_to_end() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Dispatcher::new(&[&ObservedServerCommand {
        captured: captured.clone(),
    }])
    .expect("builds");
    dispatcher
        .execute("/server lobby", CommandSource::console())
        .expect("executes");
    let captured = captured.lock().expect("lock");
    assert_eq!(captured.len(), 1);
    let (origin_kind, args_raw, target) = &captured[0];
    assert_eq!(*origin_kind, SourceKind::Console);
    assert_eq!(args_raw, "lobby");
    assert_eq!(target, "lobby");
}

#[test]
fn command_without_arguments_reports_empty_args_raw() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Dispatcher::new(&[&ObservedFooCommand {
        captured: captured.clone(),
    }])
    .expect("builds");
    dispatcher
        .execute("/foo", CommandSource::console())
        .expect("executes");
    let captured = captured.lock().expect("lock");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0], (SourceKind::Console, String::new()));
}

#[test]
fn missing_argument_is_an_error() {
    let dispatcher = Dispatcher::new(&[&ServerCommand]).expect("builds");
    let err = dispatcher
        .execute("/server", CommandSource::console())
        .expect_err("argument is required");
    assert!(matches!(err, CommandError::Brigadier(_)));
}

#[test]
fn completions_suggest_server_names() {
    let dispatcher = Dispatcher::new(&[&ServerCommand]).expect("builds");
    let suggestions = dispatcher.completions("/server ", CommandSource::console());
    let texts: Vec<String> = suggestions.list().iter().map(|s| s.text()).collect();
    assert!(texts.contains(&"lobby".to_owned()), "{texts:?}");
    assert!(texts.contains(&"pvp".to_owned()), "{texts:?}");
    assert!(texts.contains(&"skyblock".to_owned()), "{texts:?}");
}

#[test]
fn completions_filter_by_typed_prefix() {
    let dispatcher = Dispatcher::new(&[&ServerCommand]).expect("builds");
    let suggestions = dispatcher.completions("/server sk", CommandSource::console());
    let texts: Vec<String> = suggestions.list().iter().map(|s| s.text()).collect();
    assert_eq!(texts, vec!["skyblock".to_owned()]);
}

#[test]
fn command_context_from_brigadier_copies_player_origin() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Dispatcher::new(&[&ObservedFooCommand {
        captured: captured.clone(),
    }])
    .expect("builds");
    // foo carries no permission gate, so a deny-all player may execute it
    let player = CommandSource::player(PlayerRef::new(uuid(), "Notch"));
    dispatcher
        .execute("/foo", player)
        .expect("ungated command executes");
    let captured = captured.lock().expect("lock");
    assert_eq!(captured[0].0, SourceKind::Player);
}

// --- test commands (never shipped) ---

struct FooCommand;
impl CandyCommand for FooCommand {
    fn name(&self) -> &str {
        "foo"
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        lit.executes(|_| 42)
    }
}

struct ObservedFooCommand {
    captured: Arc<Mutex<Vec<(SourceKind, String)>>>,
}
impl CandyCommand for ObservedFooCommand {
    fn name(&self) -> &str {
        "foo"
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        let captured = Arc::clone(&self.captured);
        lit.executes(move |ctx| {
            let ours = CommandContext::from_brigadier(ctx);
            captured
                .lock()
                .expect("lock")
                .push((ours.origin().kind(), ours.args_raw().to_owned()));
            42
        })
    }
}

struct ServerCommand;
impl CandyCommand for ServerCommand {
    fn name(&self) -> &str {
        "server"
    }

    fn permission(&self) -> Option<&str> {
        Some("candyriya.command.server")
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        lit.then(argument_with_suggestions("target", word(), ServerNames).executes(|_| 7))
    }
}

struct ObservedServerCommand {
    captured: Arc<Mutex<Vec<(SourceKind, String, String)>>>,
}
impl CandyCommand for ObservedServerCommand {
    fn name(&self) -> &str {
        "server"
    }

    fn permission(&self) -> Option<&str> {
        Some("candyriya.command.server")
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        let captured = Arc::clone(&self.captured);
        lit.then(
            argument_with_suggestions("target", word(), ServerNames).executes(move |ctx| {
                let ours = CommandContext::from_brigadier(ctx);
                let target = get_string(ctx, "target").expect("target parsed");
                captured.lock().expect("lock").push((
                    ours.origin().kind(),
                    ours.args_raw().to_owned(),
                    target,
                ));
                7
            }),
        )
    }
}

struct BadNameCommand;
impl CandyCommand for BadNameCommand {
    fn name(&self) -> &str {
        "/server"
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        lit.executes(|_| 1)
    }
}

struct BadPermissionCommand;
impl CandyCommand for BadPermissionCommand {
    fn name(&self) -> &str {
        "badperm"
    }

    fn permission(&self) -> Option<&str> {
        Some("NOT A VALID NODE!")
    }

    fn build(&self, lit: ArgumentBuilder<CommandSource>) -> ArgumentBuilder<CommandSource> {
        lit.executes(|_| 1)
    }
}

struct ServerNames;
impl azalea_brigadier::suggestion::SuggestionProvider<CommandSource> for ServerNames {
    fn get_suggestions(
        &self,
        _context: BrigadierContext<CommandSource>,
        builder: SuggestionsBuilder,
    ) -> Suggestions {
        let remaining = builder.remaining_lowercase().to_owned();
        ["lobby", "pvp", "skyblock"]
            .into_iter()
            .filter(|name| name.starts_with(remaining.as_str()))
            .fold(builder, |builder, name| builder.suggest(name))
            .build()
    }
}
