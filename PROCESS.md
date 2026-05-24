# Process

Companion to `SPEC.md` and `ARCHITECTURE.md`. Captures development discipline. The disciplines below are stated in general form; the specific tools used to enforce them are implementation choices that may evolve.

## Repository shape

- Solo development. Direct to main. No pull requests.
- License: MIT.

## Canonical commands

The Rust toolchain provides the canonical commands directly via `cargo`. The principle of "every routine workflow has a single named entrypoint" still applies; for a Rust-only project the entrypoints are `cargo` subcommands rather than a Makefile gateway.

- `cargo build` / `cargo build --release` — compile.
- `cargo run` — run the binary.
- `cargo test` — unit + integration tests.
- `cargo fmt` — auto-format. `cargo fmt --all -- --check` verifies without writing.
- `cargo clippy --all-targets --all-features -- -D warnings` — lint with warnings as errors.

The composite "check" gate (format-check + lint + tests) is enforced by the pre-commit hook and CI. Any other composite operations live as small scripts under `scripts/`.

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

## Versioning and releases

Releases are cut by [release-plz](https://release-plz.ieni.dev/) off the back of the commit log. The shape: every push to `main` updates an auto-maintained "Release PR" that bumps `Cargo.toml`, regenerates `CHANGELOG.md`, and lists what would ship. Merging that PR creates a `vX.Y.Z` git tag, which fires `release.yml` to upload cross-platform binaries to a non-prerelease GitHub release. The same merge also publishes the crate to [crates.io](https://crates.io/crates/agent-mux). Cutting a release is therefore: review the open Release PR, merge it.

### Conventional Commits

The commit log is the source of truth for what the next version should be. Every commit on `main` follows [Conventional Commits 1.0](https://www.conventionalcommits.org/):

    <type>(<scope>): <subject>

`<type>` is one of `feat`, `fix`, `docs`, `test`, `refactor`, `chore`, `perf`, `build`, `ci`, `style`. `<scope>` is the area touched (`session`, `attention`, `remote`, `embedded-pty`, `notifications`, `readme`, …) and is optional but encouraged — the existing log is uniformly scoped and it stays that way. Breaking changes are marked with a `!` after the type (`feat!:`) or a `BREAKING CHANGE:` footer in the body.

release-plz reads these to choose the next version: `fix` bumps the patch, `feat` bumps the minor, breaking changes bump the minor (pre-1.0) or the major (post-1.0). Non-conforming subjects are silently ignored by the bump logic — the change still lands, it just doesn't move the version.

### The two release channels

- **Tagged (`vX.Y.Z`).** Stable, immutable, pinnable. Cut when the release-plz PR merges. Published to crates.io alongside the GitHub release.
- **`latest`** (rolling). Replaced on every push to `main`. Marked as a prerelease. GitHub-only — crates.io is tagged-releases-only since pre-releases on the registry would muddy `cargo install agent-mux` semantics.

### Setup

Two repository secrets, both required for the release-plz workflow to function:

- **`RELEASE_PLZ_TOKEN`** — a fine-grained PAT scoped to this repo with `contents: write` and `pull-requests: write`. The default `GITHUB_TOKEN` can't be used because tag pushes made with it don't trigger downstream workflows (an anti-loop safety in Actions), which would leave `release.yml` un-fired on tag creation.
- **`CARGO_REGISTRY_TOKEN`** — a [crates.io API token](https://crates.io/me) scoped to publish updates of `agent-mux`. release-plz's `release` command invokes `cargo publish` against this token when a merged Release PR ships a new version. The first-time publish of a brand-new crate goes through the same path — no manual `cargo publish` warmup needed, provided the crate name is still available at registration time.

If either secret is missing or expired the release-plz workflow fails noisily and no PR moves — the intended failure mode. A missing `CARGO_REGISTRY_TOKEN` does *not* affect the GitHub release path; the binary upload via `release.yml` still works, only the crates.io publish step fails.

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
