//! Integration tests for [`JsonPlayerStore`].
//!
//! Everything runs against a fresh `tempdir`; no network, no shared state.

use std::path::{Path, PathBuf};

use candyriya_player::{GameProfile, OfflinePlayer, PlayerStore, Property};
use candyriya_player_store::{JsonPlayerStore, StoreError};
use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;

fn ts(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("valid rfc3339 in test")
        .with_timezone(&Utc)
}

fn profile(id: u128, name: &str, properties: Vec<Property>) -> GameProfile {
    GameProfile {
        id: Uuid::from_u128(id),
        name: name.to_string(),
        properties,
    }
}

fn record(id: u128, name: &str) -> OfflinePlayer {
    OfflinePlayer {
        uuid: Uuid::from_u128(id),
        name: name.to_string(),
        properties: vec![Property::signed("textures", "dmFsdWU=", "c2ln")],
        first_seen: ts("2026-01-01T00:00:00Z"),
        last_seen: ts("2026-01-02T00:00:00Z"),
    }
}

fn store_path(dir: &Path) -> PathBuf {
    dir.join("players.json")
}

fn tmp_litter(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect()
}

#[test]
fn open_missing_file_yields_empty_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    let store = JsonPlayerStore::open(&path).expect("open");
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
    assert!(store.load_all().is_empty());
    assert_eq!(store.path(), path);
}

#[test]
fn missing_parent_dirs_are_created_on_open_and_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir
        .path()
        .join("nested")
        .join("deeper")
        .join("data")
        .join("players.json");
    let mut store = JsonPlayerStore::open(&path).expect("open creates parent dirs");
    assert!(path.parent().expect("parent").exists());
    assert!(!path.exists(), "open does not create the file itself");

    store
        .upsert_from_session(&profile(1, "Alice", vec![]), ts("2026-01-01T00:00:00Z"))
        .expect("upsert");
    assert!(path.is_file(), "save creates the file");
    assert!(path.parent().expect("parent").exists());
}

#[test]
fn roundtrip_many_records_signed_and_unsigned_properties() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    for i in 1..=50u128 {
        let properties = match i % 3 {
            0 => vec![],
            1 => vec![Property::signed(
                "textures",
                format!("dmFsdWUt{i}"),
                format!("c2ln-{i}"),
            )],
            _ => vec![
                Property::unsigned("textures", format!("dmFsdWU-{i}")),
                Property::signed("cape", "Y2FwZV92YWx1ZQ==", "Y2FwZV9zaWc="),
            ],
        };
        store
            .upsert_from_session(
                &profile(i, &format!("Player{i}"), properties),
                ts("2026-02-01T00:00:00Z"),
            )
            .expect("upsert");
    }
    drop(store);

    // reload from disk and compare against the in-memory view
    let reopened = JsonPlayerStore::open(&path).expect("reopen");
    let map = reopened.load_all();
    assert_eq!(map.len(), 50);
    for i in 1..=50u128 {
        let record = map.get(&Uuid::from_u128(i)).expect("record present");
        assert_eq!(record.name, format!("Player{i}"));
        match i % 3 {
            0 => assert!(record.properties.is_empty()),
            1 => {
                assert_eq!(record.properties.len(), 1);
                assert!(record.properties[0].signature.is_some());
            }
            _ => {
                assert_eq!(record.properties.len(), 2);
                assert!(record.properties[0].signature.is_none());
                assert!(record.properties[1].signature.is_some());
            }
        }
    }
}

#[test]
fn roundtrip_preserves_records_written_by_serialization() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    let original = record(42, "CraftAnnotate");
    PlayerStore::save(&mut store, original.clone()).expect("save");
    drop(store);

    let reopened = JsonPlayerStore::open(&path).expect("reopen");
    assert_eq!(
        PlayerStore::load(&reopened, original.uuid),
        Ok(Some(original))
    );
}

#[test]
fn file_is_pretty_printed_two_space_and_uuid_ordered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    PlayerStore::save(&mut store, record(9, "Nine")).expect("save nine");
    PlayerStore::save(&mut store, record(1, "One")).expect("save one");
    drop(store);

    let raw = std::fs::read_to_string(&path).expect("read raw");
    assert!(raw.contains('\n'), "pretty-printed output must span lines");
    assert!(!raw.contains('\t'), "indentation is spaces, not tabs");
    assert!(raw.contains("\n  {"), "array items are indented two spaces");
    let idx_one = raw
        .find(&Uuid::from_u128(1).to_string())
        .expect("uuid 1 present");
    let idx_nine = raw
        .find(&Uuid::from_u128(9).to_string())
        .expect("uuid 9 present");
    assert!(idx_one < idx_nine, "records are written in UUID order");
}

#[test]
fn unsigned_properties_serialize_without_signature_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    let mut rec = record(7, "Unsigned");
    rec.properties = vec![Property::unsigned("textures", "dmFsdWU=")];
    PlayerStore::save(&mut store, rec).expect("save");
    drop(store);

    let raw = std::fs::read_to_string(&path).expect("read raw");
    assert!(
        !raw.contains("signature"),
        "unsigned property omits the signature key"
    );
}

#[test]
fn atomic_rename_leaves_no_tmp_litter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    for i in 1..=10u128 {
        PlayerStore::save(&mut store, record(i, &format!("Player{i}"))).expect("save");
        store
            .upsert_from_session(
                &profile(100 + i, &format!("Session{i}"), vec![]),
                ts("2026-03-01T00:00:00Z"),
            )
            .expect("upsert");
        assert_eq!(
            tmp_litter(dir.path()),
            Vec::<String>::new(),
            "no tmp litter after save {i}"
        );
    }
}

#[test]
fn corrupt_file_garbage_bytes_errors_not_panics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    std::fs::write(&path, [0xde, 0xad, 0xbe, 0xef, 0x00, 0x42]).expect("write garbage");

    let result = JsonPlayerStore::open(&path);
    assert!(
        matches!(result, Err(StoreError::Corrupt { .. })),
        "garbage must be Corrupt, got {result:?}"
    );
    // refused file is left untouched
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x42]
    );
}

#[test]
fn corrupt_file_valid_json_wrong_shape_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    std::fs::write(&path, br#"[{"uuid": 42, "name": "x"}]"#).expect("write wrong shape");

    let result = JsonPlayerStore::open(&path);
    assert!(
        matches!(result, Err(StoreError::Corrupt { .. })),
        "wrong shape must be Corrupt, got {result:?}"
    );
}

#[test]
fn corrupt_file_with_one_bad_record_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    let good = record(1, "Good");
    let mut good_json = serde_json::to_value(&good).expect("to_value");
    good_json["last_seen"] = json!("not-a-timestamp");
    let raw = format!(
        "[{}]",
        serde_json::to_string(&good_json).expect("to_string")
    );
    std::fs::write(&path, raw).expect("write bad record");

    let result = JsonPlayerStore::open(&path);
    assert!(
        matches!(result, Err(StoreError::Corrupt { .. })),
        "one bad record must fail the load, got {result:?}"
    );
}

#[test]
fn corrupt_file_truncated_json_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    let full = serde_json::to_string_pretty(&vec![record(1, "One")]).expect("serialize");
    std::fs::write(&path, &full[..full.len() / 2]).expect("write truncated");

    let result = JsonPlayerStore::open(&path);
    assert!(
        matches!(result, Err(StoreError::Corrupt { .. })),
        "truncated file must be Corrupt, got {result:?}"
    );
}

#[test]
fn duplicate_uuids_error_reports_all_indices() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    let dup = record(777, "Dup");
    let other = record(888, "Other");
    let raw = serde_json::to_string_pretty(&vec![dup.clone(), other, dup.clone(), dup])
        .expect("serialize");
    std::fs::write(&path, raw).expect("write duplicates");

    match JsonPlayerStore::open(&path) {
        Err(StoreError::DuplicateUuid { uuid, indices, .. }) => {
            assert_eq!(uuid, Uuid::from_u128(777));
            assert_eq!(indices, vec![0, 2, 3]);
        }
        other => panic!("expected DuplicateUuid, got {other:?}"),
    }
}

#[test]
fn duplicate_uuids_reports_earliest_first_occurrence_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    let a = record(300, "A");
    let b = record(200, "B");
    // b appears twice at indices 1 and 3, a twice at 0 and 2: a reported first
    let raw = serde_json::to_string_pretty(&vec![a.clone(), b.clone(), a, b]).expect("serialize");
    std::fs::write(&path, raw).expect("write duplicates");

    match JsonPlayerStore::open(&path) {
        Err(StoreError::DuplicateUuid { uuid, indices, .. }) => {
            assert_eq!(uuid, Uuid::from_u128(300));
            assert_eq!(indices, vec![0, 2]);
        }
        other => panic!("expected DuplicateUuid, got {other:?}"),
    }
}

#[test]
fn empty_file_yields_empty_map() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    std::fs::write(&path, b"").expect("write empty");

    let store = JsonPlayerStore::open(&path).expect("empty file opens");
    assert!(store.is_empty());
}

#[test]
fn whitespace_only_file_yields_empty_map() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    std::fs::write(&path, b"  \n\n  \t\n").expect("write whitespace");

    let store = JsonPlayerStore::open(&path).expect("whitespace file opens");
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
}

#[test]
fn save_after_open_reload_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    {
        let mut store = JsonPlayerStore::open(&path).expect("open");
        store
            .upsert_from_session(
                &profile(
                    1,
                    "Alice",
                    vec![Property::signed("textures", "dmFsdWU=", "c2ln")],
                ),
                ts("2026-01-01T00:00:00Z"),
            )
            .expect("first upsert");
    }

    let mut reopened = JsonPlayerStore::open(&path).expect("reload");
    assert_eq!(reopened.len(), 1);
    reopened
        .upsert_from_session(&profile(1, "Alicia", vec![]), ts("2026-02-01T00:00:00Z"))
        .expect("second upsert");

    let final_store = JsonPlayerStore::open(&path).expect("final reload");
    let record = final_store
        .load_all()
        .get(&Uuid::from_u128(1))
        .expect("record")
        .clone();
    assert_eq!(record.name, "Alicia");
    assert_eq!(record.first_seen, ts("2026-01-01T00:00:00Z"));
    assert_eq!(record.last_seen, ts("2026-02-01T00:00:00Z"));
    assert!(record.properties.is_empty());
}

#[test]
fn upsert_keeps_first_seen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    store
        .upsert_from_session(&profile(1, "Alice", vec![]), ts("2026-01-01T00:00:00Z"))
        .expect("first");
    let updated = store
        .upsert_from_session(&profile(1, "Alice", vec![]), ts("2026-06-01T00:00:00Z"))
        .expect("second");
    assert_eq!(updated.first_seen, ts("2026-01-01T00:00:00Z"));
    assert_eq!(updated.last_seen, ts("2026-06-01T00:00:00Z"));
}

#[test]
fn upsert_last_seen_never_regresses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    store
        .upsert_from_session(&profile(1, "Alice", vec![]), ts("2026-06-01T00:00:00Z"))
        .expect("first");
    // clock went backwards: last_seen must stay at the stored maximum
    let updated = store
        .upsert_from_session(&profile(1, "Alice", vec![]), ts("2026-01-01T00:00:00Z"))
        .expect("second");
    assert_eq!(updated.last_seen, ts("2026-06-01T00:00:00Z"));
}

#[test]
fn upsert_overwrites_name_and_properties() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    store
        .upsert_from_session(
            &profile(
                1,
                "Alice",
                vec![Property::signed("textures", "b2xk", "c2ln")],
            ),
            ts("2026-01-01T00:00:00Z"),
        )
        .expect("first");
    let updated = store
        .upsert_from_session(
            &profile(1, "Alicia", vec![Property::unsigned("textures", "bmV3")]),
            ts("2026-02-01T00:00:00Z"),
        )
        .expect("second");
    assert_eq!(updated.name, "Alicia");
    assert_eq!(
        updated.properties,
        vec![Property::unsigned("textures", "bmV3")]
    );
}

#[test]
fn upsert_keys_records_by_uuid_not_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    // same uuid, different name -> one record, newest name wins
    store
        .upsert_from_session(&profile(1, "Alice", vec![]), ts("2026-01-01T00:00:00Z"))
        .expect("first");
    store
        .upsert_from_session(&profile(1, "Alicia", vec![]), ts("2026-02-01T00:00:00Z"))
        .expect("rename");
    // different uuid, same name -> two records
    store
        .upsert_from_session(&profile(2, "Alicia", vec![]), ts("2026-03-01T00:00:00Z"))
        .expect("other account");

    assert_eq!(store.len(), 2);
    let map = store.load_all();
    assert_eq!(map.get(&Uuid::from_u128(1)).expect("one").name, "Alicia");
    assert_eq!(map.get(&Uuid::from_u128(2)).expect("two").name, "Alicia");
}

#[test]
fn save_failure_rolls_back_in_memory_map() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    PlayerStore::save(&mut store, record(1, "Stable")).expect("initial save");

    // sabotage persistence: replace the file with a directory so the
    // final rename step of persist() fails
    std::fs::remove_file(&path).expect("remove file");
    std::fs::create_dir(&path).expect("create dir in its place");

    let result = PlayerStore::save(&mut store, record(2, "Lost"));
    assert!(result.is_err(), "save must report the failure");
    assert!(
        PlayerStore::load(&store, Uuid::from_u128(2))
            .expect("load")
            .is_none(),
        "failed save is not visible in memory"
    );
    // pre-existing record survives the rollback
    assert_eq!(
        PlayerStore::load(&store, Uuid::from_u128(1)).expect("load"),
        Some(record(1, "Stable"))
    );
    // no tmp litter from the failed rename
    assert_eq!(tmp_litter(dir.path()), Vec::<String>::new());
}

#[test]
fn open_on_directory_path_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("players.json");
    std::fs::create_dir(&path).expect("create dir");

    let result = JsonPlayerStore::open(&path);
    assert!(
        matches!(result, Err(StoreError::Io { .. })),
        "directory path must be Io, got {result:?}"
    );
}

#[test]
fn duplicate_detection_runs_before_any_record_is_kept() {
    // a store with a duplicate uuid two positions apart in a larger array
    // must be rejected wholesale, and the refused file must be left alone
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    let records = vec![
        record(1, "One"),
        record(2, "Two"),
        record(3, "Three"),
        record(1, "OneAgain"),
    ];
    let raw = serde_json::to_string_pretty(&records).expect("serialize");
    std::fs::write(&path, &raw).expect("write");

    match JsonPlayerStore::open(&path) {
        Err(StoreError::DuplicateUuid { indices, .. }) => assert_eq!(indices, vec![0, 3]),
        other => panic!("expected DuplicateUuid, got {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&path).expect("read back"), raw);
}

#[test]
fn map_order_is_stable_across_reloads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());

    let mut store = JsonPlayerStore::open(&path).expect("open");
    for id in [30u128, 10, 20] {
        PlayerStore::save(&mut store, record(id, &format!("R{id}"))).expect("save");
    }
    drop(store);

    let reopened = JsonPlayerStore::open(&path).expect("reopen");
    let keys: Vec<Uuid> = reopened.load_all().into_keys().collect();
    let expected: Vec<Uuid> = {
        let mut sorted = vec![
            Uuid::from_u128(10),
            Uuid::from_u128(20),
            Uuid::from_u128(30),
        ];
        sorted.sort();
        sorted
    };
    assert_eq!(keys, expected);
}

#[test]
fn trait_load_unknown_uuid_is_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    let store = JsonPlayerStore::open(&path).expect("open");

    assert_eq!(PlayerStore::load(&store, Uuid::from_u128(999)), Ok(None));
}
