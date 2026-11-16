//! Registrar behavior: insert, remove, dedupe, order, clear.

use candyriya_messaging::ChannelIdentifier;
use candyriya_messaging_velocity::ChannelRegistrar;

fn id(s: &str) -> ChannelIdentifier {
    ChannelIdentifier::new(s).expect("test channels must be valid identifiers")
}

#[test]
fn empty_registrar_is_empty_and_unregistered() {
    let registrar = ChannelRegistrar::new();
    assert!(registrar.is_empty());
    assert_eq!(registrar.len(), 0);
    assert!(registrar.channels().is_empty());
    assert!(!registrar.is_registered(&id("bungeecord:main")));
}

#[test]
fn default_registrar_behaves_like_new() {
    let registrar = ChannelRegistrar::default();
    assert!(registrar.is_empty());
    assert_eq!(registrar.len(), 0);
}

#[test]
fn register_then_is_registered() {
    let mut registrar = ChannelRegistrar::new();
    let channel = id("custom:main");
    assert!(!registrar.is_registered(&channel));
    assert!(registrar.register(&channel));
    assert!(registrar.is_registered(&channel));
    assert_eq!(registrar.len(), 1);
}

#[test]
fn register_is_idempotent_for_duplicates() {
    let mut registrar = ChannelRegistrar::new();
    let channel = id("custom:main");
    assert!(registrar.register(&channel));
    assert!(
        !registrar.register(&channel),
        "second register must report no change"
    );
    assert!(!registrar.register(&channel));
    assert_eq!(registrar.len(), 1);
    assert_eq!(registrar.channels(), vec![channel]);
}

#[test]
fn unregister_removes_and_reports() {
    let mut registrar = ChannelRegistrar::new();
    let a = id("custom:a");
    let b = id("custom:b");
    registrar.register(&a);
    registrar.register(&b);

    assert!(registrar.unregister(&a));
    assert!(!registrar.is_registered(&a));
    assert!(registrar.is_registered(&b));
    assert_eq!(registrar.channels(), vec![b.clone()]);

    // unregistering again reports false and does not corrupt the set
    assert!(!registrar.unregister(&a));
    assert_eq!(registrar.channels(), vec![b]);
}

#[test]
fn unregister_of_never_registered_channel_is_false() {
    let mut registrar = ChannelRegistrar::new();
    assert!(!registrar.unregister(&id("custom:nope")));
    assert!(registrar.is_empty());
}

#[test]
fn clear_removes_everything() {
    let mut registrar = ChannelRegistrar::new();
    registrar.register(&id("custom:a"));
    registrar.register(&id("custom:b"));
    registrar.register(&id("BungeeCord"));
    assert_eq!(registrar.len(), 3);

    registrar.clear();
    assert!(registrar.is_empty());
    assert_eq!(registrar.len(), 0);
    assert!(registrar.channels().is_empty());
    assert!(!registrar.is_registered(&id("custom:a")));
}

#[test]
fn channels_preserve_registration_order() {
    let mut registrar = ChannelRegistrar::new();
    let a = id("custom:a");
    let b = id("bungeecord:main");
    let c = id("BungeeCord");
    let d = id("candyriya:main");

    registrar.register(&a);
    registrar.register(&b);
    registrar.register(&c);
    registrar.register(&d);
    assert_eq!(registrar.channels(), vec![a.clone(), b, c, d.clone()]);
}

#[test]
fn re_registered_channel_goes_to_the_back() {
    let mut registrar = ChannelRegistrar::new();
    let a = id("custom:a");
    let b = id("custom:b");
    let c = id("custom:c");
    registrar.register(&a);
    registrar.register(&b);
    registrar.register(&c);

    assert!(registrar.unregister(&b));
    assert_eq!(registrar.channels(), vec![a.clone(), c.clone()]);

    assert!(registrar.register(&b));
    assert_eq!(registrar.channels(), vec![a, c, b]);
}

#[test]
fn unregister_middle_element_keeps_relative_order() {
    let mut registrar = ChannelRegistrar::new();
    for name in ["custom:a", "custom:b", "custom:c", "custom:d", "custom:e"] {
        registrar.register(&id(name));
    }
    registrar.unregister(&id("custom:c"));
    registrar.unregister(&id("custom:a"));
    assert_eq!(
        registrar
            .channels()
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["custom:b", "custom:d", "custom:e"],
    );
}

#[test]
fn channels_returns_clones_safe_to_hold_while_mutating() {
    let mut registrar = ChannelRegistrar::new();
    registrar.register(&id("custom:a"));
    let snapshot = registrar.channels();
    registrar.clear();
    assert_eq!(snapshot, vec![id("custom:a")]);
    assert!(registrar.is_empty());
}

#[test]
fn registrar_accepts_both_bungeecord_channel_syntaxes() {
    let mut registrar = ChannelRegistrar::new();
    assert!(registrar.register(&id("bungeecord:main")));
    assert!(registrar.register(&id("BungeeCord")));
    assert_eq!(registrar.len(), 2);
    assert!(registrar.is_registered(&id("bungeecord:main")));
    assert!(registrar.is_registered(&id("BungeeCord")));
}
