//! Player commands integration tests: registration, permissions, dispatch and
//! output shape.
//!
//! Everything here runs without network or I/O. Commands write into a capture
//! sink that stands in for the proxy; tests assert on what was "sent".

use std::sync::{Arc, Mutex};

use azalea_brigadier::errors::BuiltInError;
use azalea_brigadier::suggestion::Suggestions;

use uuid::Uuid;

use candyriya_command::{
    CandyCommand, CommandError, CommandOrigin, CommandSource, CommandState, Dispatcher,
    NetworkDirectory, OnlinePlayer, PlayerRef, ProxyEffects, ServerInfo, SourceKind, SwitchRequest,
    SwitchTarget, player_commands,
};
use candyriya_permissions::{Grant, PermissionNode};

fn notch() -> PlayerRef {
    PlayerRef::new(Uuid::from_bytes([0x11; 16]), "Notch")
}
fn steve() -> PlayerRef {
    PlayerRef::new(Uuid::from_bytes([0x22; 16]), "Steve")
}
fn alex() -> PlayerRef {
    PlayerRef::new(Uuid::from_bytes([0x33; 16]), "Alex")
}

struct TestDirectory {
    servers: Vec<ServerInfo>,
    players: Vec<OnlinePlayer>,
}

impl NetworkDirectory for TestDirectory {
    fn servers(&self) -> Vec<ServerInfo> {
        self.servers.clone()
    }

    fn online_players(&self) -> Vec<OnlinePlayer> {
        self.players.clone()
    }
}

fn fixture_directory() -> TestDirectory {
    TestDirectory {
        servers: vec![
            ServerInfo::new("lobby", 0),
            ServerInfo::new("pvp", 1),
            ServerInfo::new("skyblock", 2),
        ],
        players: vec![
            OnlinePlayer::new(notch().uuid, "Notch", Some("lobby".to_owned())),
            OnlinePlayer::new(steve().uuid, "Steve", Some("lobby".to_owned())),
            OnlinePlayer::new(alex().uuid, "Alex", Some("pvp".to_owned())),
        ],
    }
}

#[derive(Default)]
struct Capture {
    messages: Mutex<Vec<(SourceKind, String)>>,
    switches: Mutex<Vec<SwitchRequest>>,
}

impl Capture {
    fn message_lines(&self) -> Vec<String> {
        self.messages
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, line)| line.clone())
            .collect()
    }

    fn take_switches(&self) -> Vec<SwitchRequest> {
        std::mem::take(&mut *self.switches.lock().expect("lock"))
    }
}

impl ProxyEffects for Capture {
    fn message(&self, origin: &CommandOrigin, message: String) {
        self.messages
            .lock()
            .expect("lock")
            .push((origin.kind(), message));
    }

    fn switch(&self, request: SwitchRequest) {
        self.switches.lock().expect("lock").push(request);
    }
}

fn fixture_state(capture: Arc<Capture>) -> CommandState {
    let i18n = candyriya_i18n::I18n::new("en").expect("embedded locales load");
    CommandState {
        i18n,
        directory: Arc::new(fixture_directory()),
        effects: capture,
    }
}

fn dispatcher(commands: Vec<Box<dyn CandyCommand>>) -> Dispatcher {
    let refs: Vec<&dyn CandyCommand> = commands.iter().map(Box::as_ref).collect();
    Dispatcher::new(&refs).expect("player commands build")
}

fn player_with_grant(node: &str) -> CommandSource {
    CommandSource::player(notch())
        .with_grant(Grant::allow(PermissionNode::new(node).expect("valid node")))
}

fn deny_all_player() -> CommandSource {
    CommandSource::player(notch())
}

#[test]
fn player_set_is_exactly_the_four_commands() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(capture);
    let commands = player_commands(&state);
    let names: Vec<&str> = commands.iter().map(|c| c.name()).collect();
    assert_eq!(names, ["glist", "send", "server", "goto"]);
    // and it builds cleanly into a dispatcher
    dispatcher(commands);
}

#[test]
fn help_and_version_do_not_exist() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(capture);
    let dispatcher = dispatcher(player_commands(&state));

    for input in ["/help", "help", "/version", "/msg Steve hi"] {
        let err = dispatcher
            .execute(input, deny_all_player())
            .expect_err("{input} must not resolve to a command");
        let CommandError::Brigadier(syntax) = err else {
            panic!("expected a brigadier error for {input}, got {err:?}");
        };
        assert_eq!(
            syntax.kind(),
            &BuiltInError::DispatcherUnknownCommand,
            "{input}"
        );
    }
}

#[test]
fn glist_prints_count_and_per_backend_lines() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(Arc::clone(&capture));
    let dispatcher = dispatcher(player_commands(&state));

    let player = player_with_grant("candyriya.command.glist");
    dispatcher
        .execute("/glist", player)
        .expect("glist dispatches");

    let lines = capture.message_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("players online:") && line.contains("3")),
        "total line missing: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("<green>lobby<reset>") && line.contains("2")),
        "lobby line missing: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("<gold>pvp<reset>") && line.contains("1")),
        "pvp line missing: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("<gray>skyblock<reset>") && line.contains("0")),
        "skyblock line missing: {lines:?}"
    );
}

#[test]
fn glist_denied_for_deny_all_player() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(capture);
    let dispatcher = dispatcher(player_commands(&state));

    let err = dispatcher
        .execute("/glist", deny_all_player())
        .expect_err("deny-all player must not see /glist");
    let CommandError::Brigadier(syntax) = err else {
        panic!("expected brigadier error, got {err:?}");
    };
    assert_eq!(syntax.kind(), &BuiltInError::DispatcherUnknownCommand);
}

#[test]
fn glist_on_empty_network_reports_zero() {
    let capture = Arc::new(Capture::default());
    let i18n = candyriya_i18n::I18n::new("en").expect("embedded locales load");
    let effects: Arc<dyn ProxyEffects> = capture.clone();
    let state = CommandState {
        i18n,
        directory: Arc::new(TestDirectory {
            servers: vec![ServerInfo::new("lobby", 0)],
            players: vec![],
        }),
        effects,
    };
    let dispatcher = dispatcher(player_commands(&state));

    let console = CommandSource::console();
    dispatcher
        .execute("/glist", console)
        .expect("console may run");
    let lines = capture.message_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("players online:") && line.contains("0")),
        "zero-count total line missing: {lines:?}"
    );
}

#[test]
fn goto_and_server_are_aliases_for_sender_switch() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(Arc::clone(&capture));
    let dispatcher = dispatcher(player_commands(&state));

    for input in ["/server lobby", "/goto lobby"] {
        dispatcher
            .execute(input, deny_all_player())
            .expect("{input} must dispatch without a grant");
    }

    let switches = capture.take_switches();
    assert_eq!(switches.len(), 2);
    for switch in &switches {
        assert_eq!(switch.server, "lobby");
        assert_eq!(
            switch.target,
            SwitchTarget::One(notch()),
            "alias must switch the invoking player"
        );
    }
    assert_eq!(switches[0], switches[1], "aliases must switch identically");
}

#[test]
fn server_self_switch_rejects_non_players() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(Arc::clone(&capture));
    let dispatcher = dispatcher(player_commands(&state));

    let console = CommandSource::console();
    let err = dispatcher
        .execute("/server lobby", console)
        .expect_err("console cannot switch itself");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert!(capture.take_switches().is_empty());
    let lines = capture.message_lines();
    assert!(
        lines.iter().any(|line| line.contains("Only players")),
        "only-players notice missing: {lines:?}"
    );
}

#[test]
fn send_with_unknown_server_is_rejected() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(Arc::clone(&capture));
    let dispatcher = dispatcher(player_commands(&state));

    let err = dispatcher
        .execute("/send Steve novather", CommandSource::console())
        .expect_err("unknown server must be rejected");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert!(
        capture.take_switches().is_empty(),
        "no switch may be applied for an unknown server"
    );
    let lines = capture.message_lines();
    assert!(
        lines.iter().any(|line| line.contains("Unknown server")),
        "unknown-server notice missing: {lines:?}"
    );
}

#[test]
fn send_named_and_all_targets_apply_switches() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(Arc::clone(&capture));
    let dispatcher = dispatcher(player_commands(&state));

    dispatcher
        .execute("/send Steve lobby", CommandSource::console())
        .expect("named target dispatches");
    dispatcher
        .execute("/send ALL pvp", CommandSource::console())
        .expect("ALL target dispatches");

    let switches = capture.take_switches();
    assert_eq!(switches.len(), 2);
    assert_eq!(switches[0].server, "lobby");
    assert_eq!(switches[0].target, SwitchTarget::One(steve()));
    assert_eq!(switches[1].server, "pvp");
    assert_eq!(switches[1].target, SwitchTarget::All);
}

#[test]
fn send_named_target_must_be_online() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(Arc::clone(&capture));
    let dispatcher = dispatcher(player_commands(&state));

    let err = dispatcher
        .execute("/send Ghost lobby", CommandSource::console())
        .expect_err("offline named target must be rejected");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert!(capture.take_switches().is_empty());
    let lines = capture.message_lines();
    assert!(
        lines.iter().any(|line| line.contains("not online")),
        "not-online notice missing: {lines:?}"
    );
}

#[test]
fn send_requires_gate_for_players() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(Arc::clone(&capture));
    let dispatcher = dispatcher(player_commands(&state));

    let err = dispatcher
        .execute("/send Steve lobby", deny_all_player())
        .expect_err("deny-all player must not run /send");
    let CommandError::Brigadier(syntax) = err else {
        panic!("expected brigadier error, got {err:?}");
    };
    assert_eq!(syntax.kind(), &BuiltInError::DispatcherUnknownCommand);

    // with the grant, the same invocation succeeds
    dispatcher
        .execute(
            "/send Steve lobby",
            player_with_grant("candyriya.command.send"),
        )
        .expect("granted player may run /send");
    assert_eq!(capture.take_switches().len(), 1);
}

#[test]
fn server_suggestions_list_configured_servers() {
    let capture = Arc::new(Capture::default());
    let state = fixture_state(Arc::clone(&capture));
    let dispatcher = dispatcher(player_commands(&state));

    let suggestions: Suggestions = dispatcher.completions("/server ", deny_all_player());
    let texts: Vec<String> = suggestions.list().iter().map(|s| s.text()).collect();
    assert!(texts.contains(&"lobby".to_owned()), "{texts:?}");
    assert!(texts.contains(&"pvp".to_owned()), "{texts:?}");
    assert!(texts.contains(&"skyblock".to_owned()), "{texts:?}");

    let suggestions = dispatcher.completions("/server sk", deny_all_player());
    let texts: Vec<String> = suggestions.list().iter().map(|s| s.text()).collect();
    assert_eq!(texts, vec!["skyblock".to_owned()]);
}
