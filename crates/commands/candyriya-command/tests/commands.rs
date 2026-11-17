//! End-to-end built-in command tests: parse, execute, permissions, output
//! capture, tab completion. No network, no real proxy — the directory and
//! the effects sink are deterministic fixtures.
//!
//! Nothing in this file enumerates registered commands and nothing may add
//! such a test later: an enumeration would hand list parties a handle on the
//! internal-only command this crate registers.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use azalea_brigadier::errors::BuiltInError;

use uuid::Uuid;

use candyriya_command::{
    BackendSnapshot, BackendState, CommandError, CommandRegistry, CommandSource, CommandState,
    ConnectionStats, Dispatcher, Locales, NetworkDirectory, OnlinePlayer, PlayerRef, ProxyEffects,
    ProxySnapshot, ServerInfo, ShutdownHandle, SwitchRequest, SwitchResult,
};
use candyriya_i18n::I18n;
use candyriya_permissions::{Grant, PermissionNode};

fn uuid_of(n: u8) -> Uuid {
    Uuid::from_bytes([n; 16])
}

fn notch() -> PlayerRef {
    PlayerRef::new(uuid_of(0x11), "Notch")
}
fn steve() -> PlayerRef {
    PlayerRef::new(uuid_of(0x22), "Steve")
}
fn alex() -> PlayerRef {
    PlayerRef::new(uuid_of(0x33), "Alex")
}

fn player(name: &str, server: &str, n: u8) -> OnlinePlayer {
    OnlinePlayer::new(uuid_of(n), name, Some(server.to_owned()))
}

fn backend(name: &str, state: BackendState, players: u32, ping_ms: Option<u64>) -> BackendSnapshot {
    BackendSnapshot {
        name: name.to_owned(),
        address: format!("{name}.internal:25565"),
        state,
        players,
        max_players: 20,
        ping: ping_ms.map(Duration::from_millis),
    }
}

fn default_backends() -> Vec<BackendSnapshot> {
    vec![
        backend("lobby", BackendState::Online, 10, Some(42)),
        backend("pvp", BackendState::Offline, 0, None),
    ]
}

struct TestDirectory {
    servers: Vec<ServerInfo>,
    players: Vec<OnlinePlayer>,
    backends: Vec<BackendSnapshot>,
}

impl NetworkDirectory for TestDirectory {
    fn servers(&self) -> Vec<ServerInfo> {
        self.servers.clone()
    }

    fn online_players(&self) -> Vec<OnlinePlayer> {
        self.players.clone()
    }

    fn snapshot(&self) -> ProxySnapshot {
        ProxySnapshot {
            version: "9.9.9-test".to_owned(),
            uptime: Duration::from_secs(3723), // 1h 2m 3s
            config_name: "test.toml".to_owned(),
            effective_log_level: "debug".to_owned(),
            connections: ConnectionStats {
                active: 3,
                accepted: 42,
                rejected: 7,
            },
            players: self.players.clone(),
            backends: self.backends.clone(),
        }
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
        backends: default_backends(),
    }
}

#[derive(Default)]
struct Capture {
    lines: Mutex<Vec<String>>,
    switches: Mutex<Vec<SwitchRequest>>,
    behavior: Option<SwitchBehavior>,
}

type SwitchBehavior = Box<dyn Fn(&SwitchRequest) -> SwitchResult + Send + Sync>;

impl Capture {
    fn switched() -> Self {
        Self {
            behavior: Some(Box::new(|request| SwitchResult::Switched {
                player: request.player.name.clone(),
                server: request.server.clone(),
            })),
            ..Self::default()
        }
    }

    fn fail_for(name: &'static str, reason: &'static str) -> Self {
        Self {
            behavior: Some(Box::new(move |request| {
                if request.player.name == name {
                    SwitchResult::Failed {
                        player: request.player.name.clone(),
                        server: request.server.clone(),
                        reason: reason.to_owned(),
                    }
                } else {
                    SwitchResult::Switched {
                        player: request.player.name.clone(),
                        server: request.server.clone(),
                    }
                }
            })),
            ..Self::default()
        }
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("lines lock").clone()
    }

    fn take_switches(&self) -> Vec<SwitchRequest> {
        std::mem::take(&mut *self.switches.lock().expect("switches lock"))
    }
}

impl candyriya_command::OutputSender for Capture {
    fn send(&self, _source: &CommandSource, line: &str) {
        self.lines.lock().expect("lines lock").push(line.to_owned());
    }
}

impl ProxyEffects for Capture {
    fn switch(&self, request: SwitchRequest) -> SwitchResult {
        self.switches
            .lock()
            .expect("switches lock")
            .push(request.clone());
        match &self.behavior {
            Some(behavior) => behavior(&request),
            None => SwitchResult::Switched {
                player: request.player.name,
                server: request.server,
            },
        }
    }
}

struct Fixture {
    dispatcher: Dispatcher,
    capture: Arc<Capture>,
    shutdown: ShutdownHandle,
}

impl Fixture {
    fn lines(&self) -> Vec<String> {
        self.capture.lines()
    }
}

fn fixture() -> Fixture {
    fixture_with(fixture_directory(), Capture::switched())
}

fn fixture_with(directory: TestDirectory, capture: Capture) -> Fixture {
    let capture = Arc::new(capture);
    let shutdown = ShutdownHandle::new();
    let locales = Locales::new(
        I18n::new("en").expect("embedded en locale"),
        I18n::new("ex_girl").expect("embedded ex_girl locale"),
    );
    let effects: Arc<dyn ProxyEffects> = capture.clone();
    let state = CommandState {
        locales,
        directory: Arc::new(directory),
        effects,
    };
    let registry = CommandRegistry::new(state, shutdown.clone());
    let dispatcher = registry.dispatcher().expect("built-in commands build");
    Fixture {
        dispatcher,
        capture,
        shutdown,
    }
}

fn standard() -> Fixture {
    fixture()
}

fn player_with(grants: &[&str]) -> CommandSource {
    let mut source = CommandSource::player(notch());
    for node in grants {
        source = source.with_grant(Grant::allow(
            PermissionNode::new(node).expect("valid permission node"),
        ));
    }
    source
}

fn deny_all_player() -> CommandSource {
    player_with(&[])
}

fn completion_texts(dispatcher: &Dispatcher, input: &str, source: CommandSource) -> Vec<String> {
    dispatcher
        .completions(input, source)
        .list()
        .iter()
        .map(|suggestion| suggestion.text())
        .collect()
}

// --- command surface ---

#[test]
fn help_and_version_do_not_exist() {
    let fixture = standard();

    for input in [
        "/help",
        "help",
        "/version",
        "/msg Steve hi",
        "/kick Notch",
        "/discord",
        "/link",
        "/report",
        "/stop",
    ] {
        let err = fixture
            .dispatcher
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
    // the root exists but has no such subcommand; brigadier reports it as
    // an argument mismatch rather than an unknown command — either way it
    // must not run
    let err = fixture
        .dispatcher
        .execute("/candyriya stop", deny_all_player())
        .expect_err("/candyriya stop must not resolve");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert!(fixture.lines().is_empty());
}

#[test]
fn stop_alone_does_not_shut_anything_down() {
    let fixture = standard();
    let _ = fixture
        .dispatcher
        .execute("/stop", CommandSource::console());
    assert!(!fixture.shutdown.is_requested());
}

// --- /candyriya ---

#[test]
fn info_prints_proxy_state() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya info", CommandSource::console())
        .expect("info executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("9.9.9-test"), "version in {joined}");
    assert!(joined.contains("1h 2m"), "uptime in {joined}");
    assert!(joined.contains("test.toml"), "config in {joined}");
    assert!(joined.contains("debug"), "log level in {joined}");
    assert!(
        !joined.contains("активных"),
        "must not leak the other locale: {joined}"
    );
    assert!(joined.contains('3'), "active connection count in {joined}");
    assert!(joined.contains("42"), "accepted count in {joined}");
    assert!(joined.contains('7'), "rejected count in {joined}");
    assert!(joined.contains("lobby"), "backend in {joined}");
    assert!(joined.contains("pvp"), "backend in {joined}");
    assert!(joined.contains("42 ms"), "ping in {joined}");
    assert!(joined.contains("10/20"), "backend players in {joined}");
}

#[test]
fn info_without_backends_says_so() {
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![],
            players: vec![],
            backends: vec![],
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/candyriya info", CommandSource::console())
        .expect("info executes");
    assert_eq!(code, 1);
    assert!(
        fixture
            .lines()
            .iter()
            .any(|line| line.contains("No backends configured."))
    );
}

#[test]
fn info_denied_for_deny_all_player() {
    let fixture = standard();
    let err = fixture
        .dispatcher
        .execute("/candyriya info", deny_all_player())
        .expect_err("deny-all player cannot run info");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert!(fixture.lines().is_empty());
}

#[test]
fn end_triggers_shutdown_flag_and_reports_already() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya end", CommandSource::console())
        .expect("end executes");
    assert_eq!(code, 1);
    assert!(fixture.shutdown.is_requested());
    assert!(
        fixture.lines().join("\n").contains("Shutting down"),
        "shutdown line, got {:?}",
        fixture.lines()
    );

    let code = fixture
        .dispatcher
        .execute("/candyriya end", CommandSource::console())
        .expect("second end is answered, not an error");
    assert_eq!(code, 0);
    assert!(
        fixture
            .lines()
            .last()
            .expect("second line printed")
            .contains("already in progress")
    );
}

#[test]
fn end_requires_permission_and_leaves_the_flag_alone() {
    let fixture = standard();
    let err = fixture
        .dispatcher
        .execute("/candyriya end", deny_all_player())
        .expect_err("deny-all player cannot end");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert!(!fixture.shutdown.is_requested());
}

#[test]
fn bare_candyriya_is_not_a_command() {
    let fixture = standard();
    let err = fixture
        .dispatcher
        .execute("/candyriya", CommandSource::console())
        .expect_err("the root without a subcommand is not executable");
    assert!(matches!(err, CommandError::Brigadier(_)));
}

#[test]
fn servers_table_marks_state_and_counts() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya servers", CommandSource::console())
        .expect("servers executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("lobby"));
    assert!(joined.contains("pvp"));
    assert!(joined.contains("online"));
    assert!(joined.contains("offline"));
    assert!(joined.contains("10/20"));
    assert!(joined.contains("42 ms"));
}

#[test]
fn servers_table_without_servers() {
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![],
            players: vec![],
            backends: vec![],
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/candyriya servers", CommandSource::console())
        .expect("servers executes");
    assert_eq!(code, 1);
    assert!(
        fixture
            .lines()
            .iter()
            .any(|line| line.contains("No servers configured."))
    );
}

#[test]
fn servers_denied_for_deny_all_player() {
    let fixture = standard();
    let err = fixture
        .dispatcher
        .execute("/candyriya servers", deny_all_player())
        .expect_err("deny-all player cannot list servers");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert!(fixture.lines().is_empty());
}

#[test]
fn switch_moves_target_player() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya switch Steve lobby", CommandSource::console())
        .expect("switch executes");
    assert_eq!(code, 1);
    let switches = fixture.capture.take_switches();
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].player.name, "Steve");
    assert_eq!(switches[0].server, "lobby");
    assert!(fixture.lines().join("\n").contains("Steve"));
}

#[test]
fn switch_unknown_server_lists_known_servers() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya switch Notch nope", CommandSource::console())
        .expect("switch is answered");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("nope"), "bad name echoed in {joined}");
    assert!(joined.contains("lobby"), "known list in {joined}");
    assert!(joined.contains("pvp"), "known list in {joined}");
}

#[test]
fn switch_offline_target_rejected() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya switch Ghost lobby", CommandSource::console())
        .expect("switch is answered");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
    assert!(fixture.lines().join("\n").contains("not online"));
}

#[test]
fn switch_requires_switch_permission() {
    let fixture = standard();
    let player = player_with(&["candyriya.command.server"]);
    let err = fixture
        .dispatcher
        .execute("/candyriya switch Notch pvp", player)
        .expect_err("server grant does not unlock switch");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert!(fixture.capture.take_switches().is_empty());

    let player = player_with(&["candyriya.command.switch"]);
    let code = fixture
        .dispatcher
        .execute("/candyriya switch Notch pvp", player)
        .expect("switch grant unlocks switch");
    assert_eq!(code, 1);
}

// --- /server and /goto ---

#[test]
fn goto_and_server_are_aliases_for_sender_switch() {
    let fixture = standard();

    for input in ["/server lobby", "/goto lobby"] {
        fixture
            .dispatcher
            .execute(input, deny_all_player())
            .expect("{input} must dispatch without a grant");
    }

    let switches = fixture.capture.take_switches();
    assert_eq!(switches.len(), 2);
    for switch in &switches {
        assert_eq!(switch.server, "lobby");
        assert_eq!(
            switch.player,
            notch(),
            "alias must switch the invoking player"
        );
    }
    assert_eq!(switches[0], switches[1], "aliases must switch identically");
}

#[test]
fn server_self_switch_rejects_non_players() {
    let fixture = standard();

    let console = CommandSource::console();
    let code = fixture
        .dispatcher
        .execute("/server lobby", console)
        .expect("server is answered, not an error");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
    assert!(
        fixture
            .lines()
            .iter()
            .any(|line| line.contains("Only players")),
        "only-players notice missing: {:?}",
        fixture.lines()
    );
}

#[test]
fn server_invalid_name_rejected_with_known_list() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/server nope", deny_all_player())
        .expect("server is answered");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("nope"));
    assert!(joined.contains("lobby"));
    assert!(joined.contains("pvp"));
}

#[test]
fn server_sender_offline_rejected() {
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![ServerInfo::new("lobby", 0), ServerInfo::new("pvp", 1)],
            players: vec![player("jeb_", "pvp", 2)], // Notch is not online
            backends: default_backends(),
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/server lobby", deny_all_player())
        .expect("server is answered");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
    assert!(fixture.lines().join("\n").contains("not online"));
}

#[test]
fn goto_is_a_true_alias_of_server() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/goto pvp", deny_all_player())
        .expect("goto is open without a grant");
    assert_eq!(code, 1);
    assert_eq!(
        fixture.capture.take_switches().len(),
        1,
        "goto must switch like server"
    );
}

// --- /glist ---

#[test]
fn glist_prints_count_and_per_backend_lines() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/glist", deny_all_player())
        .expect("glist is open without a grant");
    assert_eq!(code, 1);

    let lines = fixture.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("players online:") && line.contains('3')),
        "total line missing: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("<green>lobby<reset>") && line.contains('2')),
        "lobby line missing: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("<gold>pvp<reset>") && line.contains('1')),
        "pvp line missing: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("<gray>skyblock<reset>") && line.contains('0')),
        "skyblock line missing: {lines:?}"
    );
    // the list carries counts, not names
    assert!(
        !lines.iter().any(|line| line.contains("Notch")),
        "glist must not print the roster: {lines:?}"
    );
}

#[test]
fn glist_counts_players_on_unregistered_backends_in_total_only() {
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![ServerInfo::new("lobby", 0)],
            players: vec![player("Notch", "survival", 1)],
            backends: vec![backend("lobby", BackendState::Online, 0, Some(10))],
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/glist", CommandSource::console())
        .expect("glist executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(
        joined.contains("players online:") && joined.contains('1'),
        "total line must count the unlisted player: {joined}"
    );
    assert!(
        !joined.contains("survival"),
        "unconfigured backends get no line: {joined}"
    );
}

#[test]
fn glist_on_empty_network_reports_zero() {
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![ServerInfo::new("lobby", 0)],
            players: vec![],
            backends: default_backends(),
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/glist", CommandSource::console())
        .expect("glist executes");
    assert_eq!(code, 1);
    let lines = fixture.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("players online:") && line.contains('0')),
        "zero-count total line missing: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("No players online.")),
        "empty-network line missing: {lines:?}"
    );
}

// --- /send ---

#[test]
fn send_single_player_moves_them() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/send Notch pvp", CommandSource::console())
        .expect("send executes");
    assert_eq!(code, 1);
    let switches = fixture.capture.take_switches();
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].player.name, "Notch");
    assert_eq!(switches[0].server, "pvp");
}

#[test]
fn send_all_moves_every_online_player() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/send ALL lobby", CommandSource::console())
        .expect("send executes");
    assert_eq!(code, 1);
    let switches = fixture.capture.take_switches();
    assert_eq!(switches.len(), 3);
    assert!(switches.iter().all(|request| request.server == "lobby"));
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("Sending"), "announcement in {joined}");
    assert!(joined.contains("3 moved"), "summary in {joined}");
}

#[test]
fn send_all_keyword_is_case_insensitive() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/send all pvp", CommandSource::console())
        .expect("send executes");
    assert_eq!(code, 1);
    assert_eq!(fixture.capture.take_switches().len(), 3);
}

#[test]
fn send_all_with_nobody_online() {
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![ServerInfo::new("lobby", 0), ServerInfo::new("pvp", 1)],
            players: vec![],
            backends: default_backends(),
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/send ALL lobby", CommandSource::console())
        .expect("send is answered");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
    assert!(fixture.lines().join("\n").contains("No players online."));
}

#[test]
fn send_unknown_server_lists_known_servers() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/send ALL nope", CommandSource::console())
        .expect("send is answered");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("nope"));
    assert!(joined.contains("lobby"));
    assert!(joined.contains("pvp"));

    let code = fixture
        .dispatcher
        .execute("/send Notch nope", CommandSource::console())
        .expect("send is answered");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
}

#[test]
fn send_unknown_target_rejected() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/send Ghost pvp", CommandSource::console())
        .expect("send is answered");
    assert_eq!(code, 0);
    assert!(fixture.capture.take_switches().is_empty());
    assert!(fixture.lines().join("\n").contains("not online"));
}

#[test]
fn send_partial_failure_reports_and_returns_zero() {
    let fixture = fixture_with(
        TestDirectory {
            servers: fixture_directory().servers,
            players: vec![player("Notch", "lobby", 1), player("jeb_", "lobby", 2)],
            backends: default_backends(),
        },
        Capture::fail_for("jeb_", "backend blew up"),
    );
    let code = fixture
        .dispatcher
        .execute("/send ALL pvp", CommandSource::console())
        .expect("send executes");
    assert_eq!(code, 0);
    let joined = fixture.lines().join("\n");
    assert!(
        joined.contains("jeb_"),
        "failed player reported in {joined}"
    );
    assert!(joined.contains("1 moved"), "summary in {joined}");
    assert!(joined.contains("1 failed"), "summary in {joined}");
}

#[test]
fn send_is_open_to_deny_all_players() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/send Steve lobby", deny_all_player())
        .expect("send is open without a grant");
    assert_eq!(code, 1);
    assert_eq!(fixture.capture.take_switches().len(), 1);
}

// --- wiring and completions ---

#[test]
fn completions_suggest_servers_and_players() {
    let fixture = standard();
    let source = CommandSource::console();

    let texts = completion_texts(&fixture.dispatcher, "/server ", source);
    assert!(texts.contains(&"lobby".to_owned()));
    assert!(texts.contains(&"pvp".to_owned()));

    let texts = completion_texts(&fixture.dispatcher, "/goto ", CommandSource::console());
    assert!(texts.contains(&"lobby".to_owned()));

    let texts = completion_texts(&fixture.dispatcher, "/send ", CommandSource::console());
    assert!(texts.contains(&"ALL".to_owned()));
    assert!(texts.contains(&"Notch".to_owned()));
    assert!(texts.contains(&"Steve".to_owned()));

    let texts = completion_texts(
        &fixture.dispatcher,
        "/candyriya switch ",
        CommandSource::console(),
    );
    assert!(texts.contains(&"Notch".to_owned()));
    assert!(texts.contains(&"Steve".to_owned()));

    let texts = completion_texts(
        &fixture.dispatcher,
        "/candyriya switch Notch ",
        CommandSource::console(),
    );
    assert!(texts.contains(&"lobby".to_owned()));
    assert!(texts.contains(&"pvp".to_owned()));
}

#[test]
fn completions_filter_by_typed_prefix() {
    let fixture = standard();
    let texts = completion_texts(&fixture.dispatcher, "/server sk", CommandSource::console());
    assert_eq!(texts, vec!["skyblock".to_owned()]);
    let texts = completion_texts(&fixture.dispatcher, "/server lo", CommandSource::console());
    assert_eq!(texts, vec!["lobby".to_owned()]);
    let texts = completion_texts(&fixture.dispatcher, "/goto zzz", CommandSource::console());
    assert!(texts.is_empty(), "no server starts with `zzz`: {texts:?}");
}

#[test]
fn roster_names_with_markup_are_escaped_in_output() {
    // angle brackets can never be typed as a word() argument, but a crafted
    // snapshot can still carry them — every rendered name must be escaped
    // before it reaches a MiniMessage line
    let fixture = fixture_with(
        TestDirectory {
            servers: fixture_directory().servers,
            players: vec![player("<gold>evil", "lobby", 9)],
            backends: default_backends(),
        },
        Capture::fail_for("<gold>evil", "backend blew up"),
    );
    let code = fixture
        .dispatcher
        .execute("/send ALL lobby", CommandSource::console())
        .expect("send executes");
    assert_eq!(code, 0, "the failed switch flips the exit code");
    let joined = fixture.lines().join("\n");
    assert!(
        joined.contains('\u{2039}') && !joined.contains("<gold>evil<reset>"),
        "untrusted name must not become markup: {joined}"
    );
}

#[test]
fn ex_girl_locale_renders_russian() {
    let fixture = standard();
    let source = CommandSource::console().with_locale("ex_girl");
    let code = fixture
        .dispatcher
        .execute("/candyriya end", source)
        .expect("end executes");
    assert_eq!(code, 1);
    assert!(
        fixture.lines().join("\n").contains("Выключ"),
        "russian shutdown line, got {:?}",
        fixture.lines()
    );
}
