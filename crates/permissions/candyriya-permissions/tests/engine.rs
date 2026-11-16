//! Integration tests for the resolution engine.
//!
//! These exercise the public API (`InMemoryProvider` + `PermissionResolver` +
//! `PermissionRegistry`) exactly the way the future config provider and the
//! proxy will use it. The matching-rule spec lives in `resolver.rs` module docs.

use std::sync::Arc;

use candyriya_permissions::{
    Grant, InMemoryProvider, PermissionContext, PermissionNode, PermissionRegistry,
    PermissionResolver, PermissionValue, Subject, SubjectKind,
};

fn node(raw: &str) -> PermissionNode {
    PermissionNode::new(raw).expect("test node must be valid")
}

fn ctx(name: &str, value: &str) -> PermissionContext {
    PermissionContext::new(name, value).expect("test context must be valid")
}

fn setup() -> (
    Arc<PermissionRegistry>,
    PermissionResolver,
    InMemoryProvider,
) {
    let registry = Arc::new(PermissionRegistry::new());
    let resolver = PermissionResolver::new(Arc::clone(&registry));
    (registry, resolver, InMemoryProvider::new())
}

fn resolve(
    resolver: &PermissionResolver,
    provider: &InMemoryProvider,
    subject: &Subject,
    node: &PermissionNode,
) -> candyriya_permissions::Resolution {
    let grants = provider.effective_grants(subject);
    resolver.resolve(subject, node, &grants)
}

#[test]
fn exact_allow_and_exact_deny() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");

    provider.grant(&subject, Grant::allow(node("candyriya.command.kick")));
    provider.grant(&subject, Grant::deny(node("candyriya.command.ban")));

    assert!(
        resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.kick")
        )
        .allowed
    );
    assert!(
        !resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.ban")
        )
        .allowed
    );
    // No grant → unregistered node → default deny.
    assert!(
        !resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.mute")
        )
        .allowed
    );
}

#[test]
fn wildcard_star_matches_everything() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(&subject, Grant::allow(node("*")));

    for probe in ["candyriya.command.kick", "a", "b.c.d.e.f"] {
        assert!(
            resolve(&resolver, &provider, &subject, &node(probe)).allowed,
            "star must match {probe}"
        );
    }
}

#[test]
fn star_allows_but_loses_to_longer_literal_deny() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(&subject, Grant::allow(node("*")));
    provider.grant(&subject, Grant::deny(node("a.b")));
    assert!(!resolve(&resolver, &provider, &subject, &node("a.b")).allowed);
    assert!(resolve(&resolver, &provider, &subject, &node("a.c")).allowed);
}

#[test]
fn wildcard_single_level_matches_direct_children() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(&subject, Grant::allow(node("candyriya.command.*")));

    assert!(
        resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.kick")
        )
        .allowed
    );
    assert!(!resolve(&resolver, &provider, &subject, &node("candyriya.chat")).allowed);
}

#[test]
fn wildcard_multi_level_matches_nested_children() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(&subject, Grant::allow(node("candyriya.command.*")));

    assert!(
        resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.moderation.mute")
        )
        .allowed
    );
    assert!(
        resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.moderation.ban.hard")
        )
        .allowed
    );
}

#[test]
fn foo_star_does_not_match_foo_itself() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(&subject, Grant::allow(node("foo.*")));

    assert!(!resolve(&resolver, &provider, &subject, &node("foo")).allowed);
    assert!(resolve(&resolver, &provider, &subject, &node("foo.bar")).allowed);
    assert!(resolve(&resolver, &provider, &subject, &node("foo.bar.baz")).allowed);
}

#[test]
fn foo_star_does_not_match_dot_boundary_violations() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(&subject, Grant::allow(node("foo.*")));

    assert!(!resolve(&resolver, &provider, &subject, &node("foobar")).allowed);
    assert!(!resolve(&resolver, &provider, &subject, &node("foobar.baz")).allowed);
    assert!(!resolve(&resolver, &provider, &subject, &node("foo")).allowed);
}

#[test]
fn longer_pattern_wins_regardless_of_value() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");

    // 3-segment allow beats 2-segment deny.
    provider.grant(&subject, Grant::deny(node("a.b")));
    provider.grant(&subject, Grant::allow(node("a.b.c")));
    assert!(resolve(&resolver, &provider, &subject, &node("a.b.c")).allowed);

    // 3-segment deny beats 2-segment allow.
    let mut provider2 = InMemoryProvider::new();
    provider2.grant(&subject, Grant::allow(node("a.b")));
    provider2.grant(&subject, Grant::deny(node("a.b.c")));
    assert!(!resolve(&resolver, &provider2, &subject, &node("a.b.c")).allowed);

    // 4-segment deny beats 3-segment allow.
    let mut provider3 = InMemoryProvider::new();
    provider3.grant(&subject, Grant::allow(node("a.b.c")));
    provider3.grant(&subject, Grant::deny(node("a.b.c.d")));
    assert!(!resolve(&resolver, &provider3, &subject, &node("a.b.c.d")).allowed);
}

#[test]
fn exact_four_segment_beats_wildcard_three_segment() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(
        &subject,
        Grant::deny(node("candyriya.command.moderation.*")),
    );
    provider.grant(
        &subject,
        Grant::allow(node("candyriya.command.moderation.mute")),
    );
    // Exact pattern has 4 literal segments vs 3 for the wildcard; the longer wins.
    assert!(
        resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.moderation.mute")
        )
        .allowed
    );

    // Sibling untouched by the exact grant still falls under the wildcard deny.
    assert!(
        !resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.moderation.ban")
        )
        .allowed
    );
}

#[test]
fn trailing_wildcard_does_not_match_exact_itself() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    // `a.b.c.*` never matches `a.b.c`; the exact deny below is the only candidate.
    provider.grant(&subject, Grant::allow(node("a.b.c.*")));
    provider.grant(&subject, Grant::deny(node("a.b.c")));
    assert!(!resolve(&resolver, &provider, &subject, &node("a.b.c")).allowed);
    // On `a.b.c.d`, only the wildcard allow applies.
    assert!(resolve(&resolver, &provider, &subject, &node("a.b.c.d")).allowed);
}

#[test]
fn equal_length_deny_beats_allow() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(&subject, Grant::allow(node("a.b.c")));
    provider.grant(&subject, Grant::deny(node("a.b.c")));
    let resolution = resolve(&resolver, &provider, &subject, &node("a.b.c"));
    assert!(!resolution.allowed);
    assert_eq!(
        resolution.best.as_ref().map(|best| best.grant.value),
        Some(PermissionValue::Deny)
    );
}

#[test]
fn deny_in_subject_beats_same_pattern_allow_in_group() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1").with_group("admins");
    provider.grant_to_group("admins", Grant::allow(node("candyriya.command.mute")));
    provider.grant(&subject, Grant::deny(node("candyriya.command.mute")));
    assert!(
        !resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.mute")
        )
        .allowed
    );
}

#[test]
fn deny_from_one_group_beats_allow_from_another_group() {
    let (_registry, resolver, mut provider) = setup();
    let subject =
        Subject::new(SubjectKind::Player, "player-1").with_groups(["allowers", "deniers"]);
    provider.grant_to_group("allowers", Grant::allow(node("a.b.c")));
    provider.grant_to_group("deniers", Grant::deny(node("a.b.c")));
    assert!(!resolve(&resolver, &provider, &subject, &node("a.b.c")).allowed);

    // And the mirrored arrangement, to prove it is not order-dependent.
    let subject2 =
        Subject::new(SubjectKind::Player, "player-2").with_groups(["deniers", "allowers"]);
    assert!(!resolve(&resolver, &provider, &subject2, &node("a.b.c")).allowed);
}

#[test]
fn inherited_deny_beats_inherited_allow_at_same_length() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1").with_groups(["g1", "g2"]);
    provider.grant_to_group("g1", Grant::allow(node("candyriya.*")));
    provider.grant_to_group("g2", Grant::deny(node("candyriya.*")));
    assert!(!resolve(&resolver, &provider, &subject, &node("candyriya.chat")).allowed);
}

#[test]
fn longer_group_pattern_wins_across_groups() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1").with_groups(["broad", "narrow"]);
    // Group "broad" allows everything candyriya.command; group "narrow" denies one subtree.
    provider.grant_to_group("broad", Grant::allow(node("candyriya.command.*")));
    provider.grant_to_group(
        "narrow",
        Grant::deny(node("candyriya.command.moderation.*")),
    );
    assert!(
        resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.kick")
        )
        .allowed
    );
    assert!(
        !resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.moderation.ban")
        )
        .allowed
    );
}

#[test]
fn disjoint_wildcards_do_not_interfere() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    provider.grant(&subject, Grant::allow(node("foo.*")));
    // bar.* can never match a node foo.* matches; both are specificity 1, both deny here.
    provider.grant(&subject, Grant::deny(node("bar.*")));

    assert!(resolve(&resolver, &provider, &subject, &node("foo.x")).allowed);
    assert!(!resolve(&resolver, &provider, &subject, &node("bar.x")).allowed);
    assert!(!resolve(&resolver, &provider, &subject, &node("baz.x")).allowed);
}

#[test]
fn no_match_falls_back_to_registry_default() {
    let (registry, resolver, provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");

    // Unregistered node → false.
    assert!(!resolve(&resolver, &provider, &subject, &node("anything.here")).allowed);

    registry
        .register(
            node("anything.here"),
            PermissionValue::Allow,
            "test default allow",
            Some("test".to_owned()),
        )
        .expect("register");
    assert!(resolve(&resolver, &provider, &subject, &node("anything.here")).allowed);

    registry
        .register(
            node("other.node"),
            PermissionValue::Deny,
            "test default deny",
            None,
        )
        .expect("register");
    assert!(!resolve(&resolver, &provider, &subject, &node("other.node")).allowed);
}

#[test]
fn registry_default_applies_only_on_no_match() {
    let (registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "player-1");
    registry
        .register(node("a.b"), PermissionValue::Allow, "test default", None)
        .expect("register");
    provider.grant(&subject, Grant::deny(node("a.b")));
    // An explicit deny wins over the default-allow.
    assert!(!resolve(&resolver, &provider, &subject, &node("a.b")).allowed);
}

#[test]
fn duplicate_registration_is_an_error() {
    let (registry, _resolver, _provider) = setup();
    registry
        .register(node("a.b"), PermissionValue::Allow, "first", None)
        .expect("first register");
    let err = registry
        .register(node("a.b"), PermissionValue::Deny, "second", None)
        .expect_err("duplicate must fail");
    assert!(err.to_string().contains("a.b"));
}

#[test]
fn context_grant_applies_only_with_matching_context() {
    let (_registry, resolver, mut provider) = setup();
    let grant = Grant::allow(node("a.b")).with_contexts(vec![ctx("server", "lobby")]);
    provider.grant_to_group("staff", grant);

    let on_lobby = Subject::new(SubjectKind::Player, "p1")
        .with_group("staff")
        .with_context("server", "lobby");
    let on_world = Subject::new(SubjectKind::Player, "p2")
        .with_group("staff")
        .with_context("server", "survival");
    let no_context = Subject::new(SubjectKind::Player, "p3").with_group("staff");

    assert!(resolve(&resolver, &provider, &on_lobby, &node("a.b")).allowed);
    assert!(!resolve(&resolver, &provider, &on_world, &node("a.b")).allowed);
    assert!(!resolve(&resolver, &provider, &no_context, &node("a.b")).allowed);
}

#[test]
fn grant_with_two_contexts_requires_both() {
    let (_registry, resolver, mut provider) = setup();
    let grant = Grant::allow(node("a.b"))
        .with_contexts(vec![ctx("server", "lobby"), ctx("world", "nether")]);
    let owner = Subject::new(SubjectKind::Player, "owner");
    provider.grant(&owner, grant);

    let both = Subject::new(SubjectKind::Player, "owner")
        .with_context("server", "lobby")
        .with_context("world", "nether");
    let one_missing = Subject::new(SubjectKind::Player, "owner").with_context("server", "lobby");
    let extra_subject_contexts_ok = Subject::new(SubjectKind::Player, "owner")
        .with_context("server", "lobby")
        .with_context("world", "nether")
        .with_context("gamemode", "creative"); // subset semantics: extra subject contexts are fine

    assert!(resolve(&resolver, &provider, &both, &node("a.b")).allowed);
    assert!(!resolve(&resolver, &provider, &one_missing, &node("a.b")).allowed);
    assert!(
        resolve(
            &resolver,
            &provider,
            &extra_subject_contexts_ok,
            &node("a.b")
        )
        .allowed
    );

    // The mismatched-value sibling is rejected too.
    let wrong_value = Subject::new(SubjectKind::Player, "owner")
        .with_context("server", "lobby")
        .with_context("world", "end");
    assert!(!resolve(&resolver, &provider, &wrong_value, &node("a.b")).allowed);
}

#[test]
fn uncontextualized_grant_applies_in_every_context() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "p1");
    provider.grant(&subject, Grant::allow(node("a.b")));

    let contextualized = Subject::new(SubjectKind::Player, "p2").with_context("server", "lobby");
    provider.grant(&contextualized, Grant::allow(node("a.b")));

    assert!(resolve(&resolver, &provider, &subject, &node("a.b")).allowed);
    assert!(resolve(&resolver, &provider, &contextualized, &node("a.b")).allowed);
}

#[test]
fn same_wildcard_deny_shadows_group_allow_at_equal_length() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "p1").with_group("broadallow");
    provider.grant_to_group("broadallow", Grant::allow(node("candyriya.command.*")));
    provider.grant(&subject, Grant::deny(node("candyriya.command.*")));
    assert!(
        !resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.kick")
        )
        .allowed
    );
}

#[test]
fn longer_subject_allow_overrides_group_wildcard_deny() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "p1").with_group("deniers");
    provider.grant_to_group("deniers", Grant::deny(node("candyriya.*")));
    provider.grant(&subject, Grant::allow(node("candyriya.command.kick")));
    assert!(
        resolve(
            &resolver,
            &provider,
            &subject,
            &node("candyriya.command.kick")
        )
        .allowed
    );
}

#[test]
fn console_subject_resolves_like_any_other() {
    let (_registry, resolver, mut provider) = setup();
    let console = Subject::new(SubjectKind::Console, "console");
    provider.grant(&console, Grant::allow(node("*")));
    assert!(resolve(&resolver, &provider, &console, &node("anything.at.all")).allowed);

    let quiet_console = Subject::new(SubjectKind::Console, "console2");
    assert!(
        !resolve(
            &resolver,
            &provider,
            &quiet_console,
            &node("anything.at.all")
        )
        .allowed
    );
}

#[test]
fn other_subject_kind_is_a_separate_key() {
    let (_registry, resolver, mut provider) = setup();
    let service = Subject::new(SubjectKind::Other("ci".to_owned()), "runner");
    provider.grant(&service, Grant::allow(node("a.b")));
    let wrong_kind = Subject::new(SubjectKind::Player, "runner");
    assert!(resolve(&resolver, &provider, &service, &node("a.b")).allowed);
    assert!(!resolve(&resolver, &provider, &wrong_kind, &node("a.b")).allowed);
}

#[test]
fn effective_grants_is_own_then_groups_in_order() {
    let (_registry, _resolver, mut provider) = setup();
    provider.grant_to_group("g1", Grant::allow(node("g1.node")));
    provider.grant_to_group("g2", Grant::allow(node("g2.node")));
    let subject = Subject::new(SubjectKind::Player, "p")
        .with_groups(["g1", "g2"])
        .with_context("server", "lobby");
    provider.grant(&subject, Grant::allow(node("own.node")));

    let effective = provider.effective_grants(&subject);
    let nodes: Vec<&str> = effective.iter().map(|grant| grant.node.as_str()).collect();
    assert_eq!(nodes, ["own.node", "g1.node", "g2.node"]);
}

#[test]
fn best_match_reports_the_winning_grant_source() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "p1").with_group("admins");
    provider.grant_to_group(
        "admins",
        Grant::allow(node("a.b.c")).with_source("group:admins"),
    );
    provider.grant(&subject, Grant::deny(node("a.b.c")).with_source("manual"));

    let resolution = resolve(&resolver, &provider, &subject, &node("a.b.c"));
    assert!(!resolution.allowed);
    let best = resolution.best.expect("a winner must be reported");
    assert_eq!(best.grant.source.as_deref(), Some("manual"));
    assert_eq!(best.grant.value, PermissionValue::Deny);
}

#[test]
fn group_specificity_beats_subject_genericity() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "p1").with_group("narrow");
    // Subject has a broad deny; the group's longer allow overrides by specificity.
    provider.grant(&subject, Grant::deny(node("a.*")));
    provider.grant_to_group("narrow", Grant::allow(node("a.b.c")));
    assert!(resolve(&resolver, &provider, &subject, &node("a.b.c")).allowed);
    // ...but the broad deny still bites on siblings.
    assert!(!resolve(&resolver, &provider, &subject, &node("a.b")).allowed);
}

#[test]
fn node_validation_rejects_bad_input_at_the_edge() {
    for raw in [
        "",
        "A.b",
        "a..b",
        ".a",
        "a.",
        "a b",
        "a**",
        "**",
        "a.**",
        "a.*.b",
        "*.a",
        "café.node",
    ] {
        assert!(
            PermissionNode::new(raw).is_err(),
            "expected rejection: {raw:?}"
        );
    }
    assert!(PermissionNode::new("a.*").is_ok());
    assert!(PermissionNode::new("*").is_ok());
}

#[test]
fn context_validation_rejects_bad_input_at_the_edge() {
    assert!(PermissionContext::new("Server", "lobby").is_err());
    assert!(PermissionContext::new("", "lobby").is_err());
    assert!(PermissionContext::new("server", "x\u{7}").is_err());
    assert!(PermissionContext::new("server", &"v".repeat(MAX_CONTEXT_VALUE_LEN_TEST)).is_ok());
    assert!(PermissionContext::new("server", &"v".repeat(MAX_CONTEXT_VALUE_LEN_TEST + 1)).is_err());
}

#[test]
fn wildcard_miss_falls_through_to_registry_default() {
    let (registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "p1");
    registry
        .register(
            node("foo"),
            PermissionValue::Allow,
            "bare foo defaults allow",
            None,
        )
        .expect("register");
    provider.grant(&subject, Grant::deny(node("foo.*")));

    // `foo.*` does not match `foo`; the registry default (allow) decides.
    assert!(resolve(&resolver, &provider, &subject, &node("foo")).allowed);
    // ...but it matches `foo.bar`, so the grant wins there.
    assert!(!resolve(&resolver, &provider, &subject, &node("foo.bar")).allowed);
}

#[test]
fn duplicate_same_pattern_grants_do_not_double_count() {
    let (_registry, resolver, mut provider) = setup();
    let subject = Subject::new(SubjectKind::Player, "p1");
    provider.grant(&subject, Grant::allow(node("a.b")));
    provider.grant(&subject, Grant::allow(node("a.b")));
    provider.grant(&subject, Grant::allow(node("a.b")));
    assert!(resolve(&resolver, &provider, &subject, &node("a.b")).allowed);
}

const MAX_CONTEXT_VALUE_LEN_TEST: usize = 128;
