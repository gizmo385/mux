# Plan: supporting other agent CLIs (Codex, Pi, …)

Status: **executed 2026-07-10** — WP0–WP9 complete on the `multi-agent-cli` branch (accepted 2026-07-09). Live-binary verification of Codex + Pi is deferred (no `codex`/`pi` on the build box; all Codex/Pi paths are synthetic-fixture-tested — see TODO.md "Other agent CLIs"). Per-WP commits:

- WP0 (spec amendment + scope decision) — `8346fd7`
- WP1 (`AgentCli` trait + Claude extraction, pure refactor) — `e7d3969`
- WP2 (`[agents]` config + multi-root discovery/watch plumbing) — `47c2572`
- WP3 (Codex read path — discovery + rollout parser) — `dac4cc9`
- WP4 (Pi read path — discovery + session parser) — `4370b23`
- WP5 (Codex spawn via post-launch adoption + resume) — `2d7f51b`
- WP6 (Pi spawn/resume via pinned session id) — `f190f6c`
- WP7 (UI: agent tag on rows + new-session agent picker) — `9ed137d`
- WP8 (Codex hook parity — `PermissionRequest`/`Stop` ingest + installer) — `97d0141`
- WP9 (docs/ledger close-out) — this commit.

Date: 2026-07-09.
Origin: user request to investigate what supporting non-Claude agent CLIs would look like, with OpenAI Codex CLI and Pi as the concrete first targets. Backed by a codebase coupling survey and per-agent research briefs (Appendices A–C). Codified in `TODO.md` under "Other agent CLIs".

This document is written to be executed by a **series of subagents** (Opus-class), one work package each. Every work package is self-contained: it names its files, its acceptance criteria, and the appendix facts it depends on. An executing agent should read this document, `SPEC.md`, `ARCHITECTURE.md`, and `PROCESS.md` before touching code.

---

## 1. Decision being made

`SPEC.md` currently scopes other agents **out** ("Other agents — Cursor, Aider, generic-LLM CLIs — none of these are in scope. Claude Code only."). This plan proposes amending that: agent-mux becomes a multiplexer for *terminal agent CLIs that persist transcripts to disk*, with Claude Code as the reference implementation and Codex + Pi as the first additional targets.

The shape of the product does not change. agent-mux remains a control plane over tmux plus on-disk transcripts; it still never renders conversations itself, never configures the agent CLI, and still derives attention from transcript content. What changes is that "the transcript" and "the `claude` binary" become *per-agent* concepts behind a trait, exactly the way "the machine it runs on" became a per-host concept behind `Host` and "how attach works" became per-driver behind `AttachmentDriver`.

**Feasibility verdict from research (short version):**

- **Pi** is nearly isomorphic to Claude Code: JSONL sessions in a per-cwd directory, a documented + versioned session format, an assistant `stopReason` field with the same semantics as Claude's `stop_reason`, and — critically — a `pi --session-id <id>` flag that both creates and reopens a session by caller-chosen id. Every existing agent-mux mechanism (identity pinning, tmux naming, resume fallback, attention tailing) maps 1:1. **Low risk.**
- **Codex** is the hard case, in three specific ways: (1) **no session-id pinning** — upstream explicitly declined it, so spawn must discover the id *after* launch by correlating the new rollout file; (2) **approval prompts are not written to the rollout** — a session blocked on an approval looks `working` from JSONL alone; parity for "blocked" needs Codex's lifecycle-hooks mechanism (`PermissionRequest`), which is a Phase-4 concern; (3) **schema churn** — Codex releases ~daily and the rollout format has already broken compatibly several times; the plan mitigates with defensive parsing keyed on the most stable record types (`turn_started`/`turn_complete`/`turn_aborted`, persisted in both history modes). **Medium risk, contained.**
- **Everything else in agent-mux is already agent-agnostic**: hosts, catalog, notifier, embedded PTY, quickswitcher, favorites, worktrees, repo registry. The coupling survey (Appendix C) found the Claude specifics concentrated in five areas — directory shape, JSONL parsing, spawn/resume argv, the id-identity contract, and hooks — all of which sit in `discovery.rs`, `watcher.rs`, `attachment.rs`, `hook_ingest.rs`/`hook_install.rs`, and a handful of constants.

## 2. Design overview

### 2.1 The new seam: `AgentCli`

A new trait, sibling to `Host` and `AttachmentDriver`, in a new `src/agent.rs` + `src/agents/{claude,codex,pi}.rs` module family. Unit-struct impls, registered in a static registry keyed by a small `AgentKind` enum:

```rust
/// Copy-able discriminator carried on Session, events, cache entries.
pub enum AgentKind { Claude, Codex, Pi }

pub trait AgentCli: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn label(&self) -> &'static str;                       // "claude" — config key + UI string
    fn default_binary(&self) -> &'static str;              // overridable via [agents.<label>] binary
    fn default_transcript_root(&self) -> PathBuf;          // ~/.claude/projects | ~/.codex/sessions | ~/.pi/agent/sessions

    /// Shape of the on-disk transcript tree, executed by Host (local read_dir / remote find).
    /// Keeps ssh mechanics in Host, layout knowledge in the agent.
    fn listing(&self) -> ListingSpec;                      // { mindepth, maxdepth, name_glob }
    /// Watch-path filter: is this path a top-level transcript for this agent's tree?
    fn is_transcript(&self, path: &Path, root: &Path) -> bool;
    /// SessionId from a transcript path (stem for Claude; trailing uuid for Codex; after '_' for Pi).
    fn session_id_from_path(&self, path: &Path) -> Option<SessionId>;

    /// Head-of-file parse: cwd, title, first user message.
    fn parse_meta(&self, head: &str) -> TranscriptMeta;
    /// Tail-of-file parse: attention + edited files (one read, like today's AttentionDerivation).
    fn derive(&self, tail: &str, cwd: &Path) -> AgentDerivation;

    /// How a new session is created. Two strategies exist in the wild.
    fn spawn(&self, cwd: &Path, minted_id: &SessionId) -> SpawnPlan;
    /// Command string to resume an existing session by id (used by the tmux resume fallback).
    fn resume_command(&self, id: &SessionId) -> String;
}

pub enum SpawnPlan {
    /// Agent accepts a caller-chosen id (claude --session-id, pi --session-id):
    /// the minted uuid is simultaneously the tmux session name suffix, the
    /// transcript stem/suffix, and the SessionId — today's identity contract, unchanged.
    PinnedId { argv: Vec<String> },
    /// Agent refuses id pinning (codex): spawn with a provisional tmux name,
    /// then adopt the id from the transcript that appears (see §2.4).
    DiscoverAfterSpawn { argv: Vec<String> },
}
```

Registry: `fn agent(kind: AgentKind) -> &'static dyn AgentCli`. `AgentKind` (not `Arc<dyn AgentCli>`) travels through channels, `Session`, and the disk cache; behavior is looked up at the point of use. The set of agents is closed per release — adding one is a code change regardless (a parser is code) — so an enum costs nothing in extensibility and buys exhaustive `match` checking everywhere the kinds diverge.

### 2.2 New architectural discipline

To be added to `ARCHITECTURE.md`'s Disciplines section verbatim (it is the review contract for every work package):

> **Agent-CLI specifics live behind the `AgentCli` trait.** No agent binary names, transcript-path shapes, JSONL field names, or spawn/resume flags appear in the Dashboard, Session Catalog, Transcript Watcher, Host Abstraction, or Attachment Driver. The watcher parses transcripts *through* the session's agent; the drivers build spawn/resume argv *through* it. Reason: the same property that let `PtyDriver` slot in behind `AttachmentDriver` — and remote hosts behind `Host` — is what lets a fourth agent CLI land as one new module instead of a codebase sweep.

### 2.3 What each existing component gains

- **`Session`** (`session.rs`): a new `agent: AgentKind` field. Everything downstream (resume fallback, parser choice, watcher routing) reads it.
- **Config** (`config.rs`): a new optional `[agents.<label>]` table — `enabled` (claude defaults `true`, others `false`), `binary` (PATH override), `transcript_root` (override). Per-host: `[hosts.<name>.agents.<label>]` with `transcript_root`. The existing per-host `transcript_root` key stays as a backward-compatible alias for the claude entry. **Zero-config behavior is byte-identical to today: claude only.**
- **Discovery** (`discovery.rs`): iterates *(host × enabled agents)* instead of *(host)*. Each (host, agent, root) triple lists via the agent's `ListingSpec`, parses via the agent's `parse_meta`/`derive`. The `fallback_dir` bucket-name decoding becomes claude-parser-internal.
- **Watcher** (`watcher.rs`): the single local `notify` watcher watches *all enabled agents' roots* (one `RecommendedWatcher`, multiple watched paths — the "one filesystem watcher process" discipline holds). Events route path → agent by root prefix; `is_top_level_transcript` generalizes to `agent.is_transcript(path, root)`. Remote pollers tick each enabled agent root per host over the same ControlMaster.
- **Attachment** (`attachment.rs`): `spawn_session` consumes `SpawnPlan`; resume fallback builds its command via `agent(session.agent).resume_command(id)`. The `agent-mux-<id>` tmux naming convention is unchanged and stays agent-neutral (ids are unique across agents — uuids for claude/codex, uuid-or-slug for pi).
- **Cache** (`cache.rs`): `CachedSession` gains `agent` with a serde default of `"claude"` so existing snapshots load unchanged.
- **Hook ingest/install** (`hook_ingest.rs`, `hook_install.rs`): Phase 4. The marker-file pipeline is already agent-neutral; what's Claude-specific is the *producer* side (settings.json schema, `notification_type` vocabulary). Codex gets a parallel producer via `hooks.json` (`PermissionRequest` → blocking, `Stop` → turn complete); Pi via a drop-in extension. Each producer normalizes into the existing marker format so the consumer (`poll_hooks_once`, catalog pins) is untouched.
- **UI**: an agent tag on the session row's status line (only rendered when ≥2 agents are enabled — claude-only users see zero change), and an agent selector step in the `n`/`N` new-session flow (same rule: skipped entirely when only one agent is enabled).

### 2.4 The Codex spawn-correlation protocol (the one genuinely new mechanism)

Codex cannot pin a session id, so the `PinnedId` identity contract (uuid == tmux name == transcript stem == SessionId) breaks for it. The `DiscoverAfterSpawn` protocol replaces it:

1. Spawn `tmux new-session -d -s agent-mux-pending-<nonce> -c <cwd> codex` (nonce = minted uuid; **never** `--ephemeral`, which suppresses the rollout).
2. The watcher, which is already watching `~/.codex/sessions/` recursively, sees the new `rollout-<ts>-<uuid>.jsonl` appear. Discovery reads line 1 (`session_meta`, carries `cwd`) — when `cwd` matches a pending spawn recorded in the last `ADOPTION_WINDOW` (~30 s), the pending entry is *adopted*: SessionId = the rollout uuid.
3. On adoption, run `tmux rename-session -t agent-mux-pending-<nonce> agent-mux-<uuid>` — restoring the durable name→id link every downstream mechanism (pane resolution stage 1, resume fallback, pane-presence indicator) already relies on.
4. If no rollout appears in the window (codex crashed, wrong binary), the pending entry surfaces as a spawn error in the footer; the tmux session, if alive, is still reachable via the cwd-fallback pane matching.

Resume needs no correlation: `codex resume <id>` takes the id directly, and the id is recoverable from any rollout filename. Remote spawn uses the same protocol — the remote poller's `NewTranscript` event is the adoption trigger.

A hardening option (not required for v1, noted for Phase 4): Codex's `tui.terminal_title` config can publish `thread-id` into the pane title, readable via tmux `#{pane_title}` as a correlation cross-check.

### 2.5 Attention semantics per agent

| Signal | Claude (today) | Codex | Pi |
| --- | --- | --- | --- |
| Working | assistant `stop_reason == "tool_use"`; user/tool-result tail | open `turn_started` (no matching `turn_complete`) | assistant `stopReason == "toolUse"`; user/toolResult tail |
| Needs input | assistant, other/missing `stop_reason` | `turn_complete` / `turn_aborted` event | assistant `stopReason ∈ {stop, error, aborted}` |
| Blocked (prompt) | `Notification` hook (`permission_prompt`, `elicitation_dialog`) | **not in rollout** — `PermissionRequest` lifecycle hook (Phase 4); until then blocked ≈ working | pi has no built-in permission gates; extensions could add them (Phase 4, optional) |
| Idle | mtime > `IDLE_THRESHOLD` overlay | same (agent-neutral) | same |
| Edited files | `tool_use` blocks: Edit/Write/MultiEdit/NotebookEdit → `file_path`/`notebook_path` | `patch_apply_end` event `changes` map (legacy mode) + `item_completed`/`TurnItem::FileChange` (paginated mode) | `toolCall` blocks: `write`/`edit` → `path` (may be **relative** — resolve against header `cwd`) |
| Title | `ai-title` entry / first user message | no title record — first `user_message` event / `session_index.jsonl` name | `session_info` entry `name` / first user message |

Known, accepted gaps at v1 (documented in README when shipping): Codex "blocked on approval" reads as `working` until Phase 4; Codex shell-command file edits (not via `apply_patch`) aren't captured; Codex `.jsonl.zst` cold-compressed rollouts are skipped by discovery (they're ≥ weeks old — far outside `DISCOVERY_MAX_AGE`'s hot set; appends re-materialize them to plain `.jsonl`).

## 3. Work packages

Dependency graph — the spine is sequential; the per-agent tracks are parallel-safe *by construction* (each fills only its own pre-stubbed module + fixtures):

```
WP0 (docs/spec) ──► WP1 (trait + claude extraction) ──► WP2 (config + multi-root plumbing)
                                                            │
                                    ┌───────────────────────┼──────────────────────┐
                                    ▼                       ▼                      │
                          WP3 (codex read path)   WP4 (pi read path)               │
                                    │                       │                      │
                                    ▼                       ▼                      │
                          WP5 (codex spawn/resume) WP6 (pi spawn/resume)           │
                                    └───────────┬───────────┘                      │
                                                ▼                                  ▼
                                       WP7 (UI: agent tag + picker)   WP8 (hooks parity, Codex first)
                                                └───────────► WP9 (docs/ledger close-out)
```

Per `PROCESS.md`: worktree agents only for packages whose files don't overlap concurrent work; every commit passes fmt+clippy+tests; living documents (`FEATURES.md`, `TODO.md`, `README.md`) update in the same commit as behavior; Conventional Commits (`feat(agents): …`); integration by rebase, never merge. Each package ends by running the `agent-mux-review` skill before commit.

**Fixtures policy (applies to WP1, WP3, WP4):** each agent gets `tests/fixtures/<agent>/` with hand-authored transcripts exercising every parser branch (working/needs-input/aborted tails, edited-files variants, meta/title extraction, malformed lines). Fixtures are built from the appendix facts; **if the user has real `~/.codex/sessions` / `~/.pi/agent/sessions` trees on a machine, capturing a few sanitized real files is strictly better** — flag this to the user at WP3/WP4 kickoff rather than silently shipping synthetic-only fixtures.

---

### WP0 — Spec amendment + scope decision record

- **Size:** small (½ h). Main thread, not a worktree agent — it's the decision gate for everything after it.
- **Files:** `SPEC.md`, `TODO.md`, `ARCHITECTURE.md` (Open questions), `docs/plans/2026-07-09-multi-agent-cli.md` (status flip).
- **Work:** Rewrite SPEC's "Other agents" out-of-scope bullet into the new scope statement (multi-agent-CLI multiplexer; Claude Code is the reference agent; agents must persist tail-parseable transcripts; chat-UI rendering stays out of scope). Add "Agent CLI" to the Glossary. Add a SPEC Functionality line for multi-agent discovery/spawn. Record the decision + date in this plan's Status line. Note the new `AgentCli` discipline as *planned* in ARCHITECTURE (WP1 makes it real).
- **Acceptance:** docs read coherently; no code.

### WP1 — Extract the seam: `AgentCli` trait + Claude impl (pure refactor)

- **Size:** large (the heart of the migration; ~1–2 agent-days). Sequential — touches foundational interfaces; **not** parallel-safe with anything.
- **Files:** new `src/agent.rs`, new `src/agents/mod.rs`, new `src/agents/claude.rs`, **stub** `src/agents/codex.rs` + `src/agents/pi.rs` (compile-empty, so WP3/WP4 never touch shared files); edits to `lib.rs`, `session.rs`, `discovery.rs`, `watcher.rs`, `attachment.rs`, `host.rs`, `cache.rs`.
- **Work:**
  1. Define `AgentKind`, `AgentCli`, `SpawnPlan`, `ListingSpec`, `TranscriptMeta`, `AgentDerivation`, and the static registry (§2.1).
  2. Move into `agents/claude.rs`, unchanged in behavior: `claude_projects_dir` (→ `default_transcript_root`), the depth-2 `.jsonl` listing shape (from `LocalHost::list_transcripts` / `SshHost::list_transcripts` — Host keeps *executing* listings, now parameterized by `ListingSpec`), `is_top_level_transcript` (→ `is_transcript`), `parse_transcript_meta` + `extract_user_text` + `is_slash_command_envelope` (→ `parse_meta`), `derive_attention_from_content` + `derive_edited_files_from_content` + `classify*` + `EDIT_TOOL_NAMES` (→ `derive`), the `claude --session-id` argv (→ `spawn` returning `PinnedId`), `claude --resume <id>` (→ `resume_command`), stem-based id derivation (→ `session_id_from_path`).
  3. Add `Session.agent: AgentKind` (constructed as `Claude` everywhere) and `CachedSession.agent` (serde default `"claude"`).
  4. Route all former call sites through `agent(kind)`. Grep-gate: after this package, `"claude"`, `stop_reason`, `.claude/projects`, `--session-id`, `--resume` appear **only** under `src/agents/` and in `cli.rs`/UI strings (those move in WP7/WP9).
  5. Add the §2.2 discipline to `ARCHITECTURE.md` and describe the new component; move existing tests alongside the moved code; keep the `host.rs` ssh-quoting pin tests passing (parameterized listing must produce byte-identical `find` commands for claude's spec).
- **Acceptance:** `cargo test` green with **no behavioral test changes** (moves only); the grep-gate above; `agent-mux-review` finds no discipline violations; a fresh run against a real `~/.claude/projects` behaves identically (verify with `/verify`-style manual smoke: dashboard paints, attach works, spawn works).

### WP2 — Config + multi-root discovery/watch plumbing

- **Size:** medium (~1 agent-day). Sequential after WP1 (touches the same discovery/watcher files).
- **Files:** `config.rs`, `discovery.rs`, `watcher.rs`, `main.rs`, `cache.rs`, `README.md`, `FEATURES.md`, `TODO.md`.
- **Work:**
  1. `[agents.<label>]` table (`enabled`, `binary`, `transcript_root`) + per-host `[hosts.<name>.agents.<label>] transcript_root`; legacy per-host `transcript_root` aliases to claude's; reject unknown labels at load with a clear error (same pattern as the reserved `local` host name).
  2. Discovery iterates (host × enabled agents); each triple resolves its root (per-host override → agent config → agent default, tilde-expanded) and lists/parses through the agent. `NewTranscript`/`Attention` events carry `AgentKind` alongside `HostId`.
  3. Local watcher: watch every enabled agent's root under one `notify` watcher; route events by longest-prefix root match. Remote poller: per-host tick loops over enabled agent roots (still one `find` per root per tick — note the cost in the code comment; an idle N-agent host is N cheap finds).
  4. Startup/liveness: a root that doesn't exist (agent not installed on that host) is silently skipped — presence of the directory *is* the installation signal. No new config needed per host for the common case.
- **Acceptance:** with no `[agents]` table, behavior byte-identical to today (claude only, all existing tests). With `[agents.codex] enabled = true` and an empty stub parser, a fake `~/.codex/sessions` tree is listed and events route to the codex stub (integration test with temp dirs). Config parse errors are informative.

### WP3 — Codex read path (discovery + parser)

- **Size:** medium-large (~1 agent-day). **Parallel-safe with WP4** (fills only `src/agents/codex.rs` + `tests/fixtures/codex/` + its own test file).
- **Depends on:** Appendix A. Read it in full before writing code.
- **Files:** `src/agents/codex.rs`, `tests/fixtures/codex/**`, `tests/codex_parser.rs` (or in-module tests).
- **Work:**
  1. `ListingSpec` for `<root>/YYYY/MM/DD/rollout-*.jsonl` (mindepth 4 / maxdepth 4, glob `rollout-*.jsonl`). Skip `.jsonl.zst` (documented gap, §2.5). `session_id_from_path` = trailing uuid of the filename (`rollout-<local-ts>-<uuid>.jsonl` — timestamp contains dashes, so parse the uuid from the *end*, 36 chars).
  2. `parse_meta`: line 1 `session_meta` → `cwd`, `id`; title = first `user_message` event (legacy) / `UserMessage` item (paginated) / none. Tolerate `turn_context` cwd refinement being absent.
  3. `derive`: scan the tail for the last of `turn_started` / `turn_complete` / `turn_aborted` (`event_msg` payload `type`) → Working / NeedsInput / NeedsInput. Edited files from `patch_apply_end.changes` keys **and** `item_completed` → `FileChange` items (both history modes; Appendix A §2b). Ignore unknown record types at both nesting levels *silently* — churn resilience is the design requirement, not a nicety.
  4. Fixtures: legacy-mode and paginated-mode transcripts; mid-turn tail (open `turn_started`); completed-turn tail; aborted; a rollout with unknown future record types interleaved; a file whose tail window starts mid-JSON-line (the existing claude parser's partial-first-line handling is the pattern).
- **Acceptance:** parser unit tests green; a synthetic `~/.codex/sessions` tree + `[agents.codex] enabled = true` shows codex sessions in the dashboard with correct cwd, title, attention, edited files (integration test through discovery). No changes outside the three named paths.

### WP4 — Pi read path (discovery + parser)

- **Size:** medium (~½–1 agent-day). **Parallel-safe with WP3.**
- **Depends on:** Appendix B.
- **Files:** `src/agents/pi.rs`, `tests/fixtures/pi/**`, `tests/pi_parser.rs`.
- **Work:**
  1. `ListingSpec` for `<root>/--<encoded-cwd>--/<ts>_<id>.jsonl` (mindepth 2 / maxdepth 2, `*.jsonl`). `session_id_from_path` = substring after the first `_` in the stem (**not** assumed to be a uuid — pi allows `[A-Za-z0-9._-]` ids). Root resolution honors `PI_CODING_AGENT_SESSION_DIR` / `PI_CODING_AGENT_DIR` env when present locally (config override still wins; remote roots come from config).
  2. `parse_meta`: line-1 `session` header → `cwd`, `id`; title = latest `session_info.name`, falling back to first user message. Note the format is *tree-structured* (`id`/`parentId`) — for meta and tail-classification purposes read linearly like the others; branch awareness is explicitly out of scope (matches pi's own tooling behavior for previews).
  3. `derive`: last `message` entry — assistant `stopReason` mapping per §2.5; `toolCall` content blocks named `write`/`edit` → `path` argument resolved against header cwd (also tolerate the legacy `file_path` alias).
  4. Fixtures per the same checklist as WP3, plus: a v3 header line, a session with `session_info` rename, a relative-path `edit` call.
- **Acceptance:** same shape as WP3's, for pi.

### WP5 — Codex spawn/resume (attachment integration)

- **Size:** medium-large (~1 agent-day). Sequential after WP3 (needs the codex parser for adoption) and after any concurrent attachment work — touches `attachment.rs`, which is shared spine. **Not parallel with WP6** (same files); run them back-to-back, either order.
- **Files:** `attachment.rs`, `main.rs` (pending-spawn table + adoption), `agents/codex.rs` (spawn/resume methods), `new_session_modal.rs` only if unavoidable.
- **Work:** implement §2.4 exactly: `SpawnPlan::DiscoverAfterSpawn`, the `agent-mux-pending-<nonce>` naming, the adoption path in the `NewTranscript` handler (cwd match within `ADOPTION_WINDOW`), `tmux rename-session` on adopt, footer error on window expiry, `resume_command` = `codex resume <id>`. The resume-fallback tmux command (`new-session -A -s agent-mux-<id> … codex resume <id>`) reuses the existing idempotent machinery. Extend `resolve_pane_target` tests to cover an adopted codex session re-attaching by name.
- **Acceptance:** unit tests for the adoption state machine (pending → adopted / expired); ssh-quoting pin test for the remote codex resume argv; manual smoke with a real `codex` binary if present on the dev box (spawn from `n`, watch the row adopt the real id, re-attach by row) — if no binary is available, say so in the completion report rather than claiming end-to-end verification.

### WP6 — Pi spawn/resume

- **Size:** small (~½ agent-day). Sequential with WP5 (shared `attachment.rs`).
- **Files:** `attachment.rs` (minimal — pi is `PinnedId`, the claude path generalizes), `agents/pi.rs`.
- **Work:** `spawn` = `pi --session-id <uuid>` via `SpawnPlan::PinnedId` (identical contract to claude: minted uuid = tmux name suffix = SessionId; pi names the file `<ts>_<uuid>.jsonl`, and `session_id_from_path` bridges the stem difference). `resume_command` = `pi --session-id <id>` (exact-id reopen, per Appendix B §3). Pin tests mirroring claude's.
- **Acceptance:** spawn/resume unit tests green; the WP5 manual-smoke caveat applies equally.

### WP7 — UI: agent tag + new-session agent picker

- **Size:** medium (~½–1 agent-day). Sequential after WP2 (needs multi-agent config); parallel-safe with WP5/WP6 if it strictly avoids `attachment.rs` (it should — the picker emits an `AgentKind` on the existing create-action channel).
- **Files:** `dashboard.rs`, `new_session_modal.rs`, `quickswitcher.rs`, `cli.rs` (help text), `main.rs` (action plumbing), `FEATURES.md`, `README.md`.
- **Work:** (1) an agent label in the session row's line-2 status (and quickswitcher rows), rendered **only when ≥2 agents are enabled** — the claude-only dashboard stays pixel-identical; (2) an agent-selection step in the `n`/`N` flow (same ≥2 gate; default = first enabled, remembered per repo is *not* v1); (3) `cli.rs` strings de-Claude-ified where they're now wrong ("terminal multiplexer for Claude Code sessions" → agent CLIs). Respect the dashboard's altitude rules — the tag competes with the attention glyph; dim/short (`cx`/`pi`/`cl`? decide in-package with a screenshot for the user).
- **Acceptance:** ratatui test-backend snapshots for both gates (1 agent = unchanged; 2 agents = tag + picker step); `build_display_rows` unit tests extended.

### WP8 — Hooks/attention parity, Codex first (Phase 4)

- **Size:** large; **explicitly deferrable** — ship WP0–WP7 and dogfood first. Filed here so the design is on record.
- **Files:** `hook_install.rs`, `hook_ingest.rs`, `agents/codex.rs`, config, README.
- **Work:** extend `install-hooks` with an `--agent codex` mode writing `~/.codex/hooks.json` handlers for `PermissionRequest` (→ blocking marker) and `Stop` (→ turn-complete marker), reusing the existing `.agent-mux-hooks` marker pipeline and `blocking_prompt` catalog pin (the consumer side is already agent-neutral). Decide marker-dir placement for codex roots (same `<transcript-root>/.agent-mux-hooks` convention — but note Codex's tree is date-nested; the hook dir sits at the root level and the existing depth-guards must ignore it, exactly as the claude tree does today). Pi's equivalent (a drop-in extension under `~/.pi/agent/extensions/` emitting the same markers) is optional and gated on pi actually being dogfooded.
- **Acceptance:** codex blocked-on-approval surfaces as `◆ blocked` within one poll tick; uninstall/idempotency mirror the claude installer's tests.

### WP9 — Docs close-out + ledger

- **Size:** small. Last.
- **Files:** `README.md`, `FEATURES.md`, `ACCEPTANCE.md`, `SPEC.md`, `ARCHITECTURE.md`, `TODO.md`, this file (status → shipped/partial).
- **Work:** README gains an "Other agents" section (what works per agent, the documented gaps table from §2.5, config example); FEATURES ledger entries; ACCEPTANCE criteria for the multi-agent milestone; TODO entries for the deliberate deferrals (zstd rollouts, Codex shell-edit capture, pi extension signal, per-repo default agent, Codex `session_index.jsonl` names as titles).

## 4. Risks and mitigations

- **Codex schema churn (highest).** ~Daily releases; the rollout format has no compat guarantee and is mid-migration (legacy → paginated history mode). Mitigation: key attention on the three turn events persisted in *both* modes; parse both edited-file encodings; ignore unknown types silently; fixture-pin what we depend on so an upstream break fails loudly in *our* tests, not in the field. Accept that Codex support is "current-release best effort" and say so in README.
- **Codex adoption race.** Two codex spawns in the same cwd inside one adoption window could cross-adopt ids. Mitigation: match on (cwd, spawn-order, file-birth-order) and shrink the window; residual risk is the same two-unnamed-sessions-one-cwd collision the codebase already documents for external claude sessions — acceptable, documented.
- **Per-tick remote cost scales with enabled agents.** N roots = N `find`s per host tick. Mitigation: roots whose directory doesn't exist are dropped from the tick loop after first check (per-host, per-agent memo); mtime-skip already bounds the rest. Only *enabled* agents cost anything.
- **Attention-semantics drift across agents** (e.g. Codex blocked ≈ working until WP8) could erode trust in the sidebar. Mitigation: the §2.5 gaps table ships in README the day multi-agent ships; WP8 is the standing fix.
- **Spec identity creep.** Supporting N agents invites "support everything" scope pressure (Cursor, Aider, …). Mitigation: WP0's spec language sets the bar — an agent qualifies only if it persists locally-tail-parseable transcripts and is resumable by id from the CLI. Agents that don't qualify are out, by criterion rather than by name.

## 5. Explicitly out of scope (this plan)

- Rendering any conversation content in agent-mux's own UI (unchanged product boundary).
- Driving agents via their RPC/app-server modes (pi `--mode rpc`, `codex app-server`) — transcript-tailing stays the source of truth; RPC is a fallback documented in the appendices if tailing ever proves insufficient.
- Aider/Cursor/other agents — nothing precludes them; each is "write `src/agents/<x>.rs` + fixtures" once WP1/WP2 exist, evaluated against the WP0 criterion.
- Per-agent theming/keybinds; mixed-agent session *conversion*; cross-agent worktree handoff.

---

## Appendix A — Codex CLI facts (researched 2026-07-09, rust-v0.144.1)

*Sessions on disk.* Root `$CODEX_HOME` (default `~/.codex`), transcripts at `sessions/YYYY/MM/DD/rollout-YYYY-MM-DDThh-mm-ss-<uuid>.jsonl` (local time, dashes for colons; uuid = thread/conversation id — recoverable from the filename). Archived: `archived_sessions/`. A background worker zstd-compresses cold rollouts to `.jsonl.zst` siblings; appends re-materialize plain `.jsonl`. SQLite sidecars (`state_5.sqlite` etc.) are derived caches — do not build on them. `~/.codex/history.jsonl` is unrelated (cross-session prompt history). Thread display names live in `$CODEX_HOME/session_index.jsonl` (append-only `{id, thread_name, updated_at}`, last wins). Rollouts can reach hundreds of MB — never whole-file read; head + tail only.

*Record schema.* Each line: `{"timestamp", "type", "payload"}` with `type` ∈ `session_meta | response_item | compacted | turn_context | world_state | event_msg | inter_agent_communication(_metadata)`. Line 1 is `session_meta`: `id`, `session_id` (compat alias), `timestamp`, `cwd`, `originator`, `cli_version`, `source`, `forked_from_id`, `history_mode` (`legacy`|`paginated`), optional `git {commit_hash, branch, repository_url}`. No title field. `turn_context` lines carry per-turn `cwd`/`model`/`approval_policy`. `event_msg` payloads have their own `type`: **always persisted** (both history modes): `token_count`, `turn_started {turn_id, started_at, …}`, `turn_complete {turn_id, last_agent_message, …}`, `turn_aborted {reason}`, `thread_rolled_back`, `thread_goal_updated`. **Legacy mode only:** `user_message {message}`, `agent_message`, `agent_reasoning`, `patch_apply_end {changes: map<path, FileChange>, success}`, `context_compacted`, `mcp_tool_call_end`. **Paginated mode instead:** `item_completed` wrapping `TurnItem`s (`UserMessage`, `AgentMessage`, `FileChange {changes, status}`, `CommandExecution`, `Plan`, `McpToolCall`). `FileChange` = `{add:{content}} | {delete:{content}} | {update:{unified_diff, move_path}}`. Local file stores default to **legacy** but migration to paginated is in flight (changelog 2026-07-08) — parse both. **Approval prompts are never persisted** (`exec_approval_request`, `apply_patch_approval_request`, `request_user_input`, `elicitation_request` are non-persisted by policy) — blocked-on-approval is invisible in JSONL.

*CLI.* **No `--session-id` equivalent; upstream declined it** (discussion #3827). Resume: `codex resume [SESSION_ID|name] [PROMPT]`, `--last` (cwd-filtered), `--all`; non-interactive `codex exec resume …`. Fork: `codex fork [id]` (new id, `forked_from_id` recorded). `codex archive|delete|unarchive`. Spawn flags: `-C/--cd <dir>`, positional prompt, `-c key=value` overrides, `--ephemeral` (**no rollout — never use**), `-o` (exec). `codex exec --json` emits `{"type":"thread.started","thread_id":"…"}` first on stdout. `[tui].terminal_title` config can include `thread-id`/`run-state` → readable via tmux `#{pane_title}`.

*Config.* `~/.codex/config.toml`; `$CODEX_HOME` relocates everything (no separate sessions-dir key); project-level `<repo>/.codex/config.toml` merges. `notify = ["prog", …]` (legacy): spawns prog with one JSON argv arg, only event `agent-turn-complete` (kebab-case keys, includes `thread-id`, `cwd`, `last-assistant-message`). **Lifecycle hooks** (2026): `~/.codex/hooks.json` / `<repo>/.codex/hooks.json` / `[hooks]` in config.toml; events `SessionStart, SubagentStart, PreToolUse, PermissionRequest, PostToolUse, PreCompact, PostCompact, UserPromptSubmit, SubagentStop, Stop`; command handlers get JSON on **stdin** (`session_id`, `cwd`, `hook_event_name`, …). `PermissionRequest` is the needs-approval signal; `Stop` is turn-complete.

*Stability.* No published compat guarantee; ~daily releases; known past breaks (session_id alias, instructions moved to turn_context, 5 sqlite schema generations, zstd addition, history-mode migration). Parse defensively; pin with fixtures.

## Appendix B — Pi facts (researched 2026-07-09, @earendil-works/pi-coding-agent v0.80.6)

*Identity.* Mario Zechner's pi; repo moved to `github.com/earendil-works/pi`; npm `@earendil-works/pi-coding-agent` (old `@mariozechner/pi-coding-agent` deprecated at 0.73.1). Docs: `https://pi.dev/docs/latest`. Binary: `pi`.

*Sessions on disk.* `~/.pi/agent/sessions/--<encoded-cwd>--/<iso-ts-with-:.-replaced-by-->_<session-id>.jsonl` — cwd encoding: strip leading `/`, replace `/\:` with `-`, wrap in `--…--` (readable, not hashed). Session id defaults to `randomUUID()` but any `[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?` id is legal — don't assume uuid shape. JSONL, append-only, **tree-structured** (`id` 8-hex + `parentId`; in-file branching). Overrides: env `PI_CODING_AGENT_SESSION_DIR` (highest), `PI_CODING_AGENT_DIR` (relocates `~/.pi/agent`), settings key `sessionDir` (global `~/.pi/agent/settings.json`, project `.pi/settings.json`). Pi sets `PI_CODING_AGENT=true` in its own process env (pane-process detection marker).

*Record schema* (documented: pi.dev/docs/latest/session-format; versioned, `CURRENT_SESSION_VERSION = 3`, v1/v2 auto-migrate, loader tolerates unknown entries). Line 1 header: `{"type":"session","version":3,"id","timestamp","cwd","parentSession?"}`. Entry types: `message, model_change, thinking_level_change, compaction, branch_summary, label, session_info, custom, custom_message`. `message.role` ∈ `user | assistant | toolResult | bashExecution | custom | branchSummary | compactionSummary`. Assistant messages carry `provider`, `model`, `usage`, **`stopReason` ∈ `stop | length | toolUse | error | aborted`**. Title: `session_info.name` (set via `--name`/`/name`/RPC), else first user message. Edited files: assistant content blocks `{"type":"toolCall","id","name","arguments"}` — `write {path, content}`, `edit {path, edits:[{oldText,newText}]}` (legacy top-level `oldText`/`newText` and a `file_path` alias tolerated); paths may be relative to header cwd. No built-in permission prompts (extensions can add gates — invisible to the tail).

*CLI.* `pi -c/--continue` (latest for cwd); `pi -r/--resume` (picker); `pi --session <path|id-prefix>`; **`pi --session-id <id>`** — "use exact project session ID, creating it if missing" (verified in source: opens if exists, else creates with pinned id) — the direct `claude --session-id` analog, usable for spawn *and* resume; `pi --fork`, `pi --no-session` (ephemeral — never use for spawns), `pi -n/--name`, `pi -p/--print`, `pi --session-dir <dir>`.

*Events/RPC (Phase-4 options).* `pi --mode rpc` (JSONL over stdio: `agent_start/agent_end/agent_settled`, `tool_execution_*`, `set_session_name`, …; sessions still persist to disk); `pi --mode json` (read-only event stream); TS extensions auto-load from `~/.pi/agent/extensions/` + `.pi/extensions/` and can subscribe to `agent_settled`/`tool_call` — a marker-emitting extension is the pi hook-parity path.

*Stability.* Format documented + versioned (good); release cadence fast (29 releases in 2 months) and one org/package rename already — pin the binary name via `[agents.pi] binary` if needed.

## Appendix C — Coupling survey (summary; full survey in session notes 2026-07-09)

Deep/structural coupling (needs the WP1 trait): transcript directory shape (`host.rs` `LocalHost::list_transcripts` read_dir depth-2 + `SshHost::list_transcripts` `find -mindepth 2 -maxdepth 2 -name '*.jsonl'`; `watcher.rs` `is_top_level_transcript` + sidechain `subagents/` filtering); the JSONL parser family (`watcher.rs` `classify_assistant` reading `message.stop_reason`, `EntryKind`/`classify`, `derive_attention_from_content`, `derive_edited_files_from_content` with `EDIT_TOOL_NAMES=[Edit,Write,MultiEdit,NotebookEdit]` and `input.file_path|notebook_path`; `discovery.rs` `parse_transcript_meta` with `cwd`/`ai-title`/`aiTitle`, `extract_user_text`, `is_slash_command_envelope`, `fallback_dir` bucket-name decode); spawn/resume argv (`attachment.rs` `tmux_new_claude_session_argv` with literal `claude --session-id`, resume `claude --resume <id>` in both drivers, `launch_in_new_window(cwd, "claude")`); the identity contract (minted uuid == tmux `agent-mux-<uuid>` == transcript stem == SessionId; `discovery.rs` stem-derivation); hooks (`hook_ingest.rs` `notification_type` vocabulary + `.agent-mux-hooks` markers; `hook_install.rs` writing `~/.claude/settings.json`).

Shallow/surface: `config.rs` `default_transcript_root()`; `cli.rs` help/banner strings; `cache.rs` `CachedSession` (needs an `agent` field); `IDLE_THRESHOLD` (agent-neutral overlay); `main.rs` "live claude detected" warning strings.

Already agent-agnostic: catalog, notifier + dispatchers, embedded PTY, quickswitcher, favorites, session renames, repo registry, worktree manager, remote-session cache mechanism, `agent-mux-<id>` pane-resolution machinery (once argv is parameterized).
