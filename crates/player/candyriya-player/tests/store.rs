//! In-memory store behavior: upsert, sighting timestamps, trait contract.
//! No network anywhere in this file.

use candyriya_player::{GameProfile, InMemoryPlayerStore, PlayerStore, Property, offline_uuid};
use chrono::{DateTime, Utc};

fn at(ts: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(ts, 0).expect("valid timestamp")
}

fn profile(name: &str) -> GameProfile {
    GameProfile {
        id: offline_uuid(name),
        name: name.to_string(),
        properties: vec![Property::unsigned("textures", "dmFsdWU=")],
    }
}

#[test]
fn upsert_new_player_sets_both_sightings() {
    let mut store = InMemoryPlayerStore::new();
    let stored = store.upsert_from_session(&profile("Notch"), at(1_000));
    assert_eq!(stored.first_seen, at(1_000));
    assert_eq!(stored.last_seen, at(1_000));
    assert_eq!(store.len(), 1);
    assert!(!store.is_empty());
}

#[test]
fn upsert_keeps_first_seen_and_bumps_last_seen() {
    let mut store = InMemoryPlayerStore::new();
    let p = profile("Notch");
    store.upsert_from_session(&p, at(1_000));
    let stored = store.upsert_from_session(&p, at(2_000));

    assert_eq!(stored.first_seen, at(1_000), "first sighting is kept");
    assert_eq!(stored.last_seen, at(2_000), "last sighting advances");
}

#[test]
fn upsert_last_seen_never_regresses_on_clock_skew() {
    let mut store = InMemoryPlayerStore::new();
    let p = profile("Notch");
    store.upsert_from_session(&p, at(2_000));
    let stored = store.upsert_from_session(&p, at(1_000));

    assert_eq!(
        stored.last_seen,
        at(2_000),
        "last_seen must not go backwards"
    );
    assert_eq!(
        stored.first_seen,
        at(2_000),
        "unchanged record keeps first_seen too"
    );
}

#[test]
fn upsert_overwrites_name_and_properties_with_latest_sighting() {
    let mut store = InMemoryPlayerStore::new();

    let mut first = profile("Notch");
    first.properties = vec![Property::unsigned("textures", "b2xk")];
    store.upsert_from_session(&first, at(1_000));

    // Mojang account names change; UUID does not.
    let mut second = GameProfile::new(first.id, "Notchy");
    second.properties = vec![Property::unsigned("textures", "bmV3")];
    let stored = store.upsert_from_session(&second, at(2_000));

    assert_eq!(stored.name, "Notchy");
    assert_eq!(stored.properties[0].value, "bmV3");
    assert_eq!(stored.first_seen, at(1_000));
    assert_eq!(stored.uuid, first.id);
}

#[test]
fn trait_load_roundtrip() {
    let mut store = InMemoryPlayerStore::new();
    let p = profile("jeb_");
    let expected = store.upsert_from_session(&p, at(1_500));

    let loaded = store
        .load(p.id)
        .expect("in-memory load never fails")
        .expect("player was upserted");
    assert_eq!(loaded, expected);
}

#[test]
fn trait_load_unknown_uuid_is_none() {
    let store = InMemoryPlayerStore::new();
    let loaded = store.load(offline_uuid("Herobrine")).expect("never fails");
    assert_eq!(loaded, None);
}

#[test]
fn trait_save_replaces_wholesale() {
    let mut store = InMemoryPlayerStore::new();
    let p = profile("Notch");
    store.upsert_from_session(&p, at(1_000));

    let mut replacement = store.load(p.id).expect("ok").expect("present");
    replacement.name = "Replaced".to_string();
    replacement.first_seen = at(5_000);
    store.save(replacement).expect("in-memory save never fails");

    let loaded = store.load(p.id).expect("ok").expect("present");
    assert_eq!(loaded.name, "Replaced");
    assert_eq!(
        loaded.first_seen,
        at(5_000),
        "save replaces wholesale, no merge"
    );
}
