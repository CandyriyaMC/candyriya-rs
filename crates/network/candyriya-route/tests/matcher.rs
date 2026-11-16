//! The forced-hosts matching matrix: exact, single-label wildcard, catch-all,
//! normalization, specificity ordering and declaration-order ties.

use candyriya_route::{ForcedHosts, RouteError};

fn table(entries: &[(&str, &[&str])]) -> ForcedHosts {
    ForcedHosts::new(
        entries
            .iter()
            .map(|(pattern, backends)| {
                (
                    pattern.to_string(),
                    backends.iter().map(|b| b.to_string()).collect(),
                )
            })
            .collect::<Vec<_>>(),
    )
    .expect("test table must build")
}

fn backends(resolved: Option<&[String]>) -> Option<Vec<&str>> {
    resolved.map(|names| names.iter().map(String::as_str).collect())
}

#[test]
fn exact_pattern_matches_only_that_host() {
    let hosts = table(&[("example.com", &["lobby"])]);
    assert_eq!(backends(hosts.resolve("example.com")), Some(vec!["lobby"]));
    assert_eq!(hosts.resolve("a.example.com"), None);
    assert_eq!(hosts.resolve("ample.com"), None);
}

#[test]
fn wildcard_covers_exactly_one_label() {
    let hosts = table(&[("*.example.com", &["wild"])]);
    assert_eq!(backends(hosts.resolve("a.example.com")), Some(vec!["wild"]));
    assert_eq!(backends(hosts.resolve("b.example.com")), Some(vec!["wild"]));
    assert_eq!(hosts.resolve("example.com"), None);
}

#[test]
fn wildcard_does_not_match_multiple_labels() {
    let hosts = table(&[("*.example.com", &["wild"])]);
    assert_eq!(hosts.resolve("a.b.example.com"), None);
    assert_eq!(hosts.resolve("a.b.c.example.com"), None);
    assert_eq!(hosts.resolve("deep.sub.example.com"), None);
}

#[test]
fn match_all_matches_anything() {
    let hosts = table(&[("*", &["everything"])]);
    assert_eq!(
        backends(hosts.resolve("a.b.c.example.com")),
        Some(vec!["everything"])
    );
    assert_eq!(
        backends(hosts.resolve("example.com")),
        Some(vec!["everything"])
    );
    assert_eq!(
        backends(hosts.resolve("localhost")),
        Some(vec!["everything"])
    );
}

#[test]
fn matching_is_case_insensitive() {
    let hosts = table(&[("Example.COM", &["lobby"]), ("*.EXAMPLE.org", &["wild"])]);
    assert_eq!(backends(hosts.resolve("EXAMPLE.com")), Some(vec!["lobby"]));
    assert_eq!(backends(hosts.resolve("A.EXAMPLE.org")), Some(vec!["wild"]));
    assert_eq!(backends(hosts.resolve("A.example.ORG")), Some(vec!["wild"]));
}

#[test]
fn trailing_dots_are_stripped_from_hosts_and_patterns() {
    let hosts = table(&[("example.com.", &["lobby"]), ("*.example.org.", &["wild"])]);
    assert_eq!(backends(hosts.resolve("example.com")), Some(vec!["lobby"]));
    assert_eq!(backends(hosts.resolve("example.com.")), Some(vec!["lobby"]));
    assert_eq!(backends(hosts.resolve("a.example.org")), Some(vec!["wild"]));
    assert_eq!(
        backends(hosts.resolve("a.example.org.")),
        Some(vec!["wild"])
    );
    // "example.com.." strips *all* trailing dots
    assert_eq!(
        backends(hosts.resolve("example.com..")),
        Some(vec!["lobby"])
    );
}

#[test]
fn most_specific_kind_wins() {
    let hosts = table(&[
        ("*", &["catchall"]),
        ("*.example.com", &["wild"]),
        ("example.com", &["exact"]),
    ]);
    assert_eq!(backends(hosts.resolve("example.com")), Some(vec!["exact"]));
    assert_eq!(backends(hosts.resolve("a.example.com")), Some(vec!["wild"]));
    assert_eq!(
        backends(hosts.resolve("a.example.net")),
        Some(vec!["catchall"])
    );
}

#[test]
fn single_label_wildcards_never_match_multi_level_hosts() {
    // with single-label semantics, *.com matches exactly X.com and
    // *.example.com matches exactly X.example.com; they never overlap
    let hosts = table(&[("*.com", &["wide"]), ("*.example.com", &["narrow"])]);
    assert_eq!(
        backends(hosts.resolve("a.example.com")),
        Some(vec!["narrow"])
    );
    assert_eq!(hosts.resolve("a.other.com"), None); // two labels before .com
    assert_eq!(hosts.resolve("b.a.example.com"), None);
    // a.com is exactly one label before .com, which matches *.com
    assert_eq!(backends(hosts.resolve("a.com")), Some(vec!["wide"]));
}

#[test]
fn exact_beats_wildcard_with_more_labels() {
    // both have two labels; the exact kind outranks the wildcard kind
    let hosts = table(&[("*.example.com", &["wild"]), ("example.com", &["exact"])]);
    assert_eq!(backends(hosts.resolve("example.com")), Some(vec!["exact"]));
}

#[test]
fn identical_patterns_are_rejected_as_duplicates() {
    let result = ForcedHosts::new(vec![
        ("*.example.com".to_string(), vec!["a".to_string()]),
        ("*.EXAMPLE.com.".to_string(), vec!["b".to_string()]),
    ]);
    assert!(
        matches!(result, Err(RouteError::DuplicatePattern(pattern)) if pattern == "*.example.com")
    );
}

#[test]
fn most_specific_beats_declaration_order() {
    // a later, more specific pattern must win over an earlier generic one
    let hosts = table(&[("*", &["catchall"]), ("*.com", &["dotcom"])]);
    assert_eq!(backends(hosts.resolve("a.com")), Some(vec!["dotcom"]));
    assert_eq!(backends(hosts.resolve("a.b.com")), Some(vec!["catchall"]));
}

#[test]
fn backends_keep_declaration_order() {
    let hosts = table(&[("example.com", &["s1", "s2", "s3"])]);
    assert_eq!(
        backends(hosts.resolve("example.com")),
        Some(vec!["s1", "s2", "s3"])
    );
}

#[test]
fn malformed_hosts_match_nothing() {
    let hosts = table(&[("*", &["everything"]), ("example.com", &["lobby"])]);
    assert_eq!(hosts.resolve(""), None);
    assert_eq!(hosts.resolve("."), None);
    assert_eq!(hosts.resolve("..."), None);
    assert_eq!(hosts.resolve("a..example.com"), None);
    assert_eq!(hosts.resolve("a example.com"), None);
    assert_eq!(hosts.resolve("a/example.com"), None);
    assert_eq!(hosts.resolve("a:b.com"), None);
}

#[test]
fn invalid_patterns_are_rejected() {
    let bad = [
        "",
        "   ",
        "a..com",
        "*.*",
        "a.*.com",
        "ex*mple.com",
        "*a.com",
        "a*.com",
        ".example.com",
        "a example.com",
        "example..com",
    ];
    for pattern in bad {
        let result = ForcedHosts::new(vec![(pattern.to_string(), vec!["a".to_string()])]);
        assert!(
            matches!(result, Err(RouteError::InvalidPattern { .. })),
            "pattern {pattern:?} must be rejected"
        );
    }
}

#[test]
fn empty_backend_list_is_rejected() {
    let result = ForcedHosts::new(vec![("example.com".to_string(), Vec::new())]);
    assert!(
        matches!(result, Err(RouteError::EmptyForcedBackends(pattern)) if pattern == "example.com")
    );
}

#[test]
fn unicode_labels_are_supported() {
    let hosts = table(&[("münchën.de", &["lobby"]), ("*.münchën.de", &["wild"])]);
    assert_eq!(backends(hosts.resolve("MÜNCHËN.de")), Some(vec!["lobby"]));
    assert_eq!(backends(hosts.resolve("x.münchën.de")), Some(vec!["wild"]));
}

#[test]
fn trailing_dot_pattern_is_the_same_as_bare_star() {
    // trailing dots are stripped for every pattern, so "*." is the catch-all
    let hosts = table(&[("*.", &["catchall"])]);
    assert_eq!(
        backends(hosts.resolve("anything.valid")),
        Some(vec!["catchall"])
    );
    // a host that fails normalization matches nothing; the caller falls back
    // to the try list in that case
    assert_eq!(hosts.resolve("*.a"), None);
}

#[test]
fn iter_yields_declaration_order() {
    let hosts = table(&[("b.com", &["b"]), ("a.com", &["a"]), ("*", &["all"])]);
    let patterns: Vec<&str> = hosts.iter().map(|entry| entry.pattern()).collect();
    assert_eq!(patterns, vec!["b.com", "a.com", "*"]);
}
