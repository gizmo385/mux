# Process

Companion to `SPEC.md` and `ARCHITECTURE.md`. Captures development discipline. The disciplines below are stated in general form; the specific tools used to enforce them are implementation choices that may evolve.

## Repository shape

- Solo development. Direct to main. No pull requests.
- License: MIT.

## Canonical commands

A `Makefile` at the repository root names the common workflows: `install`, `dev`, `build`, the umbrella `check`, the unit and end-to-end test suites. The principle is that every routine workflow has a single named entrypoint that is stable across language and tooling churn — contributors and agents do not memorise the current package manager's invocation, they call `make test`. The underlying tool may change; the names do not.

Run `make` (no target) for the list.

## Living documents

Four documents are updated *in the same commit as the change they describe*. A commit that alters user-observable behaviour, or that completes a tracked TODO, without touching the relevant document is a bug to be amended.

- **`README.md`** — what the project is, how to run it, current status, links to `SPEC.md` and `ARCHITECTURE.md` for the technically curious. Plain language. Updated when user-observable behaviour changes.
- **`FEATURES.md`** — feature ledger, grouped by release (or milestone). Each entry one line, marked ✓ shipped or ⋯ in progress. Plain language. Updated when a feature changes status.
- **`TODO.md`** — flat backlog. Each entry tagged with `#area` (and `#release` if the project tags releases). Done items deleted, not struck through. Updated when an item is added, completed, or abandoned. **New ideas go in here first**, before any decision about whether to implement now or later.
- **`ACCEPTANCE.md`** — criteria for milestones. Updated alongside FEATURES.md and TODO.md as scope settles.

Four further documents are canonical reference, updated when their scope shifts rather than commit-by-commit: `SPEC.md` (what the project is), `ARCHITECTURE.md` (how it's built), `PROCESS.md` (how we work — this file), `CLAUDE.md` (agent onboarding and patterns established by feedback).

## Engineering disciplines

These rules apply across every language in the codebase, present and future. The tools used to enforce them may evolve; the rules do not.

- **Tests.** New behaviour ships with tests. The full suite passes before every commit.
- **Agent-testable.** Every change is verifiable end-to-end without manual interaction. Tests live at the layer where the behaviour does. The discipline keeps both human and agent contributors un-stuck: nobody has to ask anyone else "does it still work?" When a bug is surfaced by clicking around, the first move is a regression test that fails for the same reason; the fix follows.
- **Linting.** Code lints clean before every commit. Warnings are treated as errors. Mechanizable architectural disciplines from `ARCHITECTURE.md` are encoded as lint rules wherever possible (see Code review).
- **Formatting.** Code is auto-formatted before every commit. No formatting churn lands in feature commits.
- **Always green, always current.** A commit that does not pass tests, lint, and format checks does not exist on `main`.

The specific test runner, linter, formatter, and language toolchain are choices that follow the work. Only the disciplines are pinned.

A local hook may run a fast subset (changed files only) for speed; CI runs the full check suite. The hook is for fast feedback during work; CI is the source of truth.

## Pre-commit hooks

Hard strictness. Format, lint, and tests must pass; the commit is refused otherwise.

`--no-verify` is reserved for genuine emergencies — recovery from a corrupt state, escaping a tooling bug — and is never used to defer fixing legitimate failures.

## Continuous integration

CI runs the full check suite on every push: format-check, lint, typecheck, tests, and the build. CI failure is a hair-on-fire signal — the rule is "always green on `main`," and a red CI is a bug to be fixed before any further work.

## Code review

Code review is layered. Each layer catches what the cheaper layers cannot.

**Layer 1 — Lint, every commit.** The mechanizable architectural disciplines from `ARCHITECTURE.md` are encoded as lint rules wherever possible. The rule: if a discipline can be expressed in lint, it goes in lint. Lint is free, runs every commit, and does not negotiate.

**Layer 2 — Project-aware review.** A custom review skill (`.claude/skills/agent-mux-review/`) that reads `SPEC.md`, `ARCHITECTURE.md`, and `PROCESS.md` before looking at the staged changes. Catches what lint cannot: architectural-contract violations beyond simple pattern-matching; drift between spec and implementation; missing updates to `FEATURES.md`, `TODO.md`, or `README.md`; naming and abstraction concerns weighed against the project's idioms; principles in this document that the diff has slipped past. Surfaces findings before fixing, so judgment calls stay in the loop. Runs at the end of every meaningful chunk of work, before commit.

The skill's review categories are filled in as project-specific rules emerge. Until enough rules have settled, the generic `my-code-review` skill is an acceptable but inferior stand-in for this layer.

**Layer 3 — Generic-smell pass.** A generic code-review skill catches the standard concerns that are not project-specific: duplication, dead code, missing error handling, naming inconsistencies, simplification opportunities. Optional once Layer 2 is reliable; useful in the interim.

**Layer 4 — Multi-agent review at milestones.** `/ultrareview` is invoked at release tags and marker tags for a heavier, multi-perspective pass. User-triggered, billed; reserved for moments where heavyweight scrutiny earns its keep.

**Layer 5 — Adversarial pass, on demand.** When stakes are high — a particularly dense change, or one that crosses an architectural boundary in a non-trivial way — a second reviewer reads the first reviewer's findings and asks what was missed.

"Meaningful," for the purpose of Layers 2 and 3 in the daily loop, includes anything touching core logic, public interfaces, or non-trivial UI. Doc-only edits, comment-only edits, and trivial configuration tweaks may skip the agent layers; Layer 1 runs unconditionally.

## Parallel work via worktree agents

Worktree-based agent parallelism is a tool, not a default. Use it when both conditions hold:

- The task touches files that do not overlap with the current main-thread work.
- The task is at least fifteen to twenty minutes of focused work.

Below that threshold, merge overhead consumes the gain.

Good candidates: independent features once the spine is in place; cross-cutting refactors that do not conflict with active feature work; documentation polish during implementation; the code review of a finished chunk while the next chunk begins.

Bad candidates: anything that touches a foundational interface that everything depends on; foundational scaffolding; work whose boundaries are not yet clear.

### Integration

Worktree-agent output is **rebased** onto `main`, never merged. Each agent must produce commits that are self-contained — touching only files outside the main-thread work — so integration is a fast-forward or a clean cherry-pick. Merge commits are forbidden in this repository. If a rebase produces conflicts, the conflict is fixed in the agent branch (or the agent is re-run against current `main`); merge commits are not used to paper over the friction.

The boundary discipline that makes rebase clean is the same discipline that makes the work parallel-safe: if two branches touch the same file, they should not have been parallel in the first place.
