//! Integration tests for the config-backed permission provider.
//! No network, no I/O — everything is built from in-memory maps.

use std::collections::HashMap;
use std::sync::Arc;

use candyriya_permissions::{
    PermissionRegistry, PermissionResolver, PermissionValue, Subject, SubjectKind,
};
use candyriya_permissions_config::{
    ConfigPermissionProvider, DEFAULT_GROUP, GrantInput, PermissionProvider, ProviderError,
};

const UUID_ALICE: &str = "069a79f4-44e9-4726-a5be-fca90e38aaf5";
const UUID_BOB: &str = "b8d1d0e8-1111-4222-8333-9f8f7e6d5c4b";

type ConfigMaps = (
    Vec<String>,
    HashMap<String, Vec<String>>,
    HashMap<String, Vec<String>>,
);

fn maps() -> ConfigMaps {
    let default_nodes = vec!["candyriya.ping".to_string()];
    let groups = HashMap::from([
        ("admin".to_string(), vec!["*".to_string()]),
        (
            "moderator".to_string(),
            vec![
                "candyriya.command.kick".to_string(),
                "candyriya.command.mute".to_string(),
            ],
        ),
        ("vip".to_string(), vec!["candyriya.fly".to_string()]),
    ]);
    let players = HashMap::from([
        (UUID_ALICE.to_string(), vec!["admin".to_string()]),
        (
            UUID_BOB.to_string(),
            vec!["moderator".to_string(), "vip".to_string()],
        ),
    ]);
    (default_nodes, groups, players)
}

fn fixture() -> ConfigPermissionProvider {
    let (d, g, p) = maps();
    ConfigPermissionProvider::new(d, g, p).expect("fixture is valid")
}

fn console() -> Subject {
    Subject::new(SubjectKind::Console, "console")
}

fn player(id: &str) -> Subject {
    Subject::new(SubjectKind::Player, id)
}

fn nodes_of(grants: &[candyriya_permissions::Grant]) -> Vec<&str> {
    grants.iter().map(|g| g.node.as_str()).collect()
}

#[test]
fn console_gets_default_nodes_only() {
    let provider = fixture();
    let grants = provider.grants_for(&console());
    assert_eq!(nodes_of(&grants), ["candyriya.ping"]);
    assert!(grants.iter().all(|g| g.value == PermissionValue::Allow));
}

#[test]
fn player_gets_default_plus_assigned_groups() {
    let provider = fixture();
    let grants = provider.grants_for(&player(UUID_BOB));
    assert_eq!(
        nodes_of(&grants),
        [
            "candyriya.ping",
            "candyriya.command.kick",
            "candyriya.command.mute",
            "candyriya.fly",
        ]
    );
}

#[test]
fn group_assignment_is_looked_up_from_player_groups_map() {
    let provider = fixture();
    let grants = provider.grants_for(&player(UUID_ALICE));
    assert_eq!(nodes_of(&grants), ["candyriya.ping", "*"]);
    assert_eq!(provider.player_groups(UUID_ALICE), ["admin"]);
    assert!(provider.player_groups("unknown-uuid").is_empty());
}

#[test]
fn subject_groups_are_unioned_with_provider_assignments() {
    let provider = fixture();
    let subject = player("00000000-0000-4000-8000-000000000000").with_group("vip");
    let grants = provider.grants_for(&subject);
    assert_eq!(nodes_of(&grants), ["candyriya.ping", "candyriya.fly"]);
}

#[test]
fn subject_groups_and_provider_assignments_dedupe_overlap() {
    let provider = fixture();
    // alice is assigned "admin" via the map; putting "admin" on the subject
    // too must not double-emit the admin grants.
    let subject = player(UUID_ALICE).with_group("admin");
    let grants = provider.grants_for(&subject);
    assert_eq!(nodes_of(&grants), ["candyriya.ping", "*"]);
}

#[test]
fn group_subject_gets_its_own_grants_only() {
    let provider = fixture();
    let subject = Subject::new(SubjectKind::Group, "moderator");
    let grants = provider.grants_for(&subject);
    assert_eq!(
        nodes_of(&grants),
        ["candyriya.command.kick", "candyriya.command.mute"]
    );
}

#[test]
fn console_never_gets_group_grants() {
    let provider = fixture();
    let subject = console().with_group("admin");
    let grants = provider.grants_for(&subject);
    assert_eq!(nodes_of(&grants), ["candyriya.ping"]);
}

#[test]
fn other_subject_gets_nothing() {
    let provider = fixture();
    let subject = Subject::new(SubjectKind::Other("quest".to_string()), "quest-1");
    assert!(provider.grants_for(&subject).is_empty());
}

#[test]
fn player_without_assignment_gets_default_only() {
    let provider = fixture();
    let grants = provider.grants_for(&player("11111111-2222-4333-8444-555555555555"));
    assert_eq!(nodes_of(&grants), ["candyriya.ping"]);
}

#[test]
fn unknown_group_in_assignment_is_ignored() {
    let (d, g, _) = maps();
    let players = HashMap::from([(UUID_ALICE.to_string(), vec!["no-such-group".to_string()])]);
    let provider = ConfigPermissionProvider::new(d, g, players).expect("valid");
    let grants = provider.grants_for(&player(UUID_ALICE));
    assert_eq!(nodes_of(&grants), ["candyriya.ping"]);
}

#[test]
fn grants_dedupe_across_default_and_group() {
    // Same node in both buckets: emitted once.
    let default_nodes = vec!["candyriya.shared".to_string()];
    let groups = HashMap::from([(
        "admin".to_string(),
        vec![
            "candyriya.shared".to_string(),
            "candyriya.admin-only".to_string(),
        ],
    )]);
    let players = HashMap::from([(UUID_ALICE.to_string(), vec!["admin".to_string()])]);
    let provider = ConfigPermissionProvider::new(default_nodes, groups, players).expect("valid");
    let grants = provider.grants_for(&player(UUID_ALICE));
    assert_eq!(
        nodes_of(&grants),
        ["candyriya.shared", "candyriya.admin-only"]
    );
}

#[test]
fn duplicate_node_within_one_bucket_is_stored_once() {
    let default_nodes = vec!["candyriya.ping".to_string(), "candyriya.ping".to_string()];
    let provider = ConfigPermissionProvider::new(default_nodes, HashMap::new(), HashMap::new())
        .expect("valid");
    assert_eq!(provider.default_grants().len(), 1);
    assert_eq!(provider.len(), 1);
}

#[test]
fn wildcard_nodes_flow_through_verbatim_and_resolve() {
    let provider = fixture();
    let grants = provider.grants_for(&player(UUID_ALICE));
    let wildcard = grants
        .iter()
        .find(|g| g.node.as_str() == "*")
        .expect("wildcard stored verbatim");
    assert_eq!(wildcard.value, PermissionValue::Allow);

    let resolver = PermissionResolver::new(Arc::new(PermissionRegistry::new()));
    let subject = player(UUID_ALICE);
    let query = candyriya_permissions::PermissionNode::new("candyriya.command.kick").unwrap();
    let resolution = resolver.resolve(&subject, &query, &grants);
    assert!(resolution.allowed);
}

#[test]
fn empty_config_yields_empty_provider() {
    let provider = ConfigPermissionProvider::new(Vec::new(), HashMap::new(), HashMap::new())
        .expect("empty is valid");
    assert!(provider.is_empty());
    assert_eq!(provider.len(), 0);
    assert!(provider.grants_for(&console()).is_empty());
    assert!(provider.grants_for(&player(UUID_ALICE)).is_empty());
}

#[test]
fn load_from_config_maps_default_key_and_groups() {
    let cfg = vec![
        (
            DEFAULT_GROUP.to_string(),
            vec!["candyriya.ping".to_string()],
        ),
        ("admin".to_string(), vec!["*".to_string()]),
    ];
    let provider = ConfigPermissionProvider::load_from_config(&cfg).expect("valid");
    assert_eq!(
        nodes_of(&provider.grants_for(&console())),
        ["candyriya.ping"]
    );
    // No player_groups can be expressed in the flat shape; wire them via new().
    assert_eq!(provider.grants_for_group("admin").len(), 1);
    assert!(provider.player_groups(UUID_ALICE).is_empty());
}

#[test]
fn load_from_config_merges_duplicate_keys_in_order() {
    let cfg = vec![
        ("admin".to_string(), vec!["candyriya.a".to_string()]),
        ("admin".to_string(), vec!["candyriya.b".to_string()]),
    ];
    let provider = ConfigPermissionProvider::load_from_config(&cfg).expect("valid");
    assert_eq!(
        nodes_of(provider.grants_for_group("admin")),
        ["candyriya.a", "candyriya.b"]
    );
}

#[test]
fn invalid_nodes_are_all_reported_with_key_and_index() {
    let default_nodes = vec!["candyriya.ok".to_string(), "NOT A NODE".to_string()];
    let groups = HashMap::from([
        (
            "admin".to_string(),
            vec!["*".to_string(), "bad node".to_string()],
        ),
        ("moderator".to_string(), vec!["..".to_string()]),
    ]);
    let err = ConfigPermissionProvider::new(default_nodes, groups, HashMap::new())
        .expect_err("invalid nodes must fail");
    let ProviderError::InvalidNodes(invalid) = err;
    let entries = invalid.entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].key, DEFAULT_GROUP);
    assert_eq!(entries[0].index, 1);
    assert_eq!(entries[0].node, "NOT A NODE");
    assert_eq!(entries[1].key, "admin");
    assert_eq!(entries[1].index, 1);
    assert_eq!(entries[1].node, "bad node");
    assert_eq!(entries[2].key, "moderator");
    assert_eq!(entries[2].index, 0);
    assert_eq!(entries[2].node, "..");
}

#[test]
fn invalid_nodes_via_load_from_config_report_offending_key_and_index() {
    let cfg = vec![
        (DEFAULT_GROUP.to_string(), vec![".leading-dot".to_string()]),
        (
            "vip".to_string(),
            vec!["candyriya.ok".to_string(), "UPPER".to_string()],
        ),
    ];
    let err = ConfigPermissionProvider::load_from_config(&cfg).expect_err("must fail");
    let ProviderError::InvalidNodes(invalid) = err;
    let entries = invalid.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].key, DEFAULT_GROUP);
    assert_eq!(entries[0].index, 0);
    assert_eq!(entries[1].key, "vip");
    assert_eq!(entries[1].index, 1);
    // The rendered error mentions every offending entry, not just the first.
    let rendered = invalid.to_string();
    assert!(rendered.contains("vip[1]"));
    assert!(rendered.contains("default[0]"));
    assert!(rendered.contains("UPPER"));
}

#[test]
fn grant_input_output_carries_source_for_diagnostics() {
    let grant = GrantInput {
        group: "admin".to_string(),
        node: "candyriya.command.kick".to_string(),
    }
    .into_grant()
    .expect("valid");
    assert_eq!(grant.source.as_deref(), Some("config:group:admin"));
    assert_eq!(grant.value, PermissionValue::Allow);

    let default_grant = GrantInput {
        group: DEFAULT_GROUP.to_string(),
        node: "*".to_string(),
    }
    .into_grant()
    .expect("valid");
    assert_eq!(default_grant.source.as_deref(), Some("config:default"));
}

#[test]
fn grants_for_group_returns_slice_or_empty() {
    let provider = fixture();
    assert_eq!(provider.grants_for_group("vip").len(), 1);
    assert!(provider.grants_for_group("does-not-exist").is_empty());
}

#[test]
fn group_grants_do_not_leak_to_other_groups() {
    let provider = fixture();
    let grants = provider.grants_for(&player(UUID_BOB));
    // bob holds moderator + vip, never admin
    assert!(!nodes_of(&grants).contains(&"*"));
}

#[test]
fn default_and_group_grants_carry_no_contexts() {
    // The subject's contexts are irrelevant to config grants: the provider
    // never attaches contexts to the grants it emits (config grants have no
    // context syntax in v1).
    let provider = fixture();
    let subject = player(UUID_ALICE)
        .with_group("admin")
        .with_context("server", "lobby");
    let grants = provider.grants_for(&subject);
    assert!(grants.iter().any(|g| g.node.as_str() == "*"));
    assert!(grants.iter().all(|g| g.contexts.is_empty()));
}
