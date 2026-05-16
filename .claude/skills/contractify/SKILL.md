---
name: contractify
description: Initialize a fresh repo, or modernize an existing one, with the living-documents + safety-nets contract — eight canonical docs, a Makefile gateway, hard-strict pre-commit hooks, single-job CI, and a layered code-review pipeline. Language-aware for TypeScript, Python, Rust, and Go.
---

# contractify

This skill installs a repo-hygiene contract on any project. The contract has three pillars:

1. **Eight canonical documents**, four of which (`README.md`, `FEATURES.md`, `TODO.md`, `ACCEPTANCE.md`) are updated commit-by-commit, and four of which (`SPEC.md`, `ARCHITECTURE.md`, `PROCESS.md`, `CLAUDE.md`) are updated when their scope shifts. The load-bearing rule: *a commit that alters user-observable behaviour, or that completes a tracked TODO, without touching the relevant document is a bug to be amended.*
2. **A Makefile gateway** with stable target names (`install`, `dev`, `build`, `check`, `format`, `lint`, `typecheck`, `test`, `e2e`, `clean`, `help`) that abstracts the language toolchain. `make check` is the composite safety gate. Pre-commit hooks bind to a fast subset; CI mirrors the full `make check` plus `make build`.
3. **A five-layer code-review pipeline**: mechanical (lint/format/typecheck/test, every commit), project-aware (a custom skill that reads the project's own canonical docs), generic-smell pass, multi-agent review at milestones, adversarial pass on demand.

The skill operates in two modes:

- **initialize** — fresh repo. Lay down the whole contract.
- **modernize** — existing repo. Audit what's present, fill gaps with `.proposed` siblings where existing files conflict; never clobber.

Language-aware coverage at the toolchain layer: **TypeScript/Node** (pnpm, npm), **Python** (uv, pip), **Rust** (cargo), **Go** (go modules). Anything else gets a stub Makefile and a "fill these in" note.

The skill commits per phase, dogfooding the "always green, always current" + "commit when good" rules. Each commit must pass `make check` from Phase 4 onward.

Per the user's global rule, commits must **not** carry a `Co-Authored-By: Claude` trailer.

---

## Phase 0 — DETECT

Determine mode, language, package manager, and git state. Ask the user what cannot be inferred.

### Mode detection

```
if .git/ does not exist:
  AUTO-INIT: run `git init -b main` immediately. Print: "no git repo here —
    initialised with `git init -b main`."
  mode = initialize
  continue.
elif working tree dirty (`git status --porcelain` non-empty):
  ERROR: working tree must be clean. Stash or commit changes, then re-run.
elif current branch is not main/master:
  WARN: current branch is X, not main/master. Confirm to proceed.
elif no living docs and no Makefile present:
  mode = initialize
else:
  mode = modernize
```

The empty-directory case is the most common starting point for greenfield projects. The user invoked `/contractify`, which means they want a contract-shaped repo — asking permission to run `git init` is friction that buys nothing. Auto-init and announce; the user can object during the Phase 0 question batches that follow if they meant to be elsewhere.

### Language detection

| Marker file | Language |
| --- | --- |
| `package.json` + `tsconfig.json` (or `*.ts` files) | TypeScript |
| `package.json` only (no TS) | JavaScript (treated as TS-style with looser typecheck) |
| `pyproject.toml` or any `*.py` file | Python |
| `Cargo.toml` | Rust |
| `go.mod` | Go |
| (none of the above) | unknown — stub Makefile, instruct user |

If multiple markers exist (e.g. polyglot repo), ask the user which language is primary. The skill installs a single primary toolchain; secondary languages get manual treatment outside this skill.

### Package manager detection (TypeScript)

| Lock file | Manager |
| --- | --- |
| `pnpm-lock.yaml` | pnpm |
| `package-lock.json` | npm |
| `yarn.lock` | yarn (treat as npm-equivalent for Make targets) |
| `bun.lockb` | bun (treat as npm-equivalent for Make targets) |
| (none) | ask: pnpm (default) or npm |

### Package manager detection (Python)

| Lock file | Manager |
| --- | --- |
| `uv.lock` | uv |
| `poetry.lock` | poetry (treat as pip-equivalent for Make targets) |
| `requirements.txt` | pip |
| (none) | ask: uv (default) or pip |

### Questions to ask the user

Two question types, asked through different mechanisms:

- **Multi-choice questions** (mode, branching model, release tags, e2e, license, strict mypy) — use **AskUserQuestion**. These have a small fixed set of valid answers, so the user picks from a list. AskUserQuestion is capped at 4 questions per call; split into batches.
- **Free-form questions** (project tagline, first release name + goal, author name override) — ask as **plain text in chat**, one at a time. **Do not shoehorn these into AskUserQuestion** — coercing a tagline question into a multi-choice ("Is this a CLI? A library? A web service?") is worse than asking nothing, because the canned options bias the user away from their actual answer. Ask `"What's the tagline for {PROJECT_NAME}? One short sentence."` and let the user type.

Do not write any files until all questions are answered; content placeholders are substituted at write-time.

**Batch 1 — workflow (4 questions):**

The first slot is conditional on whether the language was auto-detected.

- **If language was auto-detected** (Phase 0 found a marker file): slot 1 is **mode confirmation** — show what was auto-detected (mode, language, package manager) and ask for confirmation. Allow override.
- **If language is unknown** (no marker files, e.g. fresh empty directory): slot 1 is **language selection** with four options: `TypeScript`, `Python`, `Rust`, `Go`. AskUserQuestion auto-injects "Other" as a fifth choice, which the user picks for any other language and which routes to the Unknown / stub-Makefile path. Do **not** include "Unknown" as an explicit fifth option — the tool caps at four and AskUserQuestion will reject the call. There is no auto-detected mode/language to confirm; defer mode confirmation to the implicit "are these the correct settings?" review at the end of Batch 1.

The remaining three slots are unconditional:

2. **Other human contributors on this repo?** — *Just me + Claude* (solo) → keep PROCESS.md's "Parallel work via worktree agents" section, since worktree-agent parallelism is a Claude-specific workflow that fits a solo dev. *Other humans on the team* → strip the worktree section and reword PROCESS.md's "Repository shape" section to mention pull-request-based workflow rather than direct-to-main. The phrasing matters: this question is about whether other humans contribute, not about whether Claude works alone.
3. **Release tags?** — yes → ACCEPTANCE.md gets per-release section template; CLAUDE.md gets the "release tags require explicit sign-off" seed pattern. No → ACCEPTANCE.md becomes a one-line placeholder; the release-tags pattern is omitted from CLAUDE.md.
4. **End-to-end tests in scope?** — yes → wire `make e2e` to the *language-appropriate* e2e tool (see below). No → `make e2e` becomes `@echo "no e2e tests configured"`. The wiring is language-specific; do **not** offer Playwright for non-TypeScript projects.

   - TypeScript → Playwright (`pnpm exec playwright test`); creates `e2e/` with a stub spec.
   - Python → pytest (`uv run pytest tests/e2e`); creates `tests/e2e/` with `__init__.py` and a stub test.
   - Rust → `cargo test --test '*'` against `tests/`; integration tests are the e2e path.
   - Go → `go test -tags=e2e ./...`; create one tagged file under `internal/e2e/`.
   - Unknown → ask whether to ask at all; if yes, leave a TODO.

**Batch 2 — language-specific (only when relevant):**

- (Python only) **Strict mypy?** — yes (default) → mypy strict + `mypy_path = "src"` and `explicit_package_bases = true` for src-layout. No → mypy non-strict.

Skip this batch entirely if no language-specific question applies.

**Batch 3 — project content:**

The contract's first commit ships with the user's actual words, not boilerplate. Ask for content; substitute at write-time. Do not write `(To be filled in.)` or generic prose anywhere a real answer is reachable.

License (multi-choice, AskUserQuestion):

5. **License** — four options visible (capped by AskUserQuestion): `MIT` (default, recommended first), `Apache-2.0`, `GPL-3.0`, `BSD-3-Clause`. AskUserQuestion auto-injects "Other" as a fifth choice; user picks Other for `LGPL-3.0`, `MPL-2.0`, `proprietary`, `unlicensed`, or any other license, then types the SPDX identifier as free text.

The skill **writes a `LICENSE` file** in Phase 2 with the canonical text for the chosen license. For canonical OSI licenses (the four picklist options plus `LGPL-3.0` and `MPL-2.0`), the skill knows the standard text and substitutes `{YEAR}` and `{AUTHOR_NAME}` directly. For other or unfamiliar SPDX identifiers, **try `WebFetch` against `https://opensource.org/license/{spdx-id}` (or `https://www.gnu.org/licenses/` for GNU licenses) first to retrieve the canonical text**; only fall back to a stub `LICENSE` (containing `SPDX-License-Identifier: {ID}` plus the copyright line) if the fetch fails. For `proprietary`, write the one-paragraph "all rights reserved" stub. For `unlicensed`, write the contact-the-author stub. The stub paths are last resorts, not the default — canonical text always wins when reachable.

Free-form prompts in chat (one at a time, plain text — **NOT** AskUserQuestion):

6. **Project tagline** — ask: `"What's the tagline for {PROJECT_NAME}? One short sentence describing what it is and who it's for."` Take the user's reply verbatim. **Do not** convert this into a multi-choice with canned categories ("Is it a CLI? A library? A web service?") — the canned options bias the user away from their actual answer. If the user genuinely deflects ("I don't know yet"), prompt once more for three-or-four words ("just give me a rough framing — `a CLI for X` or `a library that does Y`"); accept whatever they say. Going forward without a tagline is the failure mode.
7. **Author name** — ask: `"Author name for the LICENSE copyright line? Default: {git config user.name}."` Take the reply, or use the default if the user just hits enter / says "use default".
8. (If release tags = yes:) **First release name and goal** — ask in chat: `"What should release 0 be called, and what's its goal? Format: 'r0-name: one-line goal.' Default: 'r0-bootstrap: the project boots and the contract is in place.'"` Parse the user's reply or use the default.

Sections that genuinely cannot be answered before architecture takes shape (`SPEC.md` Functionality, `ARCHITECTURE.md` Components, `ARCHITECTURE.md` Disciplines, etc.) are written as section headings followed by an HTML comment explaining what to put there. The rendered markdown shows clean section headings — no visible "fill in" prose.

Display the planned phase list and per-phase commits before beginning Phase 1. Let the user veto or proceed.

### Placeholder substitution table

| Placeholder | Source |
| --- | --- |
| `{PROJECT_NAME}` | directory name (ask to confirm) |
| `{PROJECT_TAGLINE}` | Batch 3, question 5 |
| `{LANGUAGE}` | detected in Phase 0 |
| `{PACKAGE_MANAGER}` | detected in Phase 0 (e.g. `pnpm`, `uv`, `cargo`, `go modules`) |
| `{PACKAGE_NAME}` | (Python only) `{PROJECT_NAME}` with hyphens replaced by underscores — Python identifier rules |
| `{LICENSE}` | Batch 3, question 6 (default `MIT`) |
| `{AUTHOR_NAME}` | Batch 3, question 7 (default `git config user.name`) |
| `{YEAR}` | current year |
| `{FIRST_RELEASE_NAME}` | Batch 3, question 8 (only if tagging) |
| `{FIRST_RELEASE_GOAL}` | Batch 3, question 8 (only if tagging) |

Every template substitutes these at write-time. After substitution, no `{...}` placeholders should remain in any written file. Verify with `grep -r '{[A-Z_]*}' .` after Phase 2 commits.

---

## Phase 1 — AUDIT (modernize mode only)

Skip this phase entirely in initialize mode.

Read-only audit. Produce a markdown table reporting which contract items are `present`, `partial`, or `missing`, with a proposed action for each gap. **Pause for user confirmation before Phase 2 writes anything.** The user may opt items in or out individually.

### Checks

| Item | Pass criterion |
| --- | --- |
| `PROCESS.md` exists | file at repo root |
| `PROCESS.md` content | mentions canonical commands, living-docs rule, pre-commit, CI, layered review |
| `SPEC.md` exists | file at repo root |
| `ARCHITECTURE.md` exists | file at repo root |
| `README.md` exists | file at repo root |
| `FEATURES.md` exists | file at repo root |
| `TODO.md` exists | file at repo root |
| `ACCEPTANCE.md` exists | file at repo root |
| `CLAUDE.md` exists | file at repo root |
| `Makefile` exists | yes |
| Make targets | `.PHONY`-declared targets `install dev build check format lint typecheck test e2e clean help` all present |
| Default goal is `help` | `.DEFAULT_GOAL := help` |
| `make check` is composite | invokes format-check + lint + typecheck + test |
| Pre-commit hook installed | `.git/hooks/pre-commit` exists *and* references a known framework (husky, lefthook, `pre-commit`, or shell stub) |
| Hook runs `make check` (fast) | config or hook script references it |
| CI workflow | `.github/workflows/*.yml` has a job that runs `make check` |
| Lint config strictness | language-appropriate config present and no warnings-allowed escape |
| Layer 2 review skill | `.claude/skills/<project>-review/SKILL.md` exists |

### Conflict policy

For files that exist but don't match the contract: **never overwrite**. Write `<file>.proposed` alongside (e.g. `PROCESS.md.proposed`, `Makefile.proposed`) and instruct the user in Phase 8's handoff to diff and merge by hand. Existing files marked `partial` get this treatment; files marked `missing` are written normally; files marked `present` are left alone.

For `.git/hooks/pre-commit` not managed by a known framework: emit `pre-commit.proposed` (a shell-script alternative) and include manual installation instructions in the handoff.

For CI: never edit existing workflows. If a workflow already runs `make check`, mark CI `present` and skip Phase 6. Otherwise add a new `.github/workflows/check.yml`.

---

## Phase 2 — DOCS

Lay down the eight canonical documents. In modernize mode, skip files marked `present` and use the conflict policy for `partial` files.

Use `{PROJECT_NAME}` as a placeholder for the project name (derive from directory name; ask the user to confirm). Other placeholders: `{LANGUAGE}` (TypeScript/Python/Rust/Go), `{LICENSE}` (default MIT, ask user).

### `PROCESS.md`

```markdown
# Process

Companion to `SPEC.md` and `ARCHITECTURE.md`. Captures development discipline. The disciplines below are stated in general form; the specific tools used to enforce them are implementation choices that may evolve.

## Repository shape

- {SOLO_OR_TEAM_BRANCHING}
- License: {LICENSE}.

(Where `{SOLO_OR_TEAM_BRANCHING}` is `Solo development. Direct to main. No pull requests.` for solo projects, or `Pull-request workflow. Branch off main, open a PR, merge after CI is green and review approves.` for team projects.)

## Canonical commands

A `Makefile` at the repository root names the common workflows: `install`, `dev`, `build`, the umbrella `check`, the unit and end-to-end test suites. The principle is that every routine workflow has a single named entrypoint that is stable across language and tooling churn — contributors and agents do not memorise the current package manager's invocation, they call `make test`. The underlying tool may change; the names do not.

Run `make` (no target) for the list.

## Living documents

Four documents are updated *in the same commit as the change they describe*. A commit that alters user-observable behaviour, or that completes a tracked TODO, without touching the relevant document is a bug to be amended.

- **`README.md`** — what the project is, how to run it, current status, links to `SPEC.md` and `ARCHITECTURE.md` for the technically curious. Plain language. Updated when user-observable behaviour changes.
- **`FEATURES.md`** — feature ledger, grouped by release (or milestone). Each entry one line, marked ✓ shipped or ⋯ in progress. Plain language. Updated when a feature changes status.
- **`TODO.md`** — flat backlog. Each entry tagged with `#area` (and `#release` if the project tags releases). Done items deleted, not struck through. Updated when an item is added, completed, or abandoned. **New ideas go in here first**, before any decision about whether to implement now or later.
- **`ACCEPTANCE.md`** — {ACCEPTANCE_DESCRIPTION}

(Where `{ACCEPTANCE_DESCRIPTION}` is `per-release acceptance gates. A release tag only lands on main once the matching section's criteria are all marked shipped. The list for a release is fleshed out when work on that release begins; we don't write speculative criteria for far-off releases.` for tagging projects, or `criteria for milestones. Updated alongside FEATURES.md and TODO.md as scope settles.` for untagged projects.)

Four further documents are canonical reference, updated when their scope shifts rather than commit-by-commit: `SPEC.md` (what the project is), `ARCHITECTURE.md` (how it's built), `PROCESS.md` (how we work — this file), `CLAUDE.md` (agent onboarding and patterns established by feedback).

{TAGS_SECTION}

(Where `{TAGS_SECTION}` is the full block below for tagging projects, or **omitted entirely** — the entire `## Tags` heading and its body — for untagged projects:

```markdown
## Tags

- **Release tags** at the close of each release: `r0-name`, `r1-name`, and so on.
- **Marker tags** at notable in-between moments: optional, project-defined.

No semver unless the project is a published library.
```

For untagged projects, the section is removed cleanly so there is no orphan heading or "(does not apply)" leftover prose.)

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

**Layer 2 — Project-aware review.** A custom review skill (`.claude/skills/{PROJECT_NAME}-review/`) that reads `SPEC.md`, `ARCHITECTURE.md`, and `PROCESS.md` before looking at the staged changes. Catches what lint cannot: architectural-contract violations beyond simple pattern-matching; drift between spec and implementation; missing updates to `FEATURES.md`, `TODO.md`, or `README.md`; naming and abstraction concerns weighed against the project's idioms; principles in this document that the diff has slipped past. Surfaces findings before fixing, so judgment calls stay in the loop. Runs at the end of every meaningful chunk of work, before commit.

The skill's review categories are filled in as project-specific rules emerge. Until enough rules have settled, the generic `my-code-review` skill is an acceptable but inferior stand-in for this layer.

**Layer 3 — Generic-smell pass.** A generic code-review skill catches the standard concerns that are not project-specific: duplication, dead code, missing error handling, naming inconsistencies, simplification opportunities. Optional once Layer 2 is reliable; useful in the interim.

**Layer 4 — Multi-agent review at milestones.** `/ultrareview` is invoked at release tags and marker tags for a heavier, multi-perspective pass. User-triggered, billed; reserved for moments where heavyweight scrutiny earns its keep.

**Layer 5 — Adversarial pass, on demand.** When stakes are high — a particularly dense change, or one that crosses an architectural boundary in a non-trivial way — a second reviewer reads the first reviewer's findings and asks what was missed.

"Meaningful," for the purpose of Layers 2 and 3 in the daily loop, includes anything touching core logic, public interfaces, or non-trivial UI. Doc-only edits, comment-only edits, and trivial configuration tweaks may skip the agent layers; Layer 1 runs unconditionally.

## Parallel work via worktree agents

(Include this section only if the project is a solo Claude-Code project. Strip otherwise.)

Worktree-based agent parallelism is a tool, not a default. Use it when both conditions hold:

- The task touches files that do not overlap with the current main-thread work.
- The task is at least fifteen to twenty minutes of focused work.

Below that threshold, merge overhead consumes the gain.

Good candidates: independent features once the spine is in place; cross-cutting refactors that do not conflict with active feature work; documentation polish during implementation; the code review of a finished chunk while the next chunk begins.

Bad candidates: anything that touches a foundational interface that everything depends on; foundational scaffolding; work whose boundaries are not yet clear.

### Integration

Worktree-agent output is **rebased** onto `main`, never merged. Each agent must produce commits that are self-contained — touching only files outside the main-thread work — so integration is a fast-forward or a clean cherry-pick. Merge commits are forbidden in this repository. If a rebase produces conflicts, the conflict is fixed in the agent branch (or the agent is re-run against current `main`); merge commits are not used to paper over the friction.

The boundary discipline that makes rebase clean is the same discipline that makes the work parallel-safe: if two branches touch the same file, they should not have been parallel in the first place.
```

### `FEATURES.md`

If the project tags releases:

~~~markdown
# Features

Feature ledger. Grouped by release. One line per entry. Plain language.

Legend: ✓ shipped · ⋯ in progress

## Release 0 — {FIRST_RELEASE_NAME}

<!-- Add feature entries as they ship or move to in-progress. -->
~~~

If the project does not tag releases, replace the `## Release 0 — ...` heading with `## Milestone — initial work` and drop the `{FIRST_RELEASE_NAME}` substitution.

### `TODO.md`

~~~markdown
# TODO

Flat backlog. Each entry tagged with `#area` (and `#release` if the project tags releases). Done items deleted, not struck through.

**New ideas go in here first.** When a feature, polish item, or design idea surfaces — whether from the user or the assistant — the first move is an entry below with the rationale captured at idea-time. Then, separately, decide whether to implement now or leave it. The default is "codify, then defer"; pulling an entry forward is a second decision the user makes deliberately.

## Backlog

<!-- Phase 7 of contractify will add the first real entry here. Add others as ideas surface. -->
~~~

### `ACCEPTANCE.md` (tagged-releases variant)

~~~markdown
# Acceptance

Per-release acceptance gates. A release tag only lands on `main` once the matching section's criteria are all marked shipped. The list for a release is fleshed out when work on that release begins; we don't write speculative criteria for far-off releases.

Legend: ✓ shipped · ⋯ in progress · ✗ failed

## Release 0 — {FIRST_RELEASE_NAME}

**Goal.** {FIRST_RELEASE_GOAL}

**Functional criteria** (from `SPEC.md`):

<!-- Add criteria as the release scope is fleshed out. -->

**Implicit criteria** (from `ARCHITECTURE.md` and `PROCESS.md`):

- ⋯ all `make check` targets green
- ⋯ test suite covers the criteria above
- ⋯ living documents updated

**Acceptance test.** A short scripted walkthrough that demonstrates the release criteria, end-to-end.

**Verdict.** (Filled in at release time.)
~~~

### `ACCEPTANCE.md` (untagged variant)

~~~markdown
# Acceptance

This project does not tag releases. Acceptance criteria are tracked at the milestone level in `FEATURES.md` and as `#acceptance`-tagged items in `TODO.md`.
~~~

### `SPEC.md`

~~~markdown
# Specification

What this project is. The authoritative description of scope and behaviour, distinct from `ARCHITECTURE.md` (which is about how it's built).

## Overview

{PROJECT_TAGLINE}

<!-- Expand as scope clarifies: who it's for, what makes it distinctive, what success looks like. -->

## Glossary

<!-- Define terms used elsewhere in this document and in ARCHITECTURE.md. Each term once, here. -->

## Functionality

<!-- What the project does, in user-observable terms. Behaviour, not implementation. -->

## Roadmap

<!-- Releases or milestones in chronological order, each with a one-line goal. Detail fleshes out as each one comes into focus. -->

## Out of scope

<!-- Things this project deliberately does not do. Capture these explicitly — the absence is as load-bearing as the presence. -->
~~~

### `ARCHITECTURE.md`

~~~markdown
# Architecture

How this project is built. Engineering choices and the disciplines that follow from them. Companion to `SPEC.md` (what it is) and `PROCESS.md` (how we work).

## Status

<!-- Where the architecture currently stands. Updated when scope shifts, not commit-by-commit. -->

## Tech stack

- Language: {LANGUAGE}
- Package manager: {PACKAGE_MANAGER}
<!-- Add frameworks and key dependencies as they're chosen. -->

## Components

<!-- The major pieces of the system and how they relate. A diagram is welcome but not required. -->

## Disciplines

<!-- Architectural rules that don't fit cleanly into language-level lint. Each rule stated as a constraint with a reason. These rules are the basis of Layer 2 review (see PROCESS.md). -->

## Open questions

<!-- Decisions deferred. Each one with a brief reason for the deferral. -->
~~~

### `README.md`

The README uses inline `code spans` rather than fenced code blocks for shell commands, to keep the file flat and readable.

If the project tags releases:

~~~markdown
# {PROJECT_NAME}

{PROJECT_TAGLINE}

## Status

Current release: `{FIRST_RELEASE_NAME}` (in progress).

## How to run

Run `make install` to set up dependencies, then `make dev` to start the dev environment. Run `make` (no target) for the full list of commands.

## Documents

- `SPEC.md` — what this project is.
- `ARCHITECTURE.md` — how it's built.
- `PROCESS.md` — how we work.
- `FEATURES.md` — what's shipped.
- `TODO.md` — what's planned.
- `ACCEPTANCE.md` — release gates.

## License

{LICENSE}
~~~

If the project does not tag releases, replace the `## Status` block with:

~~~markdown
## Status

<!-- One sentence on what works today and what's next. -->
~~~

### `CLAUDE.md`

~~~markdown
# {PROJECT_NAME}

{PROJECT_TAGLINE}

## Required reading before substantive work

Three documents form the canonical brief. Read all three before any non-trivial task in this repository.

- **`SPEC.md`** — what this project is.
- **`ARCHITECTURE.md`** — how it is built.
- **`PROCESS.md`** — how we work.

The disciplines in `ARCHITECTURE.md` and `PROCESS.md` are not advisory; they govern the work.

## Patterns established by feedback

Load-bearing patterns the user has explicitly asked for. These live here, in git, so they survive a laptop death.

### Codify new ideas in TODO.md before deciding to implement

When a new feature, polish item, or design idea surfaces in conversation — whether it came from the assistant or the user — the immediate move is an entry in `TODO.md` with the rationale captured at idea-time. _Then_, separately, decide whether to implement now or leave it. Do not ask "should we build this now?" without writing it down first; ideas evaporate, and the in-conversation tradeoff analysis is the most valuable part to preserve. The default is "codify, then defer"; pulling the entry forward is a second decision the user makes deliberately.

### Release tags require explicit sign-off

(Include only if the project tags releases.)

Even when the user has granted broad autonomy ("dive in", "go with your gut"), creating a release tag (`rN-name`) is a separate decision and requires explicit sign-off in that turn or one of the immediately preceding turns. Living-doc reconciliation, the ship-it commit, and the tag are three separate acts; bundling them all under a generic autonomy grant is overreach.

<!-- Add patterns as feedback surfaces them. Each pattern: lead with the rule, then a Why: line and a How to apply: line. -->
```

### `LICENSE`

Phase 2 also writes a `LICENSE` file in the repo root. The chosen license (Batch 3, question 6) determines which text:

- **`MIT`** — write the canonical MIT License text with `{YEAR}` and `{AUTHOR_NAME}` substituted into the copyright line. Standard text:

  ```
  MIT License

  Copyright (c) {YEAR} {AUTHOR_NAME}

  Permission is hereby granted, free of charge, to any person obtaining a copy
  of this software and associated documentation files (the "Software"), to deal
  in the Software without restriction, including without limitation the rights
  to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
  copies of the Software, and to permit persons to whom the Software is
  furnished to do so, subject to the following conditions:

  The above copyright notice and this permission notice shall be included in all
  copies or substantial portions of the Software.

  THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
  IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
  FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
  AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
  LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
  OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
  SOFTWARE.
  ```

- **`Apache-2.0`, `BSD-3-Clause`** — write the canonical text for the chosen license. If the implementer doesn't have the canonical text in context, fetch it from <https://opensource.org/licenses/> or write a stub `LICENSE` containing only the SPDX identifier (`SPDX-License-Identifier: Apache-2.0`) plus the copyright line, and add a `#license #setup` TODO entry.
- **`proprietary`** — write a one-paragraph stub: "This project is proprietary. All rights reserved by {AUTHOR_NAME}, {YEAR}."
- **`unlicensed`** — write a one-paragraph stub explaining the project is unlicensed and asking contributors to contact the author before reuse.

The LICENSE file is committed alongside the docs in the Phase 2 commit. Do not skip it: a project with `License: MIT` in its README and no LICENSE file is broken on day one.

### `.gitignore`

Phase 2 also writes a language-aware `.gitignore`. Without one, the first commit either drags caches in or relies on the user spotting them.

**Always include:**

```
# editor / OS
.DS_Store
*.swp
.idea/
.vscode/

# environment files
.env
.env.local

# claude worktrees
.claude/worktrees/
```

**TypeScript / Node — append:**

```
node_modules/
dist/
build/
.vite/
.turbo/
coverage/
test-results/
playwright-report/
*.log
```

**Python — append:**

```
.venv/
__pycache__/
*.py[cod]
*.egg-info/
.pytest_cache/
.mypy_cache/
.ruff_cache/
.coverage
htmlcov/
dist/
build/
```

**Rust — append:**

```
target/
Cargo.lock  # (delete this line if the project is a binary, not a library)
```

**Go — append:**

```
bin/
*.test
*.out
```

If a `.gitignore` already exists in modernize mode, append any missing entries (idempotent — don't add lines that are already there) instead of writing a `.proposed` sibling. This is the one case where merging into an existing file is preferable to a sibling — `.gitignore` is additive by design.

### Commit

After Phase 2 writes its files, commit:

```sh
git add {files written, including LICENSE and .gitignore}
git commit -m "chore: lay down living-document scaffolding"
```

(In modernize mode, only commit the files actually written or `.proposed` siblings created.)

---

## Phase 3 — MAKEFILE

Install the Makefile gateway. In modernize mode, if a `Makefile` is `present` write `Makefile.proposed`; if `partial` write `Makefile.proposed`; if `missing` write `Makefile`.

The Makefile has a common header (help target, default goal) plus a language-specific body bound in Phase 4. Write the common header here; Phase 4 fills in the body.

### Common header (always)

**Critical — the awk line uses doubled dollar signs.** The two characters in the printf are: `$` `$` `1` (four chars total: two literal dollar signs followed by the digit one), and `$` `$` `2` (same idea). In the rendered Makefile this looks like `printf "...", $$1, $$2`. Make consumes a single `$` as the start of a variable expansion, so the source must have *two* dollars per occurrence to pass `$1` and `$2` through to awk at runtime.

Two known failure modes when writing this Makefile:

1. **Markdown rendering can collapse `$$...$$` as LaTeX math.** If you read the template and see `$, $` (the digits eaten), you're reading a math-flavoured render. The intended characters are `$$1` and `$$2`.
2. **Auto-correction.** Don't "fix" `$$1` to `$1` — that breaks `make help`.

After Phase 3 writes the Makefile, run **two separate** verification commands (do not chain with `&&` — the second one is a "must not match" check whose nonzero exit is the success path, and chaining trips up `set -e`):

Step 1 — must succeed (grep finds doubled dollars):

```sh
grep -E '\$\$[12]' Makefile
```

Step 2 — must "fail" (no match — meaning no single-`$` digit-references slipped through):

```sh
grep -E '[^$]\$[12]' Makefile
```

If step 1 prints nothing, the doubled dollars are missing and `make help` will produce garbage; rewrite. If step 2 prints any line inside the awk range, a single-dollar slipped through; rewrite. Run both before committing Phase 3.

In Phase 3 we declare only `.PHONY: help`. The full `.PHONY` list is added in Phase 4 alongside the actual target rules — declaring phony targets without rules in Phase 3 leaves the Makefile in a transient broken state where `make install` between commits 3 and 4 fails with "No rule to make target", which violates "always green on main."

```makefile
## {PROJECT_NAME} canonical commands.
##
## Stable named entrypoints for the common workflows. The underlying
## tool may change; the names here do not. See PROCESS.md "Canonical
## commands".
##
## Run `make` (no target) for the list.

.PHONY: help

help: ## Show this help
	@grep -E '^[a-zA-Z][a-zA-Z0-9_-]*:.*?##' $(MAKEFILE_LIST) \
		| awk -F ':.*?##' '{printf "  \033[1m%-12s\033[0m %s\n", $$1, $$2}'

# Body — language-specific targets — added in Phase 4.

# Project-specific targets below this line.

.DEFAULT_GOAL := help
```

The Phase 4 body is inserted between `# Body — ...` and `# Project-specific targets below this line.`, and Phase 4 extends the `.PHONY` declaration to include `install dev build check format lint typecheck test e2e clean`.

### Commit

```sh
git add Makefile  # or Makefile.proposed
git commit -m "chore: add Makefile gateway"
```

At this point `make` (no target) prints the help with just the `help` entry. `make help` works. Other targets are not yet defined; that's intentional and arrives in Phase 4.

---

## Phase 4 — TOOLCHAIN

Wire the Makefile body to the detected language's tools, install dev dependencies, run `make check` once. **If `make check` fails, stop and report — do not proceed to hooks (Phase 5).**

For each language, the Makefile body and supporting config files follow.

### TypeScript / Node — pnpm

Makefile body (extends `.PHONY` from Phase 3):

```makefile
.PHONY: help install dev build check format lint typecheck test e2e clean

install: ## Install dependencies
	pnpm install

dev: ## Start the dev server
	pnpm dev

build: ## Production build
	pnpm build

check: ## Format-check + lint + typecheck + tests
	pnpm exec prettier --check . && pnpm exec eslint . && pnpm exec tsc --noEmit && pnpm test

format: ## Auto-format the codebase
	pnpm exec prettier --write .

lint: ## Lint the codebase
	pnpm exec eslint .

typecheck: ## TypeScript --noEmit pass
	pnpm exec tsc --noEmit

test: ## Unit tests
	pnpm test

e2e: ## End-to-end tests
	{E2E_TARGET}

clean: ## Remove build output
	rm -rf dist build .vite
```

Where `{E2E_TARGET}` is `pnpm exec playwright test` if the user said yes to e2e in Batch 1, otherwise `@echo "no e2e tests configured"`. If yes: also add `@playwright/test` to devDependencies, run `pnpm exec playwright install chromium`, write a stub `playwright.config.ts` and `e2e/smoke.spec.ts`, and add a `#e2e #setup` TODO entry.

A fresh TypeScript project also needs at least one passing test for `make check` to succeed. Write `src/index.ts` with a trivial export and `test/smoke.test.ts` with a single passing assertion. Without these, vitest exits with code 1 ("no test files found") and `make check` fails before Phase 5 can install hooks.

`package.json` additions (merge into existing):

```json
{
  "scripts": {
    "test": "vitest run",
    "dev": "vite",
    "build": "vite build"
  },
  "devDependencies": {
    "prettier": "^3",
    "eslint": "^9",
    "typescript-eslint": "^8",
    "@eslint/js": "^9",
    "eslint-config-prettier": "^9",
    "typescript": "^5",
    "vitest": "^2",
    "husky": "^9",
    "lint-staged": "^15"
  }
}
```

`tsconfig.json` (write only if missing):

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "strict": true,
    "noImplicitOverride": true,
    "noUncheckedIndexedAccess": true,
    "noPropertyAccessFromIndexSignature": true,
    "noFallthroughCasesInSwitch": true,
    "noImplicitReturns": true,
    "exactOptionalPropertyTypes": true,
    "useUnknownInCatchVariables": true,
    "verbatimModuleSyntax": true,
    "esModuleInterop": true,
    "skipLibCheck": true
  },
  "include": ["src/**/*", "test/**/*"],
  "exclude": ["node_modules", "dist", "build"]
}
```

`eslint.config.js` (flat config, write only if missing):

```js
import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import prettier from 'eslint-config-prettier';

export default tseslint.config(
  js.configs.recommended,
  ...tseslint.configs.recommended,
  prettier,
  {
    rules: {
      'no-console': ['warn', { allow: ['warn', 'error'] }],
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
    },
  },
  {
    files: ['**/*.test.ts', '**/*.spec.ts', 'test/**/*.ts'],
    rules: {
      'no-console': 'off',
    },
  },
);
```

`.prettierrc.json`:

```json
{
  "printWidth": 100,
  "singleQuote": true,
  "trailingComma": "all",
  "semi": true,
  "arrowParens": "always",
  "endOfLine": "lf"
}
```

### TypeScript / Node — npm

Same as pnpm but replace `pnpm exec` with `npx` and `pnpm install` with `npm install`, `pnpm test` with `npm test`, etc. Drop husky+lint-staged or keep depending on user preference.

### Python — uv

Phase 4 for Python is more involved than for TypeScript because Python's packaging and import paths need more scaffolding to make `make check` pass on a fresh project. The skill must produce: (1) a `pyproject.toml` with both `[project]` and `[build-system]` tables; (2) a `src/` layout with `src/{PACKAGE_NAME}/__init__.py`; (3) at least one passing test so pytest doesn't exit with code 5 ("no tests collected"); (4) mypy `mypy_path` and `explicit_package_bases` set, so strict mypy can resolve the src-layout package and its tests. Without all four, `make check` fails on first run and Phase 5 (hooks) cannot proceed.

Derive `{PACKAGE_NAME}` from `{PROJECT_NAME}` by replacing hyphens with underscores (Python identifier rules).

Makefile body (extends `.PHONY` from Phase 3):

```makefile
.PHONY: help install dev build check format lint typecheck test e2e clean

install: ## Install dependencies
	uv sync --all-extras --dev

dev: ## Drop into a project-aware REPL
	uv run python

build: ## Build distributable
	uv build

check: ## Format-check + lint + typecheck + tests
	uv run ruff format --check . && uv run ruff check . && uv run mypy . && uv run pytest

format: ## Auto-format
	uv run ruff format .

lint: ## Lint
	uv run ruff check .

typecheck: ## mypy
	uv run mypy .

test: ## Unit tests
	uv run pytest

e2e: ## End-to-end tests
	{E2E_TARGET}

clean: ## Remove build output
	rm -rf dist build .pytest_cache .mypy_cache .ruff_cache
```

Where `{E2E_TARGET}` is `uv run pytest tests/e2e` if the user said yes to e2e in Batch 1, otherwise `@echo "no e2e tests configured"`.

`pyproject.toml` (write the full file if missing; merge tables if present):

```toml
[project]
name = "{PROJECT_NAME}"
version = "0.0.0"
description = "{PROJECT_TAGLINE}"
requires-python = ">=3.11"
authors = [{ name = "{AUTHOR_NAME}" }]
license = { text = "{LICENSE}" }
dependencies = []

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[tool.hatch.build.targets.wheel]
packages = ["src/{PACKAGE_NAME}"]

[tool.ruff]
line-length = 100
target-version = "py311"

[tool.ruff.lint]
select = ["E", "F", "I", "N", "UP", "B", "C4", "SIM"]

[tool.mypy]
strict = true
python_version = "3.11"
mypy_path = "src"
explicit_package_bases = true

[tool.pytest.ini_options]
addopts = "-ra"
testpaths = ["tests"]

[dependency-groups]
dev = ["ruff", "mypy", "pytest", "pre-commit"]
```

Files to create alongside `pyproject.toml`:

- `src/{PACKAGE_NAME}/__init__.py` — content:
  ```python
  """{PROJECT_TAGLINE}"""

  __version__ = "0.0.0"
  ```
- `tests/__init__.py` — empty file.
- `tests/test_smoke.py` — content:
  ```python
  from {PACKAGE_NAME} import __version__


  def test_version_is_set() -> None:
      assert __version__ == "0.0.0"
  ```

If the user said yes to e2e: also create `tests/e2e/__init__.py` (empty) and `tests/e2e/test_e2e_smoke.py` with a single `def test_placeholder() -> None: pass`. Add a `#e2e #setup` TODO entry: "replace e2e placeholder with real end-to-end coverage".

### Python — pip

Same as uv but: replace `uv run` with direct invocation (assume the user is in an active virtualenv), replace `uv sync --all-extras --dev` with `pip install -e ".[dev]"`, replace `uv build` with `python -m build`. The `pyproject.toml` `[project]`, `[build-system]`, `[tool.*]` tables and the `src/` layout are identical. Move `[dependency-groups]` to a `[project.optional-dependencies]` `dev = [...]` table.

### Rust — cargo

Makefile body:

```makefile
install: ## Fetch dependencies
	cargo fetch

dev: ## Run the project in watch mode
	cargo watch -x run  # requires cargo-watch; adjust if absent

build: ## Production build
	cargo build --release

check: ## Format-check + lint + tests
	cargo fmt --all -- --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test

format: ## Auto-format
	cargo fmt --all

lint: ## Clippy
	cargo clippy --all-targets --all-features -- -D warnings

typecheck: ## Covered by build
	cargo check --all-targets

test: ## Unit + integration tests
	cargo test

e2e: ## End-to-end tests
	@echo "no e2e tests configured"

clean: ## Remove build output
	cargo clean
```

`Cargo.toml` additions (merge `[lints]`):

```toml
[lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }

[lints.rust]
unsafe_code = "forbid"
```

### Go — go modules

Makefile body:

```makefile
install: ## Fetch dependencies
	go mod download

dev: ## Run the project
	go run ./...

build: ## Production build
	go build -o bin/{PROJECT_NAME} ./...

check: ## Format-check + lint + tests
	test -z "$$(gofmt -l .)" && go vet ./... && golangci-lint run && go test ./...

format: ## Auto-format
	gofmt -w . && goimports -w .

lint: ## golangci-lint
	golangci-lint run

typecheck: ## Covered by build
	go vet ./...

test: ## Unit tests
	go test ./...

e2e: ## End-to-end tests
	@echo "no e2e tests configured"

clean: ## Remove build output
	rm -rf bin
```

`.golangci.yml`:

```yaml
linters:
  enable:
    - errcheck
    - gosimple
    - govet
    - ineffassign
    - staticcheck
    - unused
    - gofmt
    - goimports
    - revive

issues:
  max-issues-per-linter: 0
  max-same-issues: 0
```

### Unknown language

Makefile body — every target is a stub the user must fill in:

```makefile
install: ## Install dependencies
	@echo "TODO: implement install for this project's toolchain"
	@false

dev: ## Start the dev environment
	@echo "TODO: implement dev"
	@false

build: ## Production build
	@echo "TODO: implement build"
	@false

check: ## Composite safety gate (format-check + lint + typecheck + test)
	@echo "TODO: wire check to format-check + lint + typecheck + test"
	@false

format: ## Auto-format
	@echo "TODO: implement format"
	@false

lint: ## Lint
	@echo "TODO: implement lint"
	@false

typecheck: ## Type checking (skip if not applicable)
	@echo "TODO: implement typecheck or remove this target"
	@false

test: ## Unit tests
	@echo "TODO: implement test"
	@false

e2e: ## End-to-end tests
	@echo "no e2e tests configured"

clean: ## Remove build output
	@echo "TODO: implement clean"
	@false
```

Add a `TODO.md` entry: `wire Makefile targets to project toolchain #setup #area-tooling`.

### Run `make check`

After the body is wired and `make install` has run, execute `make check`. If it succeeds, proceed to Phase 5. If it fails, stop, report the failure, and instruct the user to fix it before re-running the skill (Phase 5 onwards).

### Commit

```sh
git add Makefile {language config files} {lockfile updates}
git commit -m "chore: wire {LANGUAGE} toolchain to make targets"
```

---

## Phase 5 — HOOKS

Install pre-commit hooks bound to the fast subset of `make check`. Skip if `.git/hooks/pre-commit` is `present` and managed by a known framework.

If `.git/hooks/pre-commit` exists but isn't managed by a recognized framework: write `.git/hooks/pre-commit.proposed` and instruct the user in the handoff to merge by hand.

### TypeScript — husky + lint-staged

Add to `package.json`:

```json
{
  "scripts": {
    "prepare": "husky"
  },
  "lint-staged": {
    "*.{ts,tsx,js,mjs,cjs,json,md,yml,yaml}": "prettier --write",
    "*.{ts,tsx,js,mjs,cjs}": "eslint --fix"
  }
}
```

Run `pnpm exec husky init` (or `npx husky init`). Then write `.husky/pre-commit`:

```sh
pnpm exec lint-staged && pnpm exec tsc --noEmit && pnpm test
```

(Use `npx` for npm.)

### Python — pre-commit framework

Write `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/astral-sh/ruff-pre-commit
    rev: v0.6.0
    hooks:
      - id: ruff-format
      - id: ruff
        args: [--fix]
  - repo: local
    hooks:
      - id: mypy
        name: mypy
        entry: uv run mypy .
        language: system
        types: [python]
        pass_filenames: false
        always_run: true
      - id: pytest
        name: pytest
        entry: uv run pytest
        language: system
        pass_filenames: false
        always_run: true
```

For pip projects, replace `uv run mypy .` / `uv run pytest` with `mypy .` / `pytest` (assumes the user is in an active virtualenv).

**Critical:** the mypy `entry` must end with `.` (or some path target). With `pass_filenames: false` and `always_run: true`, the hook never gets file arguments injected — so without an explicit target, mypy crashes with "Missing target module, package, files, or command." The dot tells mypy to check the whole project.

The mypy hook uses `language: system` rather than the `mirrors-mypy` repo because mirrors-mypy with `additional_dependencies: []` cannot see the project's installed packages — strict mypy on test files needs to import pytest, the package being tested, etc., and only the local environment has them. `language: system` runs mypy from the project's environment and gets all dependencies for free.

Run `uv run pre-commit install` (or `pre-commit install`).

### Rust — shell hook

Write `.git/hooks/pre-commit` directly:

```sh
#!/usr/bin/env sh
set -e
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

`chmod +x .git/hooks/pre-commit`.

For team projects, prefer the `pre-commit` framework with a `local` hook calling the same commands so the hook is reproducible across clones.

### Go — shell hook

Write `.git/hooks/pre-commit`:

```sh
#!/usr/bin/env sh
set -e
test -z "$(gofmt -l .)" || { echo "gofmt failed"; gofmt -l .; exit 1; }
go vet ./...
golangci-lint run
go test ./...
```

`chmod +x .git/hooks/pre-commit`.

### Commit

```sh
git add {hook config files}
git commit -m "chore: install pre-commit hooks"
```

---

## Phase 6 — CI

Add `.github/workflows/check.yml`. If a workflow already runs `make check`, mark CI `present` and skip.

### TypeScript

```yaml
name: check

on:
  push:
    branches: [main]
  pull_request:

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4  # remove for npm
        with:
          version: 9
      - uses: actions/setup-node@v4
        with:
          node-version: 22
          cache: pnpm  # 'npm' for npm
      - run: make install
      - run: make check
      - run: make build
```

### Python

```yaml
name: check

on:
  push:
    branches: [main]
  pull_request:

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: astral-sh/setup-uv@v3  # or: actions/setup-python@v5 for pip
      - run: make install
      - run: make check
      - run: make build
```

### Rust

```yaml
name: check

on:
  push:
    branches: [main]
  pull_request:

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: make install
      - run: make check
      - run: make build
```

### Go

```yaml
name: check

on:
  push:
    branches: [main]
  pull_request:

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-go@v5
        with:
          go-version: stable
      - uses: golangci/golangci-lint-action@v6
      - run: make install
      - run: make check
      - run: make build
```

### Commit

```sh
git add .github/workflows/check.yml
git commit -m "chore: add CI check workflow"
```

---

## Phase 7 — REVIEW SKILL

Generate a stub Layer 2 review skill at `.claude/skills/{PROJECT_NAME}-review/SKILL.md` inside the target repo. Add a TODO entry reminding the user to flesh it out as project-specific rules emerge.

### Stub `SKILL.md`

```markdown
---
name: {PROJECT_NAME}-review
description: Project-aware pre-commit review for the {PROJECT_NAME} repository. Reads SPEC.md, ARCHITECTURE.md, and PROCESS.md, then inspects the staged changes and reports findings the lint layer cannot catch — architectural-contract violations, spec/implementation drift, missing living-document updates, naming and abstraction concerns, and PROCESS.md principles the diff has slipped past. Reports only; does not auto-apply fixes. Use at the end of a meaningful chunk of work before commit.
---

# {PROJECT_NAME}-review

Layer 2 review: project-aware. Catches what lint cannot. Reports findings; does not auto-fix.

## 1. Read the canonical brief

Before looking at any diff, read in full:

- `SPEC.md` — what the project is.
- `ARCHITECTURE.md` — how it is built. Pay particular attention to architectural disciplines that must be enforced.
- `PROCESS.md` — how we work. Note the living-documents rule, the engineering disciplines, and the layered review.

## 2. Identify the diff

Run `git diff --staged`. If staging is empty, fall back to `git diff` against the working tree.

Read every hunk. Note the files touched and the surfaces affected (core logic, public interfaces, UI, configuration, docs).

## 3. Walk the review categories

For each category, examine the diff and list any concerns.

<!--
  Add project-specific categories here as rules emerge. Each category should be a heading
  with a short description of what to look for. Examples to start from:
-->

### A. Architectural-contract violations

Look for diffs that violate rules from `ARCHITECTURE.md`. (Fill in specific rules as the architecture stabilises.)

### B. Spec / implementation drift

Does the change match the terminology, scope, and out-of-scope notes in `SPEC.md`?

### C. Living-document updates

Per `PROCESS.md`, a commit that alters user-observable behaviour or completes a tracked TODO must update the relevant document. Check:

- Did user-observable behaviour change? → `README.md` and `FEATURES.md` updated?
- Did a TODO complete? → entry deleted from `TODO.md`?
- Did a release acceptance criterion ship? → `ACCEPTANCE.md` updated?

### D. Tests and agent-testability

New behaviour ships with tests. Bug fixes ship with regression tests. Tests are end-to-end runnable without manual interaction.

### E. Naming and abstraction

Does the change use the project's vocabulary (from `SPEC.md` / `ARCHITECTURE.md`)? Are abstractions premature?

### F. Process disciplines

Is the diff a clean rebase (no merge commits)? Is formatting separated from feature commits? Will `make check` pass?

<!--
  HOW TO FLESH THIS OUT:

  When ARCHITECTURE.md grows real architectural disciplines (rules like "no Math.random in
  the simulation core", "no host APIs imported from the sim layer", "all timestamps are
  integer ticks"), revisit this file. For each discipline:

  1. Add a new heading under §3 with the discipline's name.
  2. Write one-line description of what the diff should be checked for.
  3. List the specific code patterns or imports that constitute a violation.

  Until the architecture has at least three real disciplines, keep using the generic
  categories (A-F) above. The skill is reports-only at every stage; no auto-fixing.
-->

<!-- Add further project-specific categories as patterns emerge. -->

## 4. Write the report

Group findings into three buckets:

- **Blockers** — violations of an explicit rule. Must be fixed before commit.
- **Concerns** — judgment calls worth flagging. The user decides.
- **Notes** — observations that don't warrant action but are worth recording.

Format the verdict at the end:

> Blockers: N · Concerns: M · Notes: K · {commit / fix-then-commit / do-not-commit}

Do not auto-apply fixes. The skill is reports-only; the user (or the assistant in a separate turn) does the fixing.
```

### TODO.md addition

Append to `TODO.md` under the backlog:

```markdown
- flesh out `{PROJECT_NAME}-review` Layer 2 categories as project-specific rules emerge in `ARCHITECTURE.md` `#review #setup`
```

### Commit

```sh
git add .claude/skills/{PROJECT_NAME}-review/ TODO.md
git commit -m "chore: scaffold {PROJECT_NAME}-review skill"
```

---

## Phase 8 — HANDOFF

Print a summary. Do not commit anything in this phase.

```
contractify summary
===================

Mode: {initialize|modernize}
Language: {LANGUAGE} ({package manager})

Phases run:
  ✓ Phase 2 — DOCS         {N files written, M skipped, K .proposed}
  ✓ Phase 3 — MAKEFILE     {written|.proposed|skipped}
  ✓ Phase 4 — TOOLCHAIN    {make check passed/failed}
  ✓ Phase 5 — HOOKS        {framework}
  ✓ Phase 6 — CI           {workflow added|skipped}
  ✓ Phase 7 — REVIEW SKILL {.claude/skills/{PROJECT_NAME}-review/SKILL.md created}

Commits added: {list of subject lines}

Action required:
  - {list of .proposed files awaiting human merge, with diff command}
  - {any phase that stopped due to make check failure}
  - {any TODOs added for the user}

Next steps:
  1. Fill in SPEC.md, ARCHITECTURE.md, README.md with project content.
  2. Add patterns to CLAUDE.md as feedback surfaces them.
  3. As architectural rules stabilise, expand the {PROJECT_NAME}-review skill's
     review categories.
  4. Try a commit. The pre-commit hook should run make check (fast subset).
```

If any `.proposed` files were written, list them with the suggested merge command:

```sh
diff -u PROCESS.md PROCESS.md.proposed | less
# review, merge by hand, then:
rm PROCESS.md.proposed
```

If `make check` failed in Phase 4, do **not** print "Phase 5 — HOOKS" as run; instead surface the failure and instruct the user to fix and re-run from Phase 4.

---

## Idempotency and safety rules

These rules are checked throughout, not just in Phase 0:

- **Refuse on dirty working tree.** Phase 0 only; if the tree becomes dirty mid-run (e.g. user edited a file), abort.
- **Refuse on detached HEAD or non-default branch** unless confirmed in Phase 0.
- **Never overwrite an existing file.** Existing → `.proposed` sibling, then continue.
- **Never edit existing CI workflows.** Add a new file or skip.
- **Never use `--no-verify`** when committing during this skill's run. The hooks must pass; if they don't, that's a bug to surface.
- **Re-runnable.** Running on a fully-modern repo: Phase 1 audit returns all `present`, no commits land, Phase 8 reports "nothing to do."
- **No `Co-Authored-By` trailer** on any commit this skill creates.

---

## Notes for the implementer running this skill

- Use AskUserQuestion sparingly. Batch related questions. Do not ask about defaults the user has already implicitly chosen by running `/contractify` in the first place.
- When writing template files, substitute every placeholder from the table above. After Phase 2 commits, run `grep -r '{[A-Z_]*}' .` — the result must be empty. Any literal `{...}` in a written file is a bug.
- Never leave visible "(To be filled in.)" prose in a template. Sections that can't be answered up-front get an HTML comment instead, so the rendered markdown shows clean section headings.
- After each per-phase commit, run `git status` to confirm the working tree is clean before proceeding. If something is dirty (e.g. a tool wrote a cache file), `.gitignore` it before continuing.
- The TypeScript-pnpm path is the most exercised; the others (npm, uv, pip, cargo, go) are based on common conventions but should be reviewed against the user's actual project structure before committing the toolchain phase.

