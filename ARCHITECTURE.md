# Architecture

How this project is built. Engineering choices and the disciplines that follow from them. Companion to `SPEC.md` (what it is) and `PROCESS.md` (how we work).

## Status

M0 shipped (skeleton + discovery + transcript watcher + tmux attach with the outside-tmux and resume-from-transcript fallbacks). M1 substantially shipped: workspace-folder Config, Repo Registry (with TTL refresh on picker open), WorktreeManager, the new-session UI flow, session titles, the return-to-dashboard footer hint, and live discovery of new sessions via a recursive watch on the projects root. Remaining M1 polish lives in `TODO.md`; M2 (remote hosts) is the next milestone.

The product shape is decided (Shape A — tmux-aware dashboard, evolving toward inline preview), and the component boundaries below are designed to keep a Shape B pivot (full TUI chat) cheap should the M3 preview experiment suggest one.

M1 (session creation + worktree management) was inserted after the dashboard spine to take agent-mux from a passive catalog to an active orchestrator — the defining capability of Conductor-shaped tools. The Worktree Manager component below was added at the same time. The Repo Registry and minimal Config components were added later, when M1's new-session UX shifted from "use the currently-selected session's project_dir" to a first-class repo picker — a better long-term model that decouples session creation from current selection and generalises cleanly to M2 remote workspaces.

## Tech stack

- **Language.** Rust, edition 2024.
- **Package manager.** cargo.
- **TUI framework.** `ratatui` with the `crossterm` backend. The de facto choice; no serious alternative in Rust.
- **Concurrency.** A simple event loop drives the UI on the main thread. Background work (transcript watching, SSH, tmux commands) communicates back via channels. `tokio` is the planned async runtime once concurrent I/O is in play; the M0 skeleton starts synchronous and adopts it when the first concurrent subsystem lands.
- **Filesystem watching.** `notify` (debounced) for local transcript files.
- **SSH.** Shell out to the system `ssh` binary with `ControlMaster` for connection reuse, rather than a Rust-native SSH client. Rationale: the user's `~/.ssh/config` aliases, agent forwarding, keys, jump hosts, and per-host quirks come for free. The cost — parsing CLI output — is small compared to re-implementing all of that.
- **Process control.** Shell out to `tmux` likewise; tmux's CLI is stable, scriptable, and the right interface.
- **Serde.** `serde` + `serde_json` for parsing Claude Code transcript JSONL; `toml` for the config file (added in M3).

## Components

```
                +---------------------------+
                |       Dashboard (UI)      |
                |  ratatui render + input   |
                +-------------+-------------+
                              |
            actions           |          state subscriptions
                              v
                +---------------------------+
                |     Session Catalog       |
                |  in-memory: id, host,     |
                |  project, attention, ...  |
                +---+---------+-------------+
                    ^         ^
       attention    |         |    spawn / attach
        events      |         |    commands
                    |         |
        +-----------+-+     +-+--------------------+
        | Transcript  |     | Attachment Driver    |
        |   Watcher   |     |  (trait)             |
        |             |     |  --- tmux impl ---   |
        +------+------+     +----------+-----------+
               |                       |
               v                       v
        +----------------------------------------+
        |          Host Abstraction              |
        |  local | ssh (ControlMaster pool)      |
        +----------------------------------------+
```

For repo discovery — a separate pipeline that the new-session picker reads from:

```
        +-----------+   boot scan    +------------+   picker query
        |  Config   | -------------> |    Repo    | <---------------- Dashboard
        | workspace |                |  Registry  |
        |  folders  |                +------------+
        +-----------+
```

- **Dashboard.** The ratatui-based TUI. Subscribes to catalog changes and re-renders. Dispatches user input as actions (`attach`, `open-terminal`, `new-session`, `quit`, etc.) and never touches the attachment mechanism directly — it only sends actions to the Attachment Driver via the catalog's action channel. For the new-session flow, the dashboard reads the Repo Registry to populate the picker.
- **Session Catalog.** In-memory single source of truth for the set of known sessions and their derived state. Updated by the Transcript Watcher (attention state changes) and by user actions (create, dismiss, refresh). The Dashboard reads from it; nothing else does.
- **Transcript Watcher.** Tails Claude Code's transcript JSONL files and emits two kinds of events: `Attention` updates for transcripts already known to the catalog, and `NewTranscript` notifications for `.jsonl` files appearing under the projects root that the catalog has not yet seen. For local hosts, a single recursive `notify` watch on `~/.claude/projects/` covers both kinds. For remote hosts (M2), the equivalent is a polling loop over the host's existing SSH connection. The watcher is responsible for *deriving* attention state from transcript content; the catalog only stores the result.
- **Attachment Driver.** A trait describing the "attach", "spawn-terminal", and "spawn-session" operations. The tmux implementation drives all three by shelling out to `tmux`. The trait exists so the M3 inline-preview work, and any future Shape B pivot, can introduce alternative implementations without disturbing the Dashboard or Catalog. This is the load-bearing abstraction for the optionality the project depends on.
- **Repo Registry (M1).** In-memory list of git repos discovered by scanning each configured workspace folder one level deep at startup. A child counts as a repo only if it contains `.git/` as a *directory* — worktree pointer files (where `.git` is a file containing `gitdir: ...`) are deliberately excluded so agent-mux-created worktrees don't litter the picker alongside their parent repos. The Dashboard reads from the Registry to populate the new-session picker; nothing else writes to it. Refresh policy: a fresh scan runs at boot, and when the new-session picker opens after its cached snapshot has aged past a TTL. No background polling, no filesystem watching for now (deferred to dogfooding).
- **Config (M1 minimal, M4 expanded).** TOML loaded at startup from `~/.config/agent-mux/config.toml`. In M1, the schema is just `workspace_folders = [...]`. M4 extends this with themes, keybinds, and reload-on-edit. The file is the source of truth; agent-mux never writes to it.
- **Worktree Manager (M1).** Creates, lists, and removes git worktrees by shelling out to `git worktree`. Used by the new-session flow: given a repo (picked from the Repo Registry), a base branch, and a task name, it creates a worktree alongside the repo and returns its path, which the Attachment Driver then uses to spawn `claude` inside a new tmux window. Conventions settled in M1 design:
  - **Layout.** Worktrees live alongside the parent repo (`<repo>/../<repo>-<task>`). Keeps them discoverable in normal file navigation and out of the parent tree.
  - **Base branch.** The new-session prompt pre-fills the repo's default branch, resolved via `git symbolic-ref --short refs/remotes/origin/HEAD` (strip the `origin/` prefix), falling back to `main` then `master`, and finally prompting with no default if none of those resolve. The user can always override the pre-filled value.
  - **Task metadata.** A `.agent-mux/task.toml` file inside the worktree, with `task`, `base_branch`, and `created_at` fields. TOML keeps room for future structured fields without a format change.
  - **Fate on session close.** The worktree is kept by default; a discard/merge workflow is deferred to post-M4. The user can clean up manually with `git worktree remove`.
- **Host Abstraction.** Hides the local-vs-SSH distinction from the rest of the system. Exposes a small set of operations: list a remote path, read a file, run a command, open a long-lived shell. The SSH implementation owns the ControlMaster lifecycle so the rest of the codebase never thinks about connection setup. For M2, the Repo Registry will also call through this abstraction so workspace folders on remote hosts can be scanned with the same pipeline.

## Disciplines

Architectural rules. Each one stated as a constraint with a reason. These are the basis of Layer 2 review (see `PROCESS.md`).

- **tmux specifics live behind the Attachment Driver.** No `tmux` shell commands, window IDs, or session names appear in the Dashboard, Session Catalog, Transcript Watcher, or Host Abstraction. Reason: if the M2 preview experiment surfaces demand for full chat rendering (Shape B), the pivot is "add a new Attachment Driver implementation" rather than "rewrite the world." A leak of tmux strings into any other module destroys this property.
- **Sessions are host-agnostic on the API surface.** A `Session` carries a `Host` field, but operations on it (read transcript, attach, spawn terminal) go through the Host Abstraction. No `if session.host == Local` branches outside that module. Reason: keeps remote and local in lockstep; a feature that works for one works for both by construction.
- **Transcript content is the source of truth for attention.** Attention state is *derived* by the Transcript Watcher from transcript events; nothing else writes attention state into the catalog. Reason: state derived from a single source can't drift. State written from multiple sources will.
- **One filesystem watcher process.** A single `notify` runtime watches all local transcript files. No per-session threads or ad-hoc filesystem polling outside the Transcript Watcher. Reason: avoid resource bloat as the session count grows, and keep file-event ordering centralized for debugging.
- **One event loop, no nested complexity.** A single event loop on the main thread drives the UI. Background subsystems live on their own threads or tokio tasks (when tokio is introduced) and talk to the UI via channels. No nested runtimes, no `block_on` inside spawned tasks, no synchronous shell-outs from the UI thread. Reason: predictable scheduling; debuggability.
- **No unsafe.** Enforced at compile time by `unsafe_code = "forbid"` in `Cargo.toml`. There is no scenario in this project where `unsafe` is justified.
- **Errors travel up; the UI decides.** Lower layers return `Result<T, E>` with informative error types. The Dashboard decides how to surface a failure (status bar, modal, log). Panicking is reserved for genuine invariant violations.
- **Tests live where the behaviour does.** Component logic gets unit tests in-tree. Cross-component behaviour goes in `tests/` as integration tests. UI smoke tests use ratatui's test backend so they run headless in CI.
- **Worktrees are created via `git worktree`, never by directory copy.** Reason: worktrees share the `.git/` database; copies are silently divergent. The Worktree Manager is the only module that runs `git` commands; all other components receive a `PathBuf` and don't think about the worktree mechanism.
- **Session switching never blocks on I/O.** Switching focus to a session is a UI state change against in-memory data — no filesystem reads, no network calls, no process spawns, no tmux queries for state we should have cached. All such work happens asynchronously in background subsystems (the Transcript Watcher, the Host Abstraction) and lands in the catalog before the user asks for it. Reason: switching latency is the project's load-bearing differentiator from claude-squad; a switch-time I/O bug is a regression, not a minor issue. Empirical anchor: claude-squad's switch latency reached 10+ seconds on larger projects, which made the tool unusable in practice. The target here is "indistinguishable from a local tmux window switch" — tens of milliseconds, not seconds.
- **Repo discovery is a startup concern, not a keystroke concern.** The Repo Registry is populated once at boot by scanning workspace folders. The new-session picker reads from the cache; opening the picker MUST NOT trigger a filesystem walk on the hot path. A re-scan may run when the picker opens *and* the cached snapshot is older than a TTL, but the picker renders from cache first and refreshes asynchronously. Reason: the same "switching never blocks on I/O" rationale applies to the first frame of any modal — a picker that takes a beat to appear is the same failure mode dressed differently.
- **Workspace folder scanning is depth-1.** A workspace folder's direct children are checked for `.git/`; the scanner does not recurse into subdirectories. Reason: predictable cost, predictable result, no surprises from deeply-nested repos. If the user keeps repos nested deeper (e.g. `~/work/clients/<client>/<repo>`), they list the deeper directory as its own workspace folder.

## Open questions

Decisions deferred. Each with a brief reason for the deferral.

- **Attention detection heuristics.** Exact rules for `needs-input` vs `working` vs `idle`. Likely "last transcript entry is an assistant message and N seconds of stillness have passed" for `needs-input`, but the exact predicates depend on transcript shape in practice. Deferred until M0 dogfooding produces real signal.
- **Session discovery vs explicit registration.** Should agent-mux auto-discover every transcript in `~/.claude/projects/` (potentially noisy if the user has dozens of old conversations), or only those touched recently, or only those the user explicitly registers? Leaning "recent + explicit override," but defer until M0 surfaces what's actually annoying.
- **Behaviour when agent-mux is launched from inside a tmux session.** `tmux switch-client` targeting a different session is fine; targeting a different window in the *same* session is also fine. But we have to decide whether agent-mux always assumes it owns its own tmux session, or whether it can attach to whatever the user is already in. Deferred to M0 implementation.
- **Repo Registry refresh policy.** Strict TTL on picker open? Manual refresh keybind? Filesystem watch on workspace folders? Leaning "TTL on picker open + manual refresh," with a long enough TTL that the common case is cache-hit. Deferred until M1 dogfooding tells us how often the user's workspace actually changes.
- **Claude Code hooks integration.** Claude Code exposes `Notification`, `Stop`, and other hook events. These would give richer attention signals than transcript tailing. Deferred until M0 transcript-based detection ships and we have a sense of where the gaps are.
- **Keybind config schema.** Action names plus key strings, but the exact key-string grammar (`ctrl+a` vs `C-a` vs the kitty-keyboard protocol's explicit codes) isn't pinned. Deferred to M3.
- **Theme schema.** Likely named colours mapping to ratatui `Style` values. Format details deferred to M3.
- **What to do when a session's tmux window is killed externally.** Drop the session, mark it as "dead," or try to relaunch? Deferred to M0 dogfooding.
