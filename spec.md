# Envoy: Product Specification

## North Star

Envoy exists to convert one engineer into the throughput of many, by making parallel AI-assisted workstreams cheap to hold in one head. The scarce resource is not compute or agent capability — it is **the operator's attention**. Every design decision is judged against three friction principles:

1. **Every additional call is friction.** Commands must compose (`claim --run`, not claim-then-cd-then-launch). One command should carry an intent from end to end.
2. **Every directory switch is cognitive pressure.** Every command is addressable by issue number from anywhere in the repository. No workflow may require the operator to know, find, or navigate to a worktree path; worktrees are an implementation detail the operator can always ignore and occasionally enter.
3. **Every request is a potentially blocking queue.** The operator must never wait on an agent, and an agent must never silently wait on the operator. Long work runs in the background; anything that needs the operator's input is surfaced where the operator already is (terminal summary, phone relay) rather than trapped in a hidden terminal.

The target loop: describe a feature → issues appear on GitHub → claim-and-launch agents in parallel → walk away → answer questions from a phone → return to a single pane of glass showing what ran, what overlaps, what's reviewable → ship with one command → review a reviewer-authored PR body → merge → claims release themselves.

### What Envoy is, and deliberately is not

Envoy is the **coordination and execution substrate** for that loop: it claims work, provisions isolated workspaces, launches and supervises agents in them, observes what actually changed, verifies coherence, and performs the push/PR ceremony. It is one component of a pipeline whose other stages are deliberately out of the binary:

| Pipeline stage | Vision items | Owner |
| --- | --- | --- |
| Feature → issues (JSON/Scrum) → GitHub | 1–3 | **Companion planning skill** (Claude Code skill + `gh issue create`; prompt engineering, not product code) |
| Claim, spawn, parallel background agents, durable reports | 4–6, 8 | **Envoy** (claim + run layer) |
| Operator prompts answered from phone | 7 | **openfind relay**, subscribing to Envoy run events via a hook (Envoy never builds a second relay) |
| PR with reviewer-authored body, landing tags | 9 | **Envoy ship** (mechanical scaffold) + **companion reviewer agent** (judgment) |
| Release/archive claim on merge | 10 | **Envoy sync** |
| Mutual awareness among parallel agents | 11 | **Envoy** (coordination prompt + `status --json` + doctor) |

Envoy never interprets agent output to infer work status, never commits on an agent's behalf, never merges, and never decides what should be built. Humans (and their designated reviewer agents) own judgment; Envoy owns state, isolation, observation, and ceremony.

### The honest constraint

At scale, throughput is bounded by **review bandwidth**, not agent count. The system's answer is layered, not evasive: a decorrelated AI reviewer (different context, adversarial prompt, ideally different model — never the author summarizing itself) authors the PR body; the human reads every body to stay calibrated (the anti-"automated-yes-clicker" defense); **risk-weighted sampling** sends a fraction of PRs to full human code review, selected by doctor's signals (risk paths, overlap severity, diff size) plus one-in-N of the rest; and the repo's no-squash atomic-commit policy keeps every landed change cheap to revert, cherry-pick, or amend when post-merge signal finds a problem. Debt still accrues faster than in a hand-written codebase; the goal is that it accrues *legibly*.

## Product Principle

> Envoy claims work.
> Envoy launches and supervises agents in claimed worktrees.
> Envoy observes work.
> Envoy doctors work.
> Envoy ships work when doctor's publish checks pass.
> Envoy releases claims when GitHub says the work landed.
> Humans (and their reviewer agents) review; humans merge.

## Versioned Scope

**v0.1 — shipped.** Claim/list/status/doctor/release, fully local-first with read-only GitHub observation. See §2 for the as-built surface.

**v0.2 — run layer (next).** Managed agent execution: `run` command family, background workers, durable run records, `claim --run`, coordination-aware prompt composition, run-event hook.

**v0.3 — remote writes.** `ship` (guarded push + PR creation) and `sync` (release-on-merge), plus release cleanup flags.

**Companion layer (out of binary, parallel):** planning skill, reviewer skill, openfind subscription.

## Unit of Work

The GitHub issue is the unit of work. Every command keys on an issue number; a claim asserts that a specific existing issue is being worked on in a specific worktree. Envoy reads issues but never creates or edits them. Consequences, all intentional: work without an issue is invisible (file the issue first — the planning skill makes this cheap); parallelism inside one logical task is expressed as sub-issues, each claimed separately (`--onto` for stacks, `--after` for consolidation — explicit local intent, never derived from GitHub metadata); cross-repo work is N issues in N repos, claimed independently, with correlation deferred.

---

# 1. Core Objects

## 1.1 Claim (implemented)

A claim says: issue `#123` is actively being worked on in this branch/worktree, under this exact **claim generation** (`claim_id`).

* **Write-once**, keyed by claim_id: `claims/<issue>/<claim_id>.json`, releases mirrored. Reclaim = new generation; history is never rewritten. Active = claims minus release markers.
* **Declared exclusive ownership** — one active claim per issue and per worktree, enforced at claim time under the store lock. This is a registry invariant that prevents Envoy-mediated double-assignment; it is not an OS lock, and the docs never call it one. Doctor verifies the convention.
* **Atomic**: store mutations serialize under the store lock; writes are temp-file-plus-rename; an operation journal records multi-step mutations so interrupted claims roll back or surface as doctor-repairable states; concurrent claims of one issue resolve to exactly one winner.
* Worktree paths are canonical-absolute (from `git worktree list --porcelain`); redaction is an output concern.
* Fields: `claim_id`, `repo`, `issue`, `title?`, `branch` (UUID-stable `envoy/issue-<n>-<id>`; issue number is display metadata — nothing renames, so GitHub never closes a PR under a branch), `worktree`, `base_ref`, `base_sha`, `base_issue?`, `wait_for_issues?`, `declared_scope?`, `note?`, `created_at`.

## 1.2 Run (v0.2)

A run says: this agent process was launched for this exact claim generation, and here is its lifecycle and its artifacts.

* Mutable operational state (the one deliberate exception to write-once), stored under `$(git-common-dir)/envoy/runs/<run-id>/` — `run.json`, `prompt.md`, `stdout.log`, `stderr.log`, `stop-request`. Runs live in the common dir precisely so **reports survive worktree destruction** and are findable from anywhere (friction principle 2, vision item 8).
* `run.json` transitions atomically under the store lock; the lock is never held while waiting on a process.
* Schema: `run_id`, `repo`, `claim_id` (exact generation, never just an issue), `issue`, `agent` (executable name), `mode: interactive|background`, `state: queued|running|succeeded|failed|stop_requested|stopped` (plus `queued→failed` for spawn failure), `worker_pid`, `child_pid`, timestamps, `exit_code`, artifact paths, `error`.
* Prompt text, agent arguments, and log contents never appear in serialized status output — they may contain secrets. Artifacts are owner-only on Unix.
* Envoy records process lifecycle only. A succeeded run means the process exited zero, not that the issue is fixed; docs state this distinction prominently.

---

# 2. Command Surface

## 2.1 Implemented (v0.1, as built)

Global: `--json` on every command. Exit codes: `0` success, `1` warning, `2` blocked/refused, `3` operational error, `4` held.

**`gh envoy claim <issue>`** — provision and own a workspace, addressable from anywhere in the repo.
Options as implemented: `--branch` (adopt existing), `--worktree` (adopt existing), `--onto <issue>` (stack intent; base must be actively claimed; captures base branch tip as base_sha), `--after <issue>` (repeatable; consolidation intent → `wait_for_issues`), `--scope`/`--disallow <glob>`, `--note`, `--force` (permit claiming a closed issue — claiming closed issues is otherwise refused), and the worktree-entry trio: interactive claims open a **nested shell** in the claimed worktree by default (`--cd` explicit, `--no-cd` to return immediately, JSON never opens a shell). `--resume` re-enters the active claim's worktree shell and conflicts with all creation flags. The nested shell is the interactive answer to directory-switch friction; `--run` (v0.2) is the background answer, and the two are exclusive.

**`gh envoy list`** — claim-generation inventory: every active and released generation, in creation order. History view; `status` is the live view.

**`gh envoy status [--strict]`** — the single pane of glass for active work: issue, title, branch, PR facts when discoverable, diff summary, overlap with relationship, scope drift, integrity concerns. `--strict` exits 1 on coordination warnings, for automation. v0.2 adds a live-run column (agent, state, runtime) so "what is running where" never requires a terminal census.

**`gh envoy doctor [<issue> | --stack <issue>]`** — the verifier. Every check carries a **gate**: `integrity` (claim/worktree/branch/base_sha sound), `publish` (safe to push + open a PR: integrity + base not rewritten + any existing PR targets the correct base), `merge` (publish + overlap, scope, staleness). Single rollup plus per-gate rollups; ship gates on publish only; humans gate on merge. Relationship-aware overlap severities: sibling ordinary = warning, sibling risk-path = blocking; ancestor↔descendant ordinary = info (layered development), risk-path = inspect-warn; consolidation ↔ its `wait_for_issues` targets = expected/info. Evidence tiers: tracked/staged = full confidence; shared untracked paths = lower-confidence warning; ignored = excluded. Stack drift: base advanced = merge warning; base rewritten = publish block; base merged/released = restack-to-main recommendation with the manual recipe. v0.2 adds run-awareness: a live run on a claim is reported; doctor warns when reviewing/shipping a claim whose agent is still running.

**`gh envoy release <issue> --reason merged|closed|abandoned|manual`** — write a ReleaseMarker; the claim file is never mutated; reclaim afterwards yields a fresh generation. v0.2 adds: release **refuses while the claim has a live run** (stop it first — deleting the ground under a running agent is a must-block). v0.3 adds `--delete-worktree`/`--delete-branch` teardown, refusing on dirty worktree or unpushed commits without `--force`; remote branches are never deleted.

## 2.2 Run layer (v0.2)

The launch surface. Design intent: replace "open a new terminal, find the worktree, start the agent, babysit it" with one command and zero terminals.

```sh
gh envoy run <agent> [OPTIONS] [PROMPT] -- [AGENT_ARGS...]   # from inside a claimed worktree
gh envoy run list
gh envoy run status <run-id>
gh envoy run wait <run-id>
gh envoy run stop <run-id>
```

and the composed form, the workhorse of the parallel loop:

```sh
gh envoy claim <issue> --run <agent> [RUN_OPTIONS] -- [ADDITIONAL_CONTEXT]
```

Rules:

* **No adapters in v0.2.** The raw execution boundary covers `claude -p "<prompt>"` and `codex exec` alike, because modern agent CLIs take the prompt as an ordinary argument. Envoy composes the prompt *into an argument*; it never injects into a TUI, never manages planning stages, never interprets output. Agent-specific adapters (and the two-stage planning pipeline) return only when some agent genuinely requires bespoke handling. Prompt handoff (`prompt.md` + printed launch line) is the fallback for anything the raw boundary can't drive.
* **Prompt composition is the coordination feature** (vision item 11): the composed prompt includes the issue reference, any additional context verbatim, and the standing instruction that other agents may be active — *run `gh envoy status --json` to inspect active claims and coordination risks before touching shared surfaces*. Claimed worktrees become agent-legible.
* **Background is the default for `claim --run`** (friction principle 3); `--interactive` attaches to the terminal instead. A background start writes a queued record, spawns a hidden worker (current executable + run-id; not a public interface), and returns immediately with the run-id and inspection commands. The worker owns the child, redirects stdio to run logs, polls `try_wait` plus the stop-request marker, and records every transition.
* **Process-tree termination**: stop must kill the process *group* (setsid/killpg on Unix; Job Objects on Windows), not just the direct child — agents spawn shells and test runners, and orphaned grandchildren are exactly the residue a stop must not leave.
* Execution uses structured argument vectors (`OsString`), never shell strings. Environment is inherited by default (documented); logs may contain secrets, so status shows paths and sizes, never contents.
* `claim --run` commits the claim fully before spawn; agent failure never rolls back or releases the claim. `--run` conflicts with `--resume`/`--cd`/`--no-cd` (it replaces the nested-shell handoff) and interactive `--run` conflicts with `--json`.
* **Run-event hook**: config may name an `on_run_event` command invoked (fire-and-forget, non-blocking, failures logged not fatal) on every state transition with the run record path. This is the integration point for openfind's phone relay and anything else that wants to watch runs — Envoy publishes events; it does not build notification channels.
* Exit mapping: foreground run/`wait` return 0 on agent exit 0; nonzero agent exit → 3 with the child code preserved in `run.json`; invalid combinations and ownership violations → 2; a stopped run reports stopped and `wait` returns 0 (a stop the operator requested is not a refusal).

## 2.3 Remote writes (v0.3)

**`gh envoy ship <issue>`** / **`gh envoy ship --stack <issue>`** — the only remote-writing command; always an explicit target; no repository-wide form.

* Runs doctor; **refuses on publish-gate failure, no override**. Proceeds through merge-gate warnings, printing them prominently — publishing overlapping work for review is often the correct next move; `--strict` refuses on merge warnings too.
* When publish is clean but merge is not, the PR is created **as draft** (`--ready` overrides) — the review/merge distinction in GitHub-native terms.
* PR base derived from the claim (`base_issue`'s branch, else `base_ref`), never from branch names. Existing PR detected and reused; base mismatch reported, never silently retargeted. Idempotent re-ship pushes and reports.
* `--stack` ships unshipped ancestors bottom-up, stops at the first refusal, reports shipped/refused/not-attempted. `wait_for_issues` hold: push permitted, PR creation held until every listed issue's PR merges, awaited PRs named.
* Publish gate also warns/refuses when the claim has a **live run** — shipping under a still-writing agent is a race.
* `--reviewer` and `--label` pass through, so landing-automation tags (vision item 9) ride the same command.
* **PR body scaffold**: ship composes the mechanical body from state it already holds — issue link and acceptance criteria, stack position, diffstat, doctor summary with gate rollups, risk-path labels, per-commit list — and includes a designated empty section for the **reviewer agent's verdict**. The scaffold is Envoy's; the judgment is the companion reviewer's; the author agent never writes its own body.
* Ship never merges, retargets, force-pushes, or closes anything.

**`gh envoy sync`** — closes the loop (vision item 10): for every active claim whose PR is merged (or issue closed), write the corresponding ReleaseMarker (`--reason merged|closed`). Local writes only, safe to run habitually or from a hook; `--dry-run` lists what would release. Sync respects the live-run refusal.

---

# 3. Local State

Everything under `$(git rev-parse --git-common-dir)/envoy/` — same store from every worktree; nothing Envoy-owned in the working tree (config in-tree would skew per branch):

```text
$(git-common-dir)/envoy/
  config.yml        # optional; built-in defaults otherwise (risk paths, naming, on_run_event, ...)
  lock
  journal/          # operation journal for multi-step mutations
  claims/<issue>/<claim_id>.json
  releases/<issue>/<claim_id>.json
  runs/<run-id>/    # v0.2: run.json, prompt.md, stdout.log, stderr.log, stop-request
```

Source of truth: local Git state, claim/release files, run records, observed diffs, GitHub facts when queried. Derived state (diffs, overlaps, PR association, doctor output) is computed live and never persisted.

---

# 4. Companion Layer (out of binary, in the workflow)

These are part of the product vision but explicitly not product code; each composes with Envoy through its public surfaces (`--json`, the run hook, the PR body scaffold).

* **Planning skill** (vision 1–3): a Claude Code skill that turns a feature description into scrum-shaped issues (title, body, acceptance criteria, labels, `--onto`/`--after` dependency hints) and creates them via `gh issue create`. Output convention documented so `claim` picks up acceptance criteria for prompt composition and ship reuses them in PR bodies.
* **Reviewer skill** (vision 9): a decorrelated reviewer — spec/north-star/codebase context, *not* the author's session or reasoning; adversarial framing ("argue why this must not merge"); ideally a different model; reviews **per-commit** (the repo policy makes commits the revert unit); writes its verdict into ship's designated PR-body section. The human reads every body (calibration), deep-reads sampled PRs (risk-weighted by doctor signals + one-in-N).
* **openfind relay** (vision 7): subscribes to `on_run_event`; relays attention-needing states to the phone. Interactive sessions that ask questions mid-flight belong to the relay that owns the TTY; Envoy's background runs are the non-interactive kind and shouldn't prompt at all.

# 5. Repository Policy (normative for Envoy's own development, recommended for consumers)

* **No squash merges.** PRs land as merge commits preserving atomic, over-descriptive branch commits — the artifact the rapid-revision strategy depends on. Revert playbook: `git revert <sha>` for one bad commit; `git revert -m 1 <merge>` to unland a PR.
* **Commit atomicity survives review** via the amend-owning-commit discipline (fixes amended into the commit that owns the code, newest-to-oldest; no fixup-commit sprawl). Agents are pointed at this policy in contributing docs; the reviewer gates on commit quality, not just diff content.
* One slice = one issue = one claim: Envoy is developed through Envoy.

# 6. Safety Rules

**Must hold (store):** lock-serialized mutations; temp+rename writes; journaled multi-step operations with rollback or doctor-repairable residue; exactly-one-winner concurrent claims; write-once claims, generation-based reclaim.

**Must block:** claiming an actively claimed issue; claiming into an owned worktree; claiming a closed issue without `--force`; `--onto` a base without an active claim; releasing (or syncing away, or tearing down) a claim with a live run; running an agent outside an active claimed worktree; doctor calling publish safe under a rewritten base or wrong PR base; ship on publish failure (no override); any remote write from any command but ship; force-push, merge, retarget — structurally impossible, not just refused.

**Must warn:** overlap per the relationship/tier matrix; scope drift; stale base; uncommitted changes alongside an open PR; active claim on a closed issue or merged PR (recommend `release`/`sync`); live run during doctor review; shared untracked paths.

**Run rules:** every run bound to an exact claim generation; no shell-string construction ever; stop kills the process group; stop on a finished run is a no-op; worker disappearance yields conservative stale-run status (a PID is never proof of identity); prompt/log contents never serialized; `--dry-run` on ship prints exact commands and writes nothing.

# 7. Deferred and Dropped

**Deferred:** agent-specific adapters and planning pipelines; TUI prefill/attach/replay; automatic PR retargeting after base merge (manual restack recipe stands); multi-parent diff-base semantics (consolidation diffs are annotated as expected instead); GitHub sub-issue metadata verification; environment allowlists / secret redaction in logs; run-artifact retention/cleanup; cross-repo correlation (revive via Gerrit-style `Change-Id` trailers); semantic conflict detection; GitHub App; hosted anything; inferring issue completion from agent exit.

**Dropped:** agent commit porcelain (worktree isolation + ownership invariant suffice); `normalize`/SprintPlan ingestion in the binary (reborn as the companion planning skill); a second notification channel (openfind owns the phone); jj backend for now (agents know git cold; the observe layer is backend-agnostic enough to revisit).

# 8. Design Notes

* **Why the run layer is core, not scope creep:** the operator's bottleneck is attention across parallel background agents — N terminals, N directory pointers, N "done yet?" polls. Run records + `run list`/`status`/`wait` + the live-run column in `status` collapse that into one pane. Doctor's coordination checks are what make wide parallelism *cheap* — at N=1 they're insurance, at N=8 collisions are a first-order tax on throughput. Verifier and launcher are the same product seen from two sides of the same constraint.
* **Why no adapters:** the raw boundary + prompt-as-argument covers today's agent CLIs; adapters are a maintenance treadmill against weekly-changing flags. Cut until an agent forces the issue.
* **Why ship gates on publish, not merge:** a PR is a coordination mechanism, not an outcome; blocking publication on overlap warnings trains reflexive `--allow-warnings` typing, which is worse than no gate. Draft-by-default encodes "reviewable, not landable."
* **Why the reviewer is outside the binary:** review is judgment over spec + codebase context; Envoy owns mechanical truth (what changed, what overlaps, what gates pass) and hands judgment a scaffold. Keeping them separate keeps the reviewer swappable and the binary honest. (It is also, not coincidentally, the shape of a spec-aware behavioral gate a human trusts instead of re-deriving — the Maida shape; this workflow is its reference deployment.)
