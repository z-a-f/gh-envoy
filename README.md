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

Fresh, unstacked claims provision an isolated branch and worktree from an exact captured base SHA:

```sh
gh envoy claim 123
```

Existing local branches and registered worktrees can be adopted without resetting or moving them:

```sh
gh envoy claim 123 --branch my-existing-branch
gh envoy claim 124 --worktree ../existing-worktree
```

Stack and consolidation intent records exact local claim generations when they are available. Optional scopes and notes are persisted with the claim for later coordination checks:

```sh
gh envoy claim 125 --onto 123
gh envoy claim 126 --after 123 --after 124 \
  --scope 'src/**' --disallow '.github/workflows/**' \
  --note 'Coordinate this integration manually'
```

Adopted branches must contain the captured base, `--onto` requires an active local parent claim, and direct or duplicate dependencies are refused. Issue existence remains unverified until GitHub observation is implemented.

Envoy first attempts to refresh the configured remote base. When the remote is unavailable, it can use an existing remote-tracking ref or local base branch and reports the unverified fallback explicitly. Claim state is journaled under the shared Git common directory so interrupted operations remain inspectable.

Marker-only release is idempotent and preserves the generation's claim file, branch, and worktree:

```sh
gh envoy release 123
gh envoy release 123 --reason merged
```

`status` and `doctor` remain explicit `not_implemented` placeholders until their owning slices land.

## Architecture

- CLI entry points only parse, dispatch, render, and map stable exit codes.
- Coordination logic lives in the library independently of rendering.
- Local observation derives active claim diffs and integrity problems without mutating stored state.
- Git and GitHub command adapters are typed and mockable.
- Envoy-owned state lives under the repository's shared Git common directory, never in a worktree.
- Store mutations use an OS advisory lock and same-directory atomic replacement.

The local `spec.md` is normative for product behavior, and `plan.md` defines the delivery slices.
