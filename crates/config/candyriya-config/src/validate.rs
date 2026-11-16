//! Semantic validation.
//!
//! Extraction guarantees the file parses and the types line up; this pass
//! catches everything serde can't express: port ranges, cross-field
//! references, control characters and the few magic-valued fields. Runs on
//! the extracted [`Config`] after figment decoding, so every problem that
//! can be detected is collected into one error instead of one at a time.

use std::net::IpAddr;

use crate::config::{Config, SUPPORTED_CONFIG_VERSION};
use crate::error::ValidationProblem;

/// True when the string contains ASCII or Unicode control characters.
fn has_control_chars(value: &str) -> bool {
    value.chars().any(char::is_control)
}

/// True when the string contains ASCII whitespace.
fn has_whitespace(value: &str) -> bool {
    value.chars().any(char::is_whitespace)
}

/// DNS hostname check (no wildcards, no underscores, labels 1..=63, the
/// whole thing at most 253 bytes). IPs never go through here.
pub(crate) fn is_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Parses `host:port` (IPv6 must be bracketed: `[::1]:25565`). The port
/// must be in `1..=65535`. Returns the host part on success.
pub(crate) fn parse_host_port(
    field: &str,
    value: &str,
) -> Result<(String, u16), ValidationProblem> {
    if has_control_chars(value) {
        return Err(ValidationProblem::new(
            field,
            "must not contain control characters",
        ));
    }

    let (host, port_text) = if let Some(rest) = value.strip_prefix('[') {
        let close = rest.find(']').ok_or_else(|| {
            ValidationProblem::new(
                field,
                "unterminated IPv6 literal — expected `[address]:port`",
            )
        })?;
        let host = &rest[..close];
        let port_text = rest[close + 1..].strip_prefix(':').ok_or_else(|| {
            ValidationProblem::new(field, "expected `:port` after the closing bracket")
        })?;
        (host, port_text)
    } else {
        value
            .rsplit_once(':')
            .ok_or_else(|| ValidationProblem::new(field, "expected `host:port`"))?
    };

    if host.is_empty() {
        return Err(ValidationProblem::new(field, "host must not be empty"));
    }
    if has_whitespace(host) {
        return Err(ValidationProblem::new(
            field,
            "host must not contain whitespace",
        ));
    }
    if host.parse::<IpAddr>().is_err() && !is_valid_hostname(host) {
        return Err(ValidationProblem::new(
            field,
            format!("`{host}` is not a valid IP address or hostname"),
        ));
    }

    let port: u32 = port_text.parse().map_err(|_| {
        ValidationProblem::new(field, format!("port `{port_text}` is not a number"))
    })?;
    if !(1..=65535).contains(&port) {
        return Err(ValidationProblem::new(
            field,
            format!("port {port} is out of range 1..=65535"),
        ));
    }

    Ok((host.to_string(), port as u16))
}

/// True when the string parses as an absolute http(s) URL with a host.
fn is_valid_http_url(value: &str) -> bool {
    if has_whitespace(value) || has_control_chars(value) {
        return false;
    }
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    // host must be present and contain no path-within-host weirdness
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty()
}

fn check_no_control(problems: &mut Vec<ValidationProblem>, field: &str, value: &str) {
    if has_control_chars(value) {
        problems.push(ValidationProblem::new(
            field,
            "must not contain control characters",
        ));
    }
}

/// Runs every semantic check against the extracted config and returns the
/// collected problems (empty on success).
pub(crate) fn validate(config: &Config) -> Vec<ValidationProblem> {
    let mut problems = Vec::new();

    if config.config_version != SUPPORTED_CONFIG_VERSION {
        problems.push(ValidationProblem::new(
            "config-version",
            format!("unsupported version `{}` — this build understands only `{SUPPORTED_CONFIG_VERSION}`", config.config_version),
        ));
    }

    if let Err(problem) = parse_host_port("bind", &config.bind) {
        problems.push(problem);
    }

    check_no_control(&mut problems, "motd", &config.motd);

    if config.show_max_players == 0 {
        problems.push(ValidationProblem::new(
            "show-max-players",
            "must be at least 1",
        ));
    }

    check_no_control(
        &mut problems,
        "forwarding-secret-file",
        &config.forwarding_secret_file,
    );
    if config.forwarding_secret_file.trim().is_empty() {
        problems.push(ValidationProblem::new(
            "forwarding-secret-file",
            "must not be empty",
        ));
    }

    if config.locale.is_empty()
        || config.locale.len() > 32
        || !config.locale.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        problems.push(ValidationProblem::new(
            "locale",
            format!(
                "`{}` must be 1..=32 chars of lowercase a-z, digits, `-` or `_`",
                config.locale
            ),
        ));
    }

    if !is_valid_http_url(&config.protocol.update_url) {
        problems.push(ValidationProblem::new(
            "protocol.update-url",
            "must be an absolute http(s) URL without whitespace",
        ));
    }

    validate_servers(config, &mut problems);
    validate_forced_hosts(config, &mut problems);
    validate_advanced(config, &mut problems);
    validate_query(config, &mut problems);
    validate_permissions(config, &mut problems);
    validate_metrics(config, &mut problems);
    validate_runtime(config, &mut problems);

    problems
}

fn validate_servers(config: &Config, problems: &mut Vec<ValidationProblem>) {
    for (name, address) in &config.servers.servers {
        if name.is_empty() || has_control_chars(name) || has_whitespace(name) {
            problems.push(ValidationProblem::new(
                format!("servers.{name}"),
                "server names must be non-empty and contain no whitespace or control characters",
            ));
        }
        if let Err(problem) = parse_host_port(&format!("servers.{name}"), address) {
            problems.push(problem);
        }
    }

    for (index, name) in config.servers.try_servers.iter().enumerate() {
        check_no_control(problems, &format!("servers.try[{index}]"), name);
        if !config.servers.servers.contains_key(name) {
            problems.push(ValidationProblem::new(
                "servers.try",
                format!("server `{name}` referenced in `try` does not exist under [servers]"),
            ));
        }
    }

    if !config.servers.servers.is_empty() && config.servers.try_servers.is_empty() {
        problems.push(ValidationProblem::new(
            "servers.try",
            "`try` must be non-empty when at least one server is configured",
        ));
    }
}

fn validate_forced_hosts(config: &Config, problems: &mut Vec<ValidationProblem>) {
    for (hostname, servers) in &config.forced_hosts {
        if hostname.is_empty() || has_control_chars(hostname) || has_whitespace(hostname) {
            problems.push(ValidationProblem::new(
                format!("forced-hosts.{hostname}"),
                "hostnames must be non-empty and contain no whitespace or control characters",
            ));
        }
        if servers.is_empty() {
            problems.push(ValidationProblem::new(
                format!("forced-hosts.{hostname}"),
                "server list must not be empty",
            ));
        }
        for name in servers {
            check_no_control(problems, &format!("forced-hosts.{hostname}"), name);
            if !config.servers.servers.contains_key(name) {
                problems.push(ValidationProblem::new(
                    format!("forced-hosts.{hostname}"),
                    format!("server `{name}` does not exist under [servers]"),
                ));
            }
        }
    }
}

fn validate_advanced(config: &Config, problems: &mut Vec<ValidationProblem>) {
    let advanced = &config.advanced;
    if advanced.compression_threshold < -1 {
        problems.push(ValidationProblem::new(
            "advanced.compression-threshold",
            format!(
                "{} is out of range; -1 disables compression, 0+ sets the threshold in bytes",
                advanced.compression_threshold
            ),
        ));
    }
    if !(-1..=9).contains(&advanced.compression_level) {
        problems.push(ValidationProblem::new(
            "advanced.compression-level",
            format!("{} is out of range -1..=9", advanced.compression_level),
        ));
    }
}

fn validate_query(config: &Config, problems: &mut Vec<ValidationProblem>) {
    if !(1..=65535).contains(&config.query.port) {
        problems.push(ValidationProblem::new(
            "query.port",
            format!("{} is out of range 1..=65535", config.query.port),
        ));
    }
    check_no_control(problems, "query.map", &config.query.map);
    if config.query.map.is_empty() {
        problems.push(ValidationProblem::new("query.map", "must not be empty"));
    }
}

fn validate_permissions(config: &Config, problems: &mut Vec<ValidationProblem>) {
    for (field, value) in [
        ("permissions.provider", &config.permissions.provider),
        ("permissions.default", &config.permissions.default),
    ] {
        check_no_control(problems, field, value);
        if value.is_empty() {
            problems.push(ValidationProblem::new(field, "must not be empty"));
        }
    }
    for (group, grants) in &config.permissions.grants {
        if group.is_empty() || has_control_chars(group) || has_whitespace(group) {
            problems.push(ValidationProblem::new(
                format!("permissions.grants.{group}"),
                "group names must be non-empty and contain no whitespace or control characters",
            ));
        }
        for grant in grants {
            if grant.is_empty() || has_control_chars(grant) {
                problems.push(ValidationProblem::new(
                    format!("permissions.grants.{group}"),
                    "permission nodes must be non-empty and contain no control characters",
                ));
            }
        }
    }
}

fn validate_metrics(config: &Config, problems: &mut Vec<ValidationProblem>) {
    if let Err(problem) = parse_host_port(
        "metrics.prometheus-address",
        &config.metrics.prometheus_address,
    ) {
        problems.push(problem);
    }
}

fn validate_runtime(config: &Config, problems: &mut Vec<ValidationProblem>) {
    if config.runtime.worker_threads == 0 {
        problems.push(ValidationProblem::new(
            "runtime.worker-threads",
            "must be at least 1",
        ));
    }
    if config.runtime.blocking_threads == 0 {
        problems.push(ValidationProblem::new(
            "runtime.blocking-threads",
            "must be at least 1",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_rules() {
        assert!(is_valid_hostname("example.com"));
        assert!(is_valid_hostname("a-b.c-d.example.org"));
        assert!(is_valid_hostname("localhost"));
        assert!(!is_valid_hostname(""));
        assert!(!is_valid_hostname("-leading.com"));
        assert!(!is_valid_hostname("trailing-.com"));
        assert!(!is_valid_hostname("a..b.com"));
        assert!(!is_valid_hostname("under_score.example.com"));
        assert!(!is_valid_hostname(&"a".repeat(64)));
    }

    #[test]
    fn host_port_ipv6() {
        assert!(parse_host_port("test", "[::1]:8080").is_ok());
        assert!(parse_host_port("test", "[fe80::1]:25565").is_ok());
        assert!(parse_host_port("test", "[::1]").is_err());
        assert!(parse_host_port("test", "[::1]8080").is_err());
    }

    #[test]
    fn host_port_port_ranges() {
        assert!(parse_host_port("test", "127.0.0.1:1").is_ok());
        assert!(parse_host_port("test", "127.0.0.1:65535").is_ok());
        assert!(parse_host_port("test", "127.0.0.1:0").is_err());
        assert!(parse_host_port("test", "127.0.0.1:65536").is_err());
        assert!(parse_host_port("test", "127.0.0.1:70000").is_err());
        assert!(parse_host_port("test", "127.0.0.1:abc").is_err());
        assert!(parse_host_port("test", "127.0.0.1").is_err());
        assert!(parse_host_port("test", "has space:25565").is_err());
    }

    #[test]
    fn control_chars_rejected_everywhere() {
        assert!(parse_host_port("test", "0.0.0.0:25\u{7}65").is_err());
        assert!(has_control_chars("a\u{0}b"));
        assert!(!has_control_chars("clean-key-value"));
    }
}
