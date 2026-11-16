# Contributing to candyriya-rs

Thanks for your interest in contributing! This document covers the basics:
building the project, running the checks, and what we expect from pull
requests. For background and concepts, see the
[documentation](https://bejiihiu.github.io/candyriya-rs).

## Building from source

You need a stable Rust toolchain (edition 2024), installed via
[rustup](https://rustup.rs):

```shell
git clone https://github.com/bejiihiu/candyriya-rs.git
cd candyriya-rs
cargo build --release
```

The resulting binary is `target/release/candyriya`.

## Running the checks

Before opening a pull request, make sure the same checks that CI runs pass
locally:

```shell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --no-deps --workspace
```

## Style guide

- **No `unsafe`.** The workspace forbids `unsafe` code. If you think you need
  it, open an issue first — the answer is almost certainly no.
- **English comments.** Comments and documentation are written in English.
  Keep them informal and honest; document *why*, not *what*.
- **User-facing text** (error messages, CLI output, docs) is written in clear,
  grammatical English.
- Prefer existing workspace crates and public traits over new modules; keep
  dependencies between crates acyclic.
- Format with `cargo fmt` and keep `cargo clippy` clean.

## Pull requests

- **Squash-only.** This repository squash-merges pull requests. Your branch
  will land as a single commit, so keep the final commit message
  ([Conventional Commits](https://www.conventionalcommits.org/), subject ≤ 50
  characters) meaningful.
- One logical change per PR. Link related issues with `Closes #123`.
- Fill in the PR template checklist. CI must be green before review.
- By contributing, you agree that your contributions are licensed under the
  project's GPL-3.0 license (see [LICENSE](LICENSE)).

## Reporting bugs and requesting features

Use the [issue templates](https://github.com/bejiihiu/candyriya-rs/issues/new/choose).
For security issues, follow [SECURITY.md](SECURITY.md) instead — never file a
public issue.

## Code of Conduct

This project follows the [Contributor Covenant](.github/CODE_OF_CONDUCT.md).
Be kind and constructive.
