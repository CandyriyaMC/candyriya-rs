# Security Policy

## Supported Versions

Candyriya is developed on the `main` branch and is pre-1.0. Only the latest
release (and `main`) receives security fixes.

| Version | Supported |
| ------- | --------- |
| `main`  | yes       |
| latest release tag | yes |
| older releases     | no  |

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public issues,
discussions, or pull requests.**

Instead, report them privately via GitHub's private vulnerability reporting:

1. Go to the [Security Advisories](https://github.com/CandyriyaMC/candyriya-rs/security/advisories) page.
2. Click **Report a vulnerability** (or open [this link](https://github.com/CandyriyaMC/candyriya-rs/security/advisories/new) directly).
3. Fill in as much detail as you can: affected version/commit, steps to
   reproduce, impact, and any suggested fix.

## What to expect

- We aim to acknowledge your report within a few days.
- We will validate the report and keep you informed of progress toward a fix.
- Once a fix is released, we will publish a security advisory and credit you
  in it (unless you prefer to remain anonymous).
- Please give us reasonable time to ship a fix before disclosing the issue
  publicly.

## Scope

This policy covers the Candyriya codebase: the proxy, its crates, and the
release pipeline. Vulnerabilities in third-party dependencies should be
reported upstream, but please let us know as well so we can bump the
dependency or ship a mitigation.
