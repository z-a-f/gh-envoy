# Dogfooding Envoy

This guide is for teams using Envoy on Envoy itself or introducing it into another Git repository. It covers the implemented v0.1 local coordination workflow first, then labels future options separately.

## What Envoy coordinates

Envoy treats a GitHub issue as the unit of work. A claim binds one issue generation to a branch, a registered Git worktree, and an exact base commit. Other claims may also record:

- a stack parent with `--onto`;
- consolidation dependencies with one or more `--after` options;
- expected paths with `--scope`;
- prohibited paths with `--disallow`;
- human context with `--note`.

This is **declared exclusive ownership**, not an operating-system lock. Envoy does not prevent a person or process from editing a claimed worktree. It makes ownership and structural problems visible through `status` and `doctor` so operators can intervene before publishing or merging.

Envoy does not launch agents. Start each coding agent yourself in the worktree returned by `claim`, and give it the corresponding issue as its scope.

## Install for a dogfood run

Prerequisites are Git, GitHub CLI, and the repository's stable Rust toolchain. Building and all local coordination commands work without GitHub credentials after dependencies are available.

```sh
cargo build --release --locked
export PATH="$PWD/target/release:$PATH"
gh envoy --help
```

GitHub CLI discovers an executable named `gh-envoy` on `PATH` and exposes it as `gh envoy`. Direct invocation is also supported:

```sh
./target/release/gh-envoy status
```

On Windows, use `target\release\gh-envoy.exe` directly or add `target\release` to `PATH`.

Run Envoy from a worktree belonging to the repository you want to coordinate. Envoy stores shared state under that repository's Git common directory, so all of its registered worktrees see the same claims.

## Scenario 1: one independent issue

Create a fresh claim from the repository's selected base:

```sh
gh envoy claim 123
```

The command prints the claim, branch, worktree, captured base, and any warning about base verification. Change into the reported worktree before modifying files or starting an agent.

Envoy also prints an explicit directory-change prompt after the exact `Worktree` path. The `gh-envoy` extension is a child process and cannot persistently change its parent shell's directory, so a CLI-only `--switch` flag cannot provide that behavior. A future optional shell integration may wrap claim and `cd` as one shell operation.

During development:

```sh
gh envoy list
gh envoy status
gh envoy doctor 123
```

`list` is the full local history of active and released claim generations. `status` is the active repository coordination view. `doctor 123` is the focused pre-publish and pre-merge check. None of these commands mutate Git or Envoy state.

Human `list` and `status` output uses compact per-claim blocks and adds color when stdout is an interactive terminal. Piped output, redirected output, and JSON never contain color escapes. Set the standard `NO_COLOR` environment variable to disable color.

Status exits `0` when it renders successfully, even if the human or JSON report says `warning`. Use `gh envoy status --strict` when warnings should return exit code `1`.

Publishing and pull-request creation are manual in v0.1. After the work has reached its terminal state, release the claim with the reason that actually occurred:

```sh
gh envoy release 123 --reason merged
# Other reasons: closed, abandoned, manual
```

Release is idempotent and marker-only. It preserves the claim record, branch, and worktree. A later claim for the same issue creates a new claim generation rather than rewriting the old one.

## Scenario 2: several independent issues in parallel

Claim each issue before starting its worker:

```sh
gh envoy claim 201
gh envoy claim 202
gh envoy claim 203
```

Run each person or agent only in the worktree printed for its issue. Check the whole repository periodically:

```sh
gh envoy status
gh envoy doctor
```

Ordinary overlap between unrelated claims is a warning. Overlap on configured risk paths can block safe integration. Default risk categories include lockfiles, migrations, project configuration, GitHub workflows, and tests. Treat a finding as a coordination prompt: decide ownership, narrow one change, or sequence the integrations, then rerun `doctor`.

Useful operating discipline:

1. Claim before editing.
2. Put the issue number in the agent task and keep the agent in the claim's worktree.
3. Do not reuse one worktree for two active claims.
4. Run `status` when a new claim starts and before one is handed off.
5. Run `doctor` immediately before any manual push or merge decision.

## Scenario 3: adopt work already in progress

Adopt an existing local branch:

```sh
gh envoy claim 301 --branch issue-301
```

Or adopt an existing registered worktree:

```sh
gh envoy claim 302 --worktree ../issue-302
```

Envoy does not reset the branch or move the worktree. Adoption is refused when ownership would be ambiguous or when the branch does not contain the captured base. If adoption fails, inspect the named branch/worktree and the active claims rather than moving files into a different claim.

Use this scenario when dogfooding begins after development has already started. Prefer fresh claims for new work because their base and worktree lifecycle are easier to reason about.

## Scenario 4: stacked issue work

First claim the parent, then claim the child onto the active parent generation:

```sh
gh envoy claim 401
gh envoy claim 402 --onto 401
gh envoy doctor --stack 402
```

`--onto` captures both the parent issue and its exact `claim_id`. Stack doctor walks exact claim generations from the root to the requested target; it never silently substitutes a newer claim for the same issue.

There are three important parent states:

- Parent unchanged: normal stack checks apply.
- Parent advanced but the captured SHA is still an ancestor: publish remains allowed, while merge receives a warning until the child is intentionally updated.
- Parent rewritten, missing, or released: publish is blocked because the recorded base can no longer be trusted.

Envoy never performs a rebase or restack automatically. Review the effective base, execute the recovery recipe printed by doctor, and rerun the stack check. A typical manual form is:

```sh
git -C <child-worktree> rebase --onto <effective-base> <captured-parent-sha>
gh envoy doctor --stack 402
```

Do not copy that template blindly; use the concrete SHAs and worktree reported for the claim.

## Scenario 5: consolidation and dependency intent

Use `--after` when a claim coordinates or consolidates several issue results without being a linear child of one parent:

```sh
gh envoy claim 503 --after 501 --after 502 \
  --note 'Integrate the parser and renderer changes'
```

When a dependency has an active local claim, Envoy records its exact generation. When it does not, Envoy records only the issue number and does not infer a relationship later. This prevents a new generation from being mistaken for the one the claim originally intended to wait for.

Multi-parent diff-base computation and automatic dependency holds are future options. Current doctor output annotates consolidation diffs conservatively and still reports exact-generation overlaps and integrity problems.

## Scenario 6: declare file ownership boundaries

Scopes document what a claim expects to change and give status/doctor more evidence:

```sh
gh envoy claim 601 \
  --scope 'src/doctor/**' \
  --scope 'tests/doctor_*.rs' \
  --disallow '.github/workflows/**' \
  --note 'Local integrity reporting only'
```

`--scope` and `--disallow` accept repeatable glob patterns. They are declarations, not filesystem enforcement. A worker can still edit another path, but Envoy reports the mismatch. Keep scopes meaningful rather than enumerating every expected file; the goal is useful coordination evidence.

## Scenario 7: work with no usable remote

Envoy attempts to refresh the configured base remote for a fresh claim. If the remote cannot be reached, it may fall back to an existing remote-tracking reference or local base branch and reports that the base is unverified.

This makes local and offline dogfooding possible, but it changes what is known:

- Local branch, worktree, diff, ownership, and dependency checks still run.
- GitHub issue existence, title, and pull-request state are not currently observed.
- An unverified fallback should be reviewed before publishing, especially after a long offline period.

Do not interpret an issue number accepted by `claim` as proof that the GitHub issue exists. Read-only GitHub observation is a future option.

## Scenario 8: recover after interruption or drift

Claim creation is journaled. If Git or Envoy is interrupted, run:

```sh
gh envoy doctor
```

Doctor checks operation journals, persisted schemas, worktree ownership, branches, captured bases, dependency cycles, diffs, overlap, and scope. It reports conservative recovery instructions without executing them.

Common responses are:

- Missing or moved worktree: restore/register the intended worktree, or release and create a new generation if the old work is abandoned.
- Duplicate ownership: stop both workers and resolve which claim owns the branch/worktree before continuing.
- Rewritten stack parent: manually restack the child using the reported base evidence.
- Dependency cycle: release and recreate the incorrect claim relationship; doctor will not guess which edge to remove.
- Overlap on a risk path: coordinate integration order or move the shared change into one owning claim.

Always rerun the same doctor command after recovery.

When both a claim's branch and worktree are missing, doctor treats it as possibly stale and prints the non-destructive cleanup command:

```sh
gh envoy release <issue> --reason abandoned
```

Doctor never runs this command itself. Release writes a marker and preserves historical claim data; it does not delete branches or worktrees.

## Understand gates and exit codes

Doctor reports separate gates because a claim can be locally sound enough to publish while still unsafe to merge:

- **Integrity**: persisted state and local Git structure are coherent.
- **Publish**: the branch can be safely handed to the publishing workflow.
- **Merge**: overlap, stack position, and coordination evidence are safe enough for integration.

Commands use stable process exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Success, or doctor reports ok. |
| `1` | Warning that needs review; emitted by doctor, warning-producing claims, or `status --strict`. |
| `2` | Blocked or refused. |
| `3` | Operational error, such as invalid state or command failure. |
| `4` | Held; reserved for a future ship workflow and not currently emitted by implemented commands. |

For scripts, use `--json` and still check the exit code:

```sh
gh envoy status --strict --json
gh envoy doctor 123 --json
```

JSON paths are shortened by default to avoid exposing machine-specific directory prefixes. Human recovery instructions retain full paths. Set `redact_paths_in_json: false` only when the consumer needs absolute paths and the output will remain appropriately protected.

## Repository configuration

Envoy reads optional configuration from `<git-common-dir>/envoy/config.yml`. Defaults apply when the file is absent. For example:

```yaml
base_remote: origin
default_base_ref: main
worktree_root: /absolute/path/to/envoy-worktrees
redact_paths_in_json: true
risk_paths:
  "infra/**": infrastructure
  "schema/**": schema
```

`worktree_root` must be absolute. Custom `risk_paths` extend the built-in risk categories. Keep this local common-directory configuration consistent for everyone sharing the repository; it is not read from whichever worktree happens to invoke Envoy.

## Current boundaries and future options

The following distinction matters during dogfooding:

| Capability | Status |
| --- | --- |
| Claim creation and adoption | Available now. |
| Full active and released claim history with `list` | Available now. |
| Local status, overlap, scope, and integrity observation | Available now. |
| Single-claim and exact-generation stack doctor | Available now. |
| Marker-only, idempotent release | Available now. |
| GitHub issue/title and PR-state observation | **Future option; not implemented.** It must remain read-only when introduced. |
| `ship` for guarded push and PR creation | **Future option; not implemented.** Until then, publish manually after doctor. |
| Whole-stack shipping, holds, dry runs, and PR readiness controls | **Future option; not implemented.** |
| Release-time branch/worktree deletion flags | **Future option; not implemented.** Current release never deletes them. |
| Automatic consolidation diff bases | **Future option; not implemented.** |
| Agent execution or scheduling | Not an Envoy responsibility. Start and supervise agents externally. |
| Automatic merge, retarget, rebase/restack, or force-push | Not implemented and intentionally outside the automatic workflow. |

Future syntax shown in the specification or plan is design material, not a command to run against the current binary. Use `gh envoy --help` as the source of truth for the installed command surface.

## Dogfood checklist

Before starting:

- Build the current branch with `cargo build --release --locked`.
- Run `gh envoy doctor` and resolve pre-existing integrity failures.
- Decide which issues are independent, stacked, or consolidation work.
- Ensure every worker knows its issue number and assigned worktree.

Before publishing:

- Confirm the worker is on the claim's branch and worktree.
- Run `gh envoy status` and review overlap/scope findings.
- Run `gh envoy doctor <issue>` or `gh envoy doctor --stack <issue>`.
- Treat unverified bases and risk-path overlap as explicit review items.
- Push and create the PR manually; v0.1 performs no remote writes.

After the issue reaches a terminal state:

- Release the claim with the correct reason.
- Leave branch/worktree cleanup to the operator.
- Run repository-wide doctor again before assigning the next wave of work.
