//! Registry semantics: construction validation, try chain ordering,
//! fallback boundaries, forced-hosts wiring.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use candyriya_route::{BackendRegistry, BackendState, ForcedHosts, RouteError};

fn addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
}

fn registry() -> BackendRegistry {
    BackendRegistry::new(
        vec![
            ("lobby".to_string(), addr(25565)),
            ("minigames".to_string(), addr(25566)),
            ("survival".to_string(), addr(25567)),
        ],
        vec![
            "lobby".to_string(),
            "minigames".to_string(),
            "survival".to_string(),
        ],
    )
    .expect("registry must build")
}

#[test]
fn registry_keeps_registration_order() {
    let registry = registry();
    let names: Vec<&str> = registry.iter().map(|backend| backend.name()).collect();
    assert_eq!(names, vec!["lobby", "minigames", "survival"]);
    assert_eq!(registry.len(), 3);
    assert!(!registry.is_empty());
}

#[test]
fn state_roundtrip() {
    let registry = registry();
    assert_eq!(registry.state("lobby"), Some(BackendState::Unknown));
    registry
        .set_state("lobby", BackendState::Online)
        .expect("known backend");
    assert_eq!(registry.state("lobby"), Some(BackendState::Online));
    registry
        .set_state("lobby", BackendState::Offline)
        .expect("known backend");
    assert_eq!(registry.state("lobby"), Some(BackendState::Offline));
    assert_eq!(registry.state("no-such-backend"), None);
    assert!(matches!(
        registry.set_state("no-such-backend", BackendState::Online),
        Err(RouteError::UnknownBackend(name)) if name == "no-such-backend"
    ));
}

#[test]
fn try_chain_preserves_configuration_order() {
    let registry = BackendRegistry::new(
        vec![
            ("b".to_string(), addr(2)),
            ("a".to_string(), addr(1)),
            ("c".to_string(), addr(3)),
        ],
        // try order differs from registration order on purpose
        vec!["c".to_string(), "a".to_string(), "b".to_string()],
    )
    .expect("registry must build");
    assert_eq!(registry.try_chain(), &["c", "a", "b"]);
}

#[test]
fn next_try_walks_the_chain_then_stops() {
    let registry = registry();
    let names: Vec<&str> = (0..10)
        .map_while(|attempts| registry.next_try(attempts))
        .map(|backend| backend.name())
        .collect();
    assert_eq!(names, vec!["lobby", "minigames", "survival"]);
    assert!(registry.next_try(3).is_none());
    assert!(registry.next_try(100).is_none());
    assert!(registry.next_try(usize::MAX).is_none());
}

#[test]
fn duplicate_backend_is_rejected() {
    let result = BackendRegistry::new(
        vec![
            ("lobby".to_string(), addr(1)),
            ("lobby".to_string(), addr(2)),
        ],
        vec!["lobby".to_string()],
    );
    assert!(matches!(result, Err(RouteError::DuplicateBackend(name)) if name == "lobby"));
}

#[test]
fn empty_try_list_is_rejected() {
    let result = BackendRegistry::new(vec![("lobby".to_string(), addr(1))], Vec::new());
    assert!(matches!(result, Err(RouteError::EmptyTryList)));
}

#[test]
fn unknown_backend_in_try_list_is_rejected() {
    let result = BackendRegistry::new(
        vec![("lobby".to_string(), addr(1))],
        vec!["lobby".to_string(), "ghost".to_string()],
    );
    assert!(matches!(result, Err(RouteError::UnknownTryBackend(name)) if name == "ghost"));
}

#[test]
fn duplicate_backend_in_try_list_is_rejected() {
    let result = BackendRegistry::new(
        vec![
            ("lobby".to_string(), addr(1)),
            ("other".to_string(), addr(2)),
        ],
        vec![
            "lobby".to_string(),
            "other".to_string(),
            "lobby".to_string(),
        ],
    );
    assert!(matches!(result, Err(RouteError::DuplicateTryBackend(name)) if name == "lobby"));
}

#[test]
fn invalid_backend_names_are_rejected() {
    for bad in ["", "has space", "with/slash", "tab\there", &"x".repeat(65)] {
        let result = BackendRegistry::new(vec![(bad.to_string(), addr(1))], vec![bad.to_string()]);
        assert!(
            matches!(result, Err(RouteError::InvalidBackendName(_))),
            "name {bad:?} must be rejected"
        );
    }
}

#[test]
fn unicode_alphanumeric_backend_names_are_accepted() {
    let registry = BackendRegistry::new(
        vec![("лаундж-1".to_string(), addr(1))],
        vec!["лаундж-1".to_string()],
    );
    assert!(registry.is_ok());
}

#[test]
fn resolve_host_without_forced_hosts_returns_none() {
    let registry = registry();
    assert_eq!(registry.resolve_host("play.example.com"), None);
}

#[test]
fn forced_hosts_route_to_listed_backends() {
    let mut registry = registry();
    let forced = ForcedHosts::new(vec![(
        "*.example.com".to_string(),
        vec!["minigames".to_string(), "survival".to_string()],
    )])
    .expect("forced hosts must build");
    registry.set_forced_hosts(forced).expect("backends exist");
    assert_eq!(
        registry.resolve_host("party.example.com"),
        Some(&["minigames".to_string(), "survival".to_string()][..])
    );
    assert_eq!(registry.resolve_host("other.net"), None);
    // try chain is untouched by forced hosts
    assert_eq!(registry.try_chain(), &["lobby", "minigames", "survival"]);
}

#[test]
fn forced_hosts_referencing_unknown_backends_are_rejected() {
    let mut registry = registry();
    let forced = ForcedHosts::new(vec![(
        "games.example.com".to_string(),
        vec!["ghost".to_string()],
    )])
    .expect("forced hosts must build");
    assert!(matches!(
        registry.set_forced_hosts(forced),
        Err(RouteError::UnknownForcedBackend { host, backend })
            if host == "games.example.com" && backend == "ghost"
    ));
    // the rejected table is not installed
    assert!(registry.forced_hosts().is_empty());
}

#[test]
fn forced_hosts_can_be_replaced() {
    let mut registry = registry();
    let first = ForcedHosts::new(vec![(
        "a.example.com".to_string(),
        vec!["lobby".to_string()],
    )])
    .expect("build");
    registry.set_forced_hosts(first).expect("valid");
    let second = ForcedHosts::new(vec![(
        "b.example.com".to_string(),
        vec!["survival".to_string()],
    )])
    .expect("build");
    registry.set_forced_hosts(second).expect("valid");
    assert_eq!(registry.resolve_host("a.example.com"), None);
    assert_eq!(
        registry.resolve_host("b.example.com"),
        Some(&["survival".to_string()][..])
    );
}

#[test]
fn get_returns_backend_with_matching_address() {
    let registry = registry();
    let backend = registry.get("minigames").expect("registered");
    assert_eq!(backend.addr(), addr(25566));
    assert!(registry.get("ghost").is_none());
}
