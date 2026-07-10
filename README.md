# Envoy

Envoy is a GitHub-native coordination verifier for parallel AI-assisted development. It tracks declared worktree ownership for GitHub issues, observes overlap, and reports whether work is structurally sound. Envoy does not run agents or merge changes.

The project builds a single `gh-envoy` binary. Git remains the source of truth: Envoy invokes the Git CLI through a typed process boundary and does not use `libgit2`.

## Development

Install the stable Rust toolchain, then run:

```sh
cargo build --locked
cargo check --locked
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --locked
```

Tests use temporary repositories and fake GitHub command runners. They do not require GitHub credentials or network access after Cargo dependencies are available.

## CLI

Build the extension entrypoint and inspect its commands:

```sh
cargo build --release --locked
./target/release/gh-envoy --help
```

When `gh-envoy` is installed on `PATH`, GitHub CLI exposes it as:

```sh
gh envoy --help
```

Slice 1 establishes command dispatch for `claim`, `status`, `doctor`, and `release`. These commands intentionally report `not_implemented` until their owning vertical slices land.

## Architecture

- CLI entry points only parse, dispatch, render, and map stable exit codes.
- Coordination logic lives in the library independently of rendering.
- Git and GitHub command adapters are typed and mockable.
- Envoy-owned state lives under the repository's shared Git common directory, never in a worktree.
- Store mutations use an OS advisory lock and same-directory atomic replacement.

The local `spec.md` is normative for product behavior, and `plan.md` defines the delivery slices.
