---
name: agent-mux-review
description: Project-aware pre-commit review for the agent-mux repository. Reads SPEC.md, ARCHITECTURE.md, and PROCESS.md, then inspects the staged changes and reports findings the lint layer cannot catch — architectural-contract violations, spec/implementation drift, missing living-document updates, naming and abstraction concerns, and PROCESS.md principles the diff has slipped past. Reports only; does not auto-apply fixes. Use at the end of a meaningful chunk of work before commit.
---

# agent-mux-review

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

Is the diff a clean rebase (no merge commits)? Is formatting separated from feature commits? Will `cargo fmt --check && cargo clippy -- -D warnings && cargo test` pass?

<!--
  HOW TO FLESH THIS OUT:

  When ARCHITECTURE.md grows real architectural disciplines (rules like "no host APIs imported
  from the session layer", "all session state mutations go through the supervisor", "remote
  transport details never leak into the UI layer"), revisit this file. For each discipline:

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
