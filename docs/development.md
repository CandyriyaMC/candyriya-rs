# Development

This document collects notes for people hacking on candyriya-rs. For building
and submitting changes, see [CONTRIBUTING.md](../CONTRIBUTING.md).

## Audit pipeline

CI is the security gate for the dependency tree. On every push and pull
request, the `security` job runs [cargo-deny](https://embarkstudios.github.io/cargo-deny/)
against the checked-in `Cargo.lock`: `advisories` fails on known RustSec
vulnerabilities, unmaintained or unsound crates, `licenses` enforces the
allowlist (GPL-3.0-only for this workspace itself, permissive licenses
otherwise), `bans` forbids multiple versions of the same crate (the skip list
in `deny.toml` documents the duplicates that are blocked on upstream), and
`sources` rejects crates from anywhere but crates.io and this repository. The
`audit-check` job (RustSec/audit-check) audits the same lockfile against the
RustSec advisory database and annotates the run. Goal: no known-vulnerable or
license-violating code reaches `main`, and dependency hygiene issues surface
in review, not in production. The CI jobs are the gate — running cargo-deny
locally is a convenience, not a requirement.
