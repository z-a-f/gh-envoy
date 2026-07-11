# Envoy

Envoy is a GitHub-native coordination verifier for parallel AI-assisted development. It tracks declared worktree ownership for GitHub issues, observes overlap, and reports whether work is structurally sound. Envoy does not run agents or merge changes.

The project builds a single `gh-envoy` binary. Git remains the source of truth: Envoy invokes the Git CLI through a typed process boundary and does not use `libgit2`.

## Start here: dogfooding Envoy

Envoy is useful when several people or coding agents need to work on different GitHub issues in the same repository without silently sharing branches or worktrees. An issue claim records declared exclusive ownership of one branch and one worktree. It is a coordination contract, not a filesystem or process lock: Envoy detects unsafe states and reports them, while people and agents still perform the development work.

Build and install the extension from a local checkout, then confirm GitHub CLI can run it:

```sh
gh extension install .
gh envoy --help
```

The root `gh-envoy` script builds the release binary on demand with `cargo build --release --locked`, so reinstalling after code changes is not required.

Alternatively, put a built binary on `PATH` directly:

```sh
cargo build --release --locked
export PATH="$PWD/target/release:$PATH"
gh envoy --help
```

You can also invoke `target/release/gh-envoy` directly. On Windows, Git for Windows provides the `sh.exe` interpreter needed to run the root script; you can also add `target\release` to `PATH` using your normal shell or system settings.

The basic dogfooding loop is:

1. From any worktree in the target repository, claim an issue with `gh envoy claim 123`.
2. Change into the worktree printed by the command and start the human or agent doing issue 123 there.
3. Repeat for other independent issues. Each claim receives its own branch and worktree.
4. Use `gh envoy status` while work is active to find ownership, overlap, scope, and integrity concerns.
5. Before publishing or integrating work, run `gh envoy doctor 123`, or `gh envoy doctor --stack 123` for stacked work.
6. Push, open the pull request, review, and merge with your existing tools. Envoy v0.1 does not write to GitHub.
7. Mark the local claim complete with `gh envoy release 123 --reason merged`. Release preserves the branch and worktree.

Choose the claim form that matches the work:

| Scenario | Command | When to use it |
| --- | --- | --- |
| New independent work | `gh envoy claim 123` | Create an isolated branch and worktree from the selected base. |
| Existing branch | `gh envoy claim 123 --branch issue-123` | Adopt a local branch without resetting it. |
| Existing worktree | `gh envoy claim 123 --worktree ../issue-123` | Adopt an already registered Git worktree. |
| Stacked change | `gh envoy claim 124 --onto 123` | Record that issue 124 is based on the exact active generation of issue 123. |
| Consolidation work | `gh envoy claim 130 --after 123 --after 124` | Record that issue 130 should wait for several issue generations. |
| Bounded ownership | `gh envoy claim 131 --scope 'src/**' --disallow '.github/workflows/**'` | Declare expected and prohibited paths so status and doctor can flag drift. |
| Automation | `gh envoy status --json` | Consume stable machine-readable output and exit codes. |

Current commands are deliberately local and read-only with respect to GitHub. Issue-title and pull-request observation, guarded push/PR creation, stack shipping, and optional release cleanup are **future options and are not implemented yet**. Agent launching, automatic rebasing/restacking, merging, retargeting, and force-pushing are outside the current command set; keep those steps explicit and human-controlled.

See the [dogfooding guide](docs/dogfooding.md) for complete workflows, stack and dependency behavior, safety gates, configuration, JSON use, and troubleshooting. The [docs index](docs/README.md) separates operator guidance from the normative [product specification](spec.md).

## Development

Install the stable Rust toolchain, then run:

```sh
gh extension install .
cargo check --locked
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --locked
```

Tests use temporary repositories and fake GitHub command runners. They do not require GitHub credentials or network access after Cargo dependencies are available.

## CLI

Install locally as a gh extension, or build the entrypoint directly:

```sh
gh extension install .
gh envoy --help
```

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

Inspect all active claims and their local coordination findings from any registered worktree:

```sh
gh envoy status
gh envoy status --json
```

Status derives diffs, overlap relationships, scope findings, and local integrity hints without changing repository or Envoy state. GitHub and PR fields remain explicitly unverified until read-only GitHub observation lands.

Run local integrity checks for the repository or one active issue:

```sh
gh envoy doctor
gh envoy doctor 123
gh envoy doctor --stack 123
gh envoy doctor 123 --json
```

Doctor verifies persisted claim schemas, canonical worktree ownership, branches, captured base SHAs, diff derivation, dependency graphs, overlap, scope, and interrupted operation journals without mutating Git or Envoy state. It reports separate integrity, publish, and merge gates, uses exit codes `0` for ok, `1` for warning, `2` for blocked, and `3` for an operational error, and emits conservative recovery commands for abandoned operations. Path evidence is shortened in JSON by default according to `redact_paths_in_json`; human recovery instructions retain full paths.

Stack doctor follows exact `base_claim_id` generations and renders them from root to target. It never substitutes a reclaimed issue generation. An advanced parent whose captured SHA remains in history leaves publish `ok` and warns merge; rewritten, missing, or released exact parents block publish. `base_claim_id` and `wait_for` cycles are publish errors. Consolidation diffs receive a neutral annotation because multi-parent diff-base computation remains deferred, while their exact-generation overlaps retain the normal risk severity.

Envoy never restacks automatically. After reviewing the effective base, use the reported recipe manually and rerun doctor:

```sh
git -C <child-worktree> rebase --onto <effective-base> <captured-parent-sha>
gh envoy doctor --stack <child-issue>
```

## Architecture

- CLI entry points only parse, dispatch, render, and map stable exit codes.
- Coordination logic lives in the library independently of rendering.
- Local observation derives active claim diffs, exact-generation overlap and scope findings, and integrity problems without mutating stored state.
- Git and GitHub command adapters are typed and mockable.
- Envoy-owned state lives under the repository's shared Git common directory, never in a worktree.
- Store mutations use an OS advisory lock and same-directory atomic replacement.

The local `spec.md` is normative for product behavior, and `plan.md` defines the delivery slices.
