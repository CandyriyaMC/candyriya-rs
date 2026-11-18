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
    ConnectionStats, Dispatcher, Locales, NetworkDirectory, OnlinePlayer, PlayerRef,
    PluginPhaseReport, ProxyEffects, ProxySnapshot, PsConn, PsExitStatus, PsSnapshot, PsUnit,
    PsUnitKind, PsUnitState, PsWatchHandle, ReloadReport, ReloadScope, ServerInfo, ShutdownHandle,
    SwitchRequest, SwitchResult,
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
    ps: PsSnapshot,
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

    fn ps_snapshot(&self) -> PsSnapshot {
        self.ps.clone()
    }
}

fn ps_unit(
    pid: u64,
    ppid: u64,
    name: &str,
    kind: PsUnitKind,
    started_at: std::time::Instant,
) -> PsUnit {
    PsUnit {
        pid,
        ppid,
        name: name.to_owned(),
        kind,
        state: PsUnitState::Running,
        parked: false,
        started_at,
        restart_count: 0,
        last_exit: None,
        exited_at: None,
    }
}

fn default_ps_snapshot() -> PsSnapshot {
    let now = std::time::Instant::now();
    let units = vec![
        ps_unit(
            1,
            0,
            "init",
            PsUnitKind::LongLived,
            now - Duration::from_secs(3600),
        ),
        ps_unit(
            2,
            1,
            "config",
            PsUnitKind::LongLived,
            now - Duration::from_secs(3599),
        ),
        ps_unit(
            3,
            1,
            "proxy.accept",
            PsUnitKind::LongLived,
            now - Duration::from_secs(3598),
        ),
        ps_unit(
            4,
            1,
            "players",
            PsUnitKind::LongLived,
            now - Duration::from_secs(3597),
        ),
        ps_unit(
            5,
            4,
            "conn-1",
            PsUnitKind::Connection,
            now - Duration::from_secs(600),
        ),
        ps_unit(
            6,
            4,
            "conn-2",
            PsUnitKind::Connection,
            now - Duration::from_secs(300),
        ),
        ps_unit(
            7,
            1,
            "plugins",
            PsUnitKind::LongLived,
            now - Duration::from_secs(3590),
        ),
        PsUnit {
            restart_count: 2,
            ..ps_unit(
                8,
                7,
                "plugin-doom",
                PsUnitKind::Plugin,
                now - Duration::from_secs(3000),
            )
        },
    ];
    let conns = vec![
        PsConn {
            pid: 5,
            bytes_rx: 1024,
            bytes_tx: 2048,
            connected_at: now - Duration::from_secs(600),
            last_activity: now - Duration::from_secs(3),
        },
        PsConn {
            pid: 6,
            bytes_rx: 4096,
            bytes_tx: 8192,
            connected_at: now - Duration::from_secs(300),
            last_activity: now - Duration::from_secs(1),
        },
    ];
    PsSnapshot {
        units,
        conns,
        zombies: 0,
        stuck_zombies: 0,
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
        ps: default_ps_snapshot(),
    }
}

fn empty_ps_snapshot() -> PsSnapshot {
    PsSnapshot {
        units: Vec::new(),
        conns: Vec::new(),
        zombies: 0,
        stuck_zombies: 0,
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
    watch: PsWatchHandle,
}

impl Fixture {
    fn lines(&self) -> Vec<String> {
        self.capture.lines()
    }

    /// Take the pending ps watch request, if any; mirrors what the console
    /// REPL does after dispatch.
    fn take_watch(&self) -> Option<bool> {
        self.watch.take()
    }
}

fn fixture() -> Fixture {
    fixture_with(fixture_directory(), Capture::switched())
}

fn fixture_with(directory: TestDirectory, capture: Capture) -> Fixture {
    fixture_with_reload_fn(directory, capture, None)
}

fn fixture_with_reload_fn(
    directory: TestDirectory,
    capture: Capture,
    reload: Option<candyriya_command::ReloadFn>,
) -> Fixture {
    let capture = Arc::new(capture);
    let shutdown = ShutdownHandle::new();
    let watch = PsWatchHandle::new();
    let locales = Locales::new(
        I18n::new("en").expect("embedded en locale"),
        I18n::new("ex_girl").expect("embedded ex_girl locale"),
    );
    let effects: Arc<dyn ProxyEffects> = capture.clone();
    let state = CommandState {
        locales,
        directory: Arc::new(directory),
        effects,
        watch: watch.clone(),
    };
    let registry = CommandRegistry::new(state, shutdown.clone());
    let registry = match reload {
        Some(reload) => registry.with_reload(reload),
        None => registry,
    };
    let dispatcher = registry.dispatcher().expect("built-in commands build");
    Fixture {
        dispatcher,
        capture,
        shutdown,
        watch,
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
            ps: default_ps_snapshot(),
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
            ps: default_ps_snapshot(),
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
            ps: default_ps_snapshot(),
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
            ps: default_ps_snapshot(),
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
            ps: default_ps_snapshot(),
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
            ps: default_ps_snapshot(),
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
            ps: default_ps_snapshot(),
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
            ps: default_ps_snapshot(),
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

// --- /candyriya reload ---

fn ok_report() -> ReloadReport {
    ReloadReport {
        config: Some(Ok("2 backends re-registered".to_owned())),
        plugins: Some(PluginPhaseReport {
            reloaded: 1,
            failed: 0,
            details: vec![],
        }),
    }
}

fn reload_fixture(report: ReloadReport, scopes: Arc<Mutex<Vec<ReloadScope>>>) -> Fixture {
    let directory = fixture_directory();
    let reload: candyriya_command::ReloadFn = Arc::new(move |scope| {
        scopes.lock().expect("scopes lock").push(scope);
        report.clone()
    });
    fixture_with_reload_fn(directory, Capture::switched(), Some(reload))
}

#[test]
fn reload_denied_for_deny_all_player_and_does_not_call_the_hook() {
    let scopes = Arc::new(Mutex::new(Vec::new()));
    let fixture = reload_fixture(ok_report(), Arc::clone(&scopes));

    let err = fixture
        .dispatcher
        .execute("/candyriya reload", deny_all_player())
        .expect_err("deny-all player cannot reload");
    let CommandError::Brigadier(syntax) = err else {
        panic!("expected a brigadier error, got {err:?}");
    };
    // brigadier renders a permission-gated node as an argument failure,
    // not as "unknown command" — to the invoker the subtree is simply
    // not there (the same contract as every gated `/candyriya` node)
    assert_eq!(syntax.kind(), &BuiltInError::DispatcherUnknownArgument);

    // the scoped forms sit behind the same gate
    for input in ["/candyriya reload config", "/candyriya reload plugins"] {
        let err = fixture
            .dispatcher
            .execute(input, deny_all_player())
            .expect_err("deny-all player cannot reload");
        assert!(matches!(err, CommandError::Brigadier(_)));
    }
    assert!(scopes.lock().expect("scopes lock").is_empty());
    assert!(fixture.lines().is_empty());
}

#[test]
fn reload_prints_both_phase_summaries_and_returns_one() {
    let scopes = Arc::new(Mutex::new(Vec::new()));
    let fixture = reload_fixture(ok_report(), Arc::clone(&scopes));

    let code = fixture
        .dispatcher
        .execute("/candyriya reload", CommandSource::console())
        .expect("reload executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(
        joined.contains("config reloaded"),
        "config line in {joined}"
    );
    assert!(
        joined.contains("1 plugins reloaded, 0 failed"),
        "plugins line in {joined}"
    );
    assert_eq!(
        scopes.lock().expect("scopes lock").as_slice(),
        [ReloadScope::All]
    );
}

#[test]
fn reload_config_only_and_plugins_only() {
    let scopes = Arc::new(Mutex::new(Vec::new()));
    let fixture = reload_fixture(ok_report(), Arc::clone(&scopes));

    let code = fixture
        .dispatcher
        .execute("/candyriya reload config", CommandSource::console())
        .expect("reload config executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("config reloaded"));
    assert!(!joined.contains("plugins reloaded"));

    let before = fixture.lines().len();
    let code = fixture
        .dispatcher
        .execute("/candyriya reload plugins", CommandSource::console())
        .expect("reload plugins executes");
    assert_eq!(code, 1);
    // the capture accumulates from the previous call, so only the fresh
    // lines decide what this invocation printed
    let joined = fixture.lines()[before..].join("\n");
    assert!(joined.contains("1 plugins reloaded, 0 failed"));
    assert!(!joined.contains("config reloaded"));

    assert_eq!(
        scopes.lock().expect("scopes lock").as_slice(),
        [ReloadScope::Config, ReloadScope::Plugins]
    );
}

#[test]
fn reload_config_failure_is_reported_and_plugins_still_run() {
    let report = ReloadReport {
        config: Some(Err("expected `[servers]`".to_owned())),
        plugins: Some(PluginPhaseReport {
            reloaded: 2,
            failed: 1,
            details: vec!["broken: wasm stage failed".to_owned()],
        }),
    };
    let fixture = reload_fixture(report, Arc::new(Mutex::new(Vec::new())));

    let code = fixture
        .dispatcher
        .execute("/candyriya reload", CommandSource::console())
        .expect("reload executes");
    assert_eq!(code, 0);
    let joined = fixture.lines().join("\n");
    assert!(
        joined.contains("Configuration reload failed"),
        "config failure line in {joined}"
    );
    assert!(
        joined.contains("expected `[servers]`"),
        "failure reason in {joined}"
    );
    assert!(
        joined.contains("2 plugins reloaded, 1 failed"),
        "plugins summary in {joined}"
    );
    assert!(
        joined.contains("broken: wasm stage failed"),
        "failure detail in {joined}"
    );
}

#[test]
fn reload_without_wiring_reports_unavailable() {
    let fixture = fixture();

    let code = fixture
        .dispatcher
        .execute("/candyriya reload", CommandSource::console())
        .expect("reload answers");
    assert_eq!(code, 0);
    assert!(
        fixture
            .lines()
            .join("\n")
            .contains("Reload is not wired in this build.")
    );
}

#[test]
fn reload_swaps_the_config_store_atomically() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("candyriya.toml");
    std::fs::write(
        &path,
        "config-version = \"0.1\"\nbind = \"127.0.0.1:25565\"\nmotd = \"<green>before\"\nonline-mode = false\n[protocol]\nauto-update = false\n",
    )
    .expect("write v1");
    let store = Arc::new(candyriya_config::ConfigStore::from_path(&path).expect("v1 loads"));
    assert_eq!(store.get().motd, "<green>before");

    // the file changes underneath; the store must observe it after reload
    std::fs::write(
        &path,
        "config-version = \"0.1\"\nbind = \"127.0.0.1:25565\"\nmotd = \"<aqua>after\"\nonline-mode = false\n[protocol]\nauto-update = false\n",
    )
    .expect("write v2");

    let store_for_closure = Arc::clone(&store);
    let path_for_closure = path.clone();
    let reload: candyriya_command::ReloadFn = Arc::new(move |_scope| ReloadReport {
        config: Some(
            store_for_closure
                .reload(&path_for_closure)
                .map(|_| "1 backends re-registered".to_owned())
                .map_err(|error| error.to_string()),
        ),
        plugins: None,
    });
    let fixture = fixture_with_reload_fn(fixture_directory(), Capture::switched(), Some(reload));
    let code = fixture
        .dispatcher
        .execute("/candyriya reload config", CommandSource::console())
        .expect("reload config executes");
    assert_eq!(code, 1);
    assert_eq!(store.get().motd, "<aqua>after");
}

// --- /candyriya ps and /candyriya tree ---

#[test]
fn ps_aggregate_collapses_connections_and_counts_everything() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya ps", CommandSource::console())
        .expect("ps executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    // the two conns collapse into one aggregate row
    assert!(joined.contains("conn × 2"), "aggregate row in {joined}");
    assert!(
        !joined.contains("conn-1"),
        "conns must stay collapsed in {joined}"
    );
    assert!(
        !joined.contains("conn-2"),
        "conns must stay collapsed in {joined}"
    );
    // non-connection units keep their rows
    assert!(joined.contains("proxy.accept"), "{joined}");
    assert!(joined.contains("plugin-doom"), "{joined}");
    // summary line reports the tree's own counters
    assert!(
        joined.contains("units: <gold>8<reset>"),
        "units count in {joined}"
    );
    assert!(
        joined.contains("conn: <gold>2<reset>"),
        "conn count in {joined}"
    );
    assert!(
        joined.contains("plugins: <gold>1<reset>"),
        "plugins count in {joined}"
    );
    assert!(
        joined.contains("zombies: <gold>0<reset>"),
        "zombies count in {joined}"
    );
    // the legend is there, plain and complete
    assert!(joined.contains("R=running"), "legend in {joined}");
    // byte sums land on the aggregate row (1 KiB + 4 KiB = 5 KiB)
    assert!(joined.contains("5 KiB"), "rx sum in {joined}");
    assert!(joined.contains("10 KiB"), "tx sum in {joined}");
}

#[test]
fn ps_full_expands_every_row() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya ps --full", CommandSource::console())
        .expect("ps --full executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("conn-1"), "full expands conns in {joined}");
    assert!(joined.contains("conn-2"), "full expands conns in {joined}");
    assert!(
        !joined.contains("conn × 2"),
        "no aggregate row in --full: {joined}"
    );
    assert!(
        joined.contains("units: <gold>8<reset>"),
        "summary still honest in --full: {joined}"
    );
}

#[test]
fn ps_full_is_the_same_table_with_every_row() {
    // both modes share the header and the summary shape; --full only expands
    let full = standard();
    let code = full
        .dispatcher
        .execute("/candyriya ps --full", CommandSource::console())
        .expect("ps --full executes");
    assert_eq!(code, 1);
    let lines = full.lines();
    assert!(lines[0].contains("PID"), "header in {:?}", lines);
    assert!(lines.join("\n").contains("units: <gold>8<reset>"));
}

#[test]
fn ps_watch_sets_the_latch_without_printing() {
    for command in [
        "/candyriya ps --watch",
        "/candyriya ps --watch --full",
        "/candyriya ps --full --watch",
    ] {
        let fixture = standard();
        let code = fixture
            .dispatcher
            .execute(command, CommandSource::console())
            .expect("ps --watch executes");
        assert_eq!(code, 1);
        assert!(
            fixture.lines().is_empty(),
            "ps --watch must not paint before the REPL takes the latch: {command}"
        );
        let watch = fixture.take_watch();
        assert!(watch.is_some(), "latch must be set by {command}");
        assert_eq!(
            watch,
            Some(command.contains("--full")),
            "view selection carried by {command}"
        );
        assert_eq!(
            fixture.take_watch(),
            None,
            "the latch is consumed once (the REPL takes it after dispatch)"
        );
    }
}

#[test]
fn ps_watch_from_a_non_console_source_renders_one_frame_without_latching() {
    let fixture = standard();
    let player = player_with(&["candyriya.command.ps"]);
    let code = fixture
        .dispatcher
        .execute("/candyriya ps --watch", player)
        .expect("granted player may run ps --watch");
    assert_eq!(code, 1);
    // no watch latch: a chat source gets one ordinary frame, like /candyriya ps
    assert_eq!(fixture.take_watch(), None);
    let joined = fixture.lines().join("\n");
    assert!(
        joined.contains("PID"),
        "ordinary frame for a player: {joined}"
    );
}

#[test]
fn ps_watch_is_denied_for_a_deny_all_player() {
    let fixture = standard();
    let err = fixture
        .dispatcher
        .execute("/candyriya ps --watch", deny_all_player())
        .expect_err("deny-all player cannot run ps --watch");
    assert!(matches!(err, CommandError::Brigadier(_)));
    assert_eq!(fixture.take_watch(), None);
}

#[test]
fn ordinary_ps_does_not_set_the_watch_latch() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya ps --full", CommandSource::console())
        .expect("ps --full executes");
    assert_eq!(code, 1);
    assert_eq!(fixture.take_watch(), None);
}

#[test]
fn tree_renders_box_drawing_guides() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute("/candyriya tree", CommandSource::console())
        .expect("tree executes");
    assert_eq!(code, 1);
    let lines = fixture.lines();
    let joined = lines.join("\n");
    assert!(joined.contains("init"), "{joined}");
    assert!(joined.contains("├──"), "branch guides in {joined}");
    assert!(joined.contains("└──"), "tail guides in {joined}");
    assert!(
        joined.contains("conn-1"),
        "tree names all units in {joined}"
    );
    assert!(joined.contains("plugin-doom"), "{joined}");
    // the root line carries no guide
    let root_line = lines
        .iter()
        .find(|line| line.contains("init"))
        .expect("root line");
    assert!(!root_line.contains("──"), "root has no guide: {root_line}");
    // plugin caption carries restarts-relevant facts only (kind), and exit when present
    assert_eq!(lines.len(), 8, "one line per unit in {joined}");
}

#[test]
fn tree_marks_parked_and_failed_units() {
    let now = std::time::Instant::now();
    let mut ps = default_ps_snapshot();
    ps.units.push(PsUnit {
        pid: 9,
        ppid: 4,
        name: "conn-frozen".to_owned(),
        kind: PsUnitKind::Connection,
        state: PsUnitState::Running,
        parked: true,
        started_at: now - Duration::from_secs(60),
        restart_count: 0,
        last_exit: None,
        exited_at: None,
    });
    ps.units.push(PsUnit {
        pid: 10,
        ppid: 7,
        name: "plugin-dead".to_owned(),
        kind: PsUnitKind::Plugin,
        state: PsUnitState::FailedTerminal,
        parked: false,
        started_at: now - Duration::from_secs(30),
        restart_count: 5,
        last_exit: Some(PsExitStatus::Failure("boom".to_owned())),
        exited_at: Some(now - Duration::from_secs(5)),
    });
    ps.conns.push(PsConn {
        pid: 9,
        bytes_rx: 1,
        bytes_tx: 1,
        connected_at: now - Duration::from_secs(60),
        last_activity: now,
    });
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![],
            players: vec![],
            backends: vec![],
            ps,
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/candyriya tree", CommandSource::console())
        .expect("tree executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("frozen"), "parked marker in {joined}");
    assert!(joined.contains("conn-frozen"), "{joined}");
    assert!(joined.contains("plugin-dead"), "{joined}");
    assert!(joined.contains("failure(boom)"), "last exit in {joined}");
}

#[test]
fn ps_denied_for_deny_all_player() {
    let fixture = standard();
    for input in [
        "/candyriya ps",
        "/candyriya ps --full",
        "/candyriya ps --watch",
        "/candyriya tree",
    ] {
        let err = fixture
            .dispatcher
            .execute(input, deny_all_player())
            .expect_err("deny-all player cannot run the ps surface");
        assert!(matches!(err, CommandError::Brigadier(_)));
    }
    assert!(fixture.lines().is_empty());
}

#[test]
fn ps_grant_opens_only_the_ps_surface() {
    let fixture = standard();
    let player = player_with(&["candyriya.command.ps"]);
    let code = fixture
        .dispatcher
        .execute("/candyriya ps", player)
        .expect("granted player may run ps");
    assert_eq!(code, 1);
    let tree_err = fixture
        .dispatcher
        .execute("/candyriya tree", player_with(&["candyriya.command.ps"]))
        .expect_err("ps grant must not open tree");
    assert!(matches!(tree_err, CommandError::Brigadier(_)));
}

#[test]
fn ps_empty_tree_renders_honest_empty_state() {
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![],
            players: vec![],
            backends: vec![],
            ps: empty_ps_snapshot(),
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/candyriya ps", CommandSource::console())
        .expect("ps executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("No units"), "empty state in {joined}");
    assert!(
        joined.contains("units: <gold>0<reset>"),
        "zeros in the summary in {joined}"
    );
    assert!(!joined.contains("HTTP"), "nothing invented in {joined}");
}

#[test]
fn tree_empty_tree_renders_empty_state() {
    let fixture = fixture_with(
        TestDirectory {
            servers: vec![],
            players: vec![],
            backends: vec![],
            ps: empty_ps_snapshot(),
        },
        Capture::switched(),
    );
    let code = fixture
        .dispatcher
        .execute("/candyriya tree", CommandSource::console())
        .expect("tree executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(joined.contains("No units"), "empty state in {joined}");
}

#[test]
fn ps_outputs_ex_girl_summary_for_ex_girl_source() {
    let fixture = standard();
    let code = fixture
        .dispatcher
        .execute(
            "/candyriya ps",
            CommandSource::console().with_locale("ex_girl"),
        )
        .expect("ps executes");
    assert_eq!(code, 1);
    let joined = fixture.lines().join("\n");
    assert!(
        joined.contains("юнитов: <gold>8<reset>"),
        "ex_girl summary in {joined}"
    );
    assert!(joined.contains("АПТАЙМ"), "ex_girl header in {joined}");
    assert!(!joined.contains("UPTIME"), "no en header leak in {joined}");
}
