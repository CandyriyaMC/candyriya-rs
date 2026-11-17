# Contributing to Candyriya

Thanks for your interest in contributing! This document covers the basics:
building the project, running the checks, and what we expect from pull
requests. For background and concepts, see the
[documentation](https://candyriyamc.github.io/candyriya-rs).

## Building from source

You need a stable Rust toolchain (edition 2024), installed via
[rustup](https://rustup.rs):

```shell
git clone https://github.com/CandyriyaMC/candyriya-rs.git
cd candyriya-rs
cargo build --release
```

The resulting binary is `target/release/candyriya`.

## Running the checks

Before opening a pull request, make sure the same checks that CI runs pass
locally:

```shell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

All code must pass these checks before it is considered complete.

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

## Rust coding standards

We follow idiomatic, modern, production-grade Rust, the official Rust API
Guidelines, and strict Clippy lints.

### General style
- Follow the official Rust API Guidelines and standard rustfmt formatting.
- Use descriptive, consistent naming.
- Prefer explicit, readable and maintainable code.
- Avoid unnecessary abstractions and excessive generic programming.
- Prefer composition over complicated type hierarchies.

### Memory management
- Minimize unnecessary allocations.
- Avoid unnecessary cloning.
- Prefer borrowing over ownership transfer when appropriate.
- Use `Arc` only when shared ownership is actually required.
- Avoid unnecessary heap allocations.

### Error handling
- Never use `unwrap()` in production code without a justified, documented reason.
- Never swallow errors; no ignored `Result`s.
- Use meaningful error types; prefer `thiserror` for library errors.
- Propagate errors explicitly.
- Never use panics for normal control flow.

### Async and concurrency
- Never block Tokio runtime worker threads.
- Avoid unnecessary mutexes and shared mutable state.
- Prefer message passing where it simplifies synchronization.
- Use bounded channels where backpressure is required.
- Keep CPU-intensive operations separate from asynchronous networking.
- Avoid spawning tasks without a clear lifecycle.

### Safety
- Minimize `unsafe` code; the workspace forbids it.
- Never bypass Rust's safety guarantees without a technical justification.

### Code quality
- No dead code.
- No duplicated logic.
- No artificial complexity.
- No placeholder implementations.
- No ignored errors.
- No unnecessary dependencies.
- All public APIs must have meaningful documentation.

## Branching model

- **`dev` is the integration branch.** All day-to-day work lands there.
- **`main` tracks the released state** and only moves when a batch of dev
  changes is merged deliberately — not per pull request.
- Branch off `dev`, open your PR **against `dev`**. Once a decent batch has
  accumulated, it is squash-merged into `main`, and tagged `v*` releases are
  cut from `main` (the release workflow builds all targets automatically).
- Direct pushes to both `main` and `dev` are blocked by a repository ruleset:
  everything goes through a pull request, and the CI checks must be green.
- If a change is docs/metadata-only and CI would waste a full build, put
  `[ci ignore]` in the commit message (push) or the PR title
  (pull_request). The CI jobs are then reported as skipped (which counts as
  green) and the Discord feed stays quiet. Never use it for code changes —
  reviewers may revert it. GitHub's native `[skip ci]` markers also work,
  but they leave required checks pending and will block the merge.
- Dependabot targets `dev` by default (it is the repository's default
  branch).

## Pull requests

- PRs target **`dev`**, not `main`. `dev` is squash-merged into `main` in
  batches; you never open a PR against `main` directly.
- **Squash-only.** This repository squash-merges pull requests. Your branch
  will land as a single commit, so keep the final commit message
  ([Conventional Commits](https://www.conventionalcommits.org/), subject ≤ 50
  characters) meaningful.
- One logical change per PR. Link related issues with `Closes #123`.
- Fill in the PR template checklist. CI must be green before review.
- By contributing, you agree that your contributions are licensed under the
  project's GPL-3.0 license (see [LICENSE](LICENSE)).

## Reporting bugs and requesting features

Use the [issue templates](https://github.com/CandyriyaMC/candyriya-rs/issues/new/choose).
For security issues, follow [SECURITY.md](SECURITY.md) instead — never file a
public issue.

## Code of Conduct

This project follows the [Contributor Covenant](.github/CODE_OF_CONDUCT.md).
Be kind and constructive.
