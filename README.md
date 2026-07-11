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
2. In an interactive terminal, Envoy opens a nested shell in the new worktree. Start the human or agent doing issue 123 there.
3. Repeat for other independent issues. Each claim receives its own branch and worktree.
4. Use `gh envoy list` for claim history and `gh envoy status` for active ownership, overlap, scope, and integrity concerns.
5. Before publishing or integrating work, run `gh envoy doctor 123`, or `gh envoy doctor --stack 123` for stacked work.
6. Push, open the pull request, review, and merge with your existing tools. Envoy v0.1 does not write to GitHub.
7. Mark the local claim complete with `gh envoy release 123 --reason merged`. Release preserves the branch and worktree.

`gh envoy` cannot change the directory of its parent shell. Instead, an interactive human claim opens a nested shell in the exact claimed worktree; exit that shell to return to the original directory. Use `--no-cd` to create the claim and return immediately, or `--cd` to request the worktree shell explicitly. JSON claims never open a shell.

Choose the claim form that matches the work:

| Scenario | Command | When to use it |
| --- | --- | --- |
| New independent work | `gh envoy claim 123` | Create an isolated branch and worktree from the selected base. |
| Existing branch | `gh envoy claim 123 --branch issue-123` | Adopt a local branch without resetting it. |
| Existing worktree | `gh envoy claim 123 --worktree ../issue-123` | Adopt an already registered Git worktree. |
| Stacked change | `gh envoy claim 124 --onto 123` | Record that issue 124 is based on the exact active generation of issue 123. |
| Consolidation work | `gh envoy claim 130 --after 123 --after 124` | Record that issue 130 should wait for several issue generations. |
| Bounded ownership | `gh envoy claim 131 --scope 'src/**' --disallow '.github/workflows/**'` | Declare expected and prohibited paths so status and doctor can flag drift. |
| Claim inventory | `gh envoy list` | Show every active and released claim generation. |
| Automation | `gh envoy status --strict --json` | Consume machine-readable output and fail on coordination warnings. |

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

## Releasing

Push a version tag to build and publish precompiled binaries via `.github/workflows/release.yml`:

```sh
git tag v0.1.0
git push origin v0.1.0
```

Use plain, hyphen-free semver tags (`v0.1.0`, not `v0.1.0-alpha.1`). `gh-extension-precompile` marks any tag containing a hyphen as a prerelease, and `gh extension install <owner>/gh-envoy` only detects binary assets from the repository's **latest non-prerelease** release. If only prereleases exist, `gh` falls back to cloning the repo and running the root `gh-envoy` script, which requires Rust on the user's machine. If you must publish a prerelease and still want it installable, mark it as the latest release explicitly:

```sh
gh release edit <tag> --prerelease=false
gh api -X PATCH repos/<owner>/gh-envoy/releases/<id> -f make_latest=true
```

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
gh envoy claim 124 --no-cd
gh envoy claim 125 --cd
gh envoy claim 123 --resume
```

Interactive claims enter a nested worktree shell by default. `--no-cd` is intended for callers that will start work separately; `--cd` makes the shell handoff explicit. The shell is selected from `SHELL`, then `COMSPEC`, with a platform fallback.

If an issue already has an active local claim, use `--resume` to enter that exact generation instead of receiving an already-claimed error:

```sh
gh envoy claim 123 --resume
```

Resume is read-only: it does not create a generation, refresh a base, or change persisted scope. It verifies that the active branch is still registered at the claimed worktree, then opens the nested shell there. Creation options, `--no-cd`, and `--json` cannot be combined with `--resume`.

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

List the complete local claim history, including released generations:

```sh
gh envoy list
gh envoy list --json
```

Human output uses compact per-claim summaries. Interactive terminals receive color automatically; redirected output and JSON remain free of ANSI escapes. Set `NO_COLOR` to disable color explicitly.

Inspect all active claims and their local coordination findings from any registered worktree:

```sh
gh envoy status
gh envoy status --json
```

Status renders one readable block per active claim rather than a terminal-wide table. It always displays declared allowed/disallowed scope, derives diffs, overlap relationships, scope findings, and local integrity hints without changing repository or Envoy state. Interactive terminals use color for healthy, warning, and problem markers; redirected output stays plain. GitHub and PR fields remain explicitly unverified until read-only GitHub observation lands.

Overlap is evidence from current diffs, not a prediction from intersecting scope globs. Two claims that both declare `README.md` show `none (diff-based)` until both actually change that path; then the overlap is reported. Leading `./` and Windows `\` separators in new scope declarations are normalized to Git's repository-relative `/` paths.

Status is informational and exits `0` after rendering, even when the report contains warnings. Use `gh envoy status --strict` when a warning should produce exit code `1`, such as in CI. For a stale claim whose branch and worktree are both gone, run doctor for the safe marker-only release recommendation.

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
