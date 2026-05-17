# Architecture

How this project is built. Engineering choices and the disciplines that follow from them. Companion to `SPEC.md` (what it is) and `PROCESS.md` (how we work).

## Status

M0 shipped (skeleton + discovery + transcript watcher + tmux attach with the outside-tmux and resume-from-transcript fallbacks). M1 substantially shipped: workspace-folder Config, Repo Registry (with TTL refresh on picker open), WorktreeManager, the new-session UI flow, session titles, the return-to-dashboard footer hint, and live discovery of new sessions via a recursive watch on the projects root. Remaining M1 polish lives in `TODO.md`. M2 substantially shipped: host config schema, the `Host` trait, `LocalHost`, `SshHost`, the per-host startup-discovery wire-up (one background thread per `[hosts.<name>]`, sessions stream into the catalog as connects succeed, initial attention pre-computed against the same connection), per-host remote-attention polling (one background polling thread per connected host, mtime-skip keeps idle hosts cheap, `NewTranscript` events carry the originating `HostId` so the catalog builds the session through the right `Host` impl), and remote tmux attach + spawn-terminal with an idempotent `claude --resume` fallback when no remote pane matches. What remains: portability items (macOS-remote `find` flavour) and remote-side observability — both in `TODO.md`. Remote session *creation* is post-M5.

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

- **Dashboard.** The ratatui-based TUI. Subscribes to catalog changes and re-renders. Dispatches user input as actions (`attach`, `open-terminal`, `new-session`, `quit`, etc.) and never touches the attachment mechanism directly — it only sends actions to the Attachment Driver via the catalog's action channel. For the new-session flow, the dashboard reads the Repo Registry to populate the picker. List rows are grouped in two levels: host headers (local first, SSH hosts alphabetical; only hosts with ≥1 session get a header), and within each host, project headers (`project_dir` of each session, ordered by the project's most-recent session). Sessions within a project order by recency. The grouping lives in the `dashboard` module's pure `build_display_rows` so the layout logic is unit-testable without standing up a ratatui frame.
- **Session Catalog.** In-memory single source of truth for the set of known sessions and their derived state. Updated by the Transcript Watcher (attention state changes) and by user actions (create, dismiss, refresh). The Dashboard reads from it; nothing else does.
- **Transcript Watcher.** Tails Claude Code's transcript JSONL files and emits three kinds of events: `Attention` updates for transcripts already known to the catalog, `NewTranscript` notifications for `.jsonl` files appearing under the projects root that the catalog has not yet seen, and `LivePanes` snapshots of every tmux pane's `pane_current_path` per host (for the per-session "Enter is a fast switch vs. an auto-resume" indicator). For local hosts, a single recursive `notify` watch on `~/.claude/projects/` covers both transcript-event kinds. For remote hosts (M2), the equivalent is a per-host polling loop on a background thread, ticking every `REMOTE_POLL_INTERVAL` (3s) against the host's existing `ControlMaster` connection. The loop uses the mtime returned by `Host::list_transcripts` to skip files that haven't changed, so an idle ten-session host costs one `find` per interval rather than N `tail -c` round-trips. `NewTranscript` events carry the originating `HostId` (and the file's mtime) so the dashboard routes the path through the correct `Host` impl when it builds the session — local transcripts come from the recursive `notify` thread, remote transcripts from the polling thread. Pane-presence polling is a sibling thread per host (local + each remote) on the same 3s cadence — separate from the transcript poller so a slow `tmux list-panes` over ssh can't backpressure the attention pipeline. The watcher is responsible for *deriving* attention state from transcript content and *capturing* pane state from tmux; the catalog only stores the result.
- **Attachment Driver.** A trait describing the "attach", "spawn-terminal", and "spawn-session" operations. The tmux implementation drives all three by shelling out to `tmux`. For remote sessions, attach + spawn-terminal dispatch on `session.host.is_local()`: the driver runs `tmux list-panes` on the remote to find a pane whose `pane_current_path` matches the session's `project_dir`, then `ssh -t <target> tmux attach -t <pane>`. When no pane matches, it falls through to `ssh -t <target> tmux new-session -A -s agent-mux-<id> -c <cwd> claude --resume <id>` — `-A` makes the fallback idempotent (a second attempt reuses the spawned session rather than racing a parallel `claude --resume` on the transcript). Inside local tmux this lives in a new local tmux window (nested tmux); outside local tmux it `SuspendAndRun`s the ssh directly. The trait exists so the M3 inline-preview work, and any future Shape B pivot, can introduce alternative implementations without disturbing the Dashboard or Catalog. This is the load-bearing abstraction for the optionality the project depends on.
- **Repo Registry (M1).** In-memory list of git repos discovered by scanning each configured workspace folder one level deep at startup. A child counts as a repo only if it contains `.git/` as a *directory* — worktree pointer files (where `.git` is a file containing `gitdir: ...`) are deliberately excluded so agent-mux-created worktrees don't litter the picker alongside their parent repos. The Dashboard reads from the Registry to populate the new-session picker; nothing else writes to it. Refresh policy: a fresh scan runs at boot, and when the new-session picker opens after its cached snapshot has aged past a TTL. No background polling, no filesystem watching for now (deferred to dogfooding).
- **Config (M1 minimal, M2 hosts, M5 expanded).** TOML loaded at startup from `~/.config/agent-mux/config.toml`. M1 contributes `workspace_folders = [...]`. M2 adds `[hosts.<name>]` tables with `ssh = "<target>"` (a `~/.ssh/config` alias or `user@host`) and an optional `transcript_root` (defaults to `~/.claude/projects`; tilde-expanded). The table key `<name>` is the dashboard label; the SSH target is a separate field so the label can stay friendly while the destination changes (alias rename, key rotation, IP shift). The name `local` is reserved — the local host is implicit, never configured — and is rejected at load with a clear error. Per-host `workspace_folders` is deliberately omitted in M2: this milestone covers attach to existing remote sessions only; remote session *creation* (which would need remote repo discovery) is post-M5 and will extend the schema when it lands. M5 extends this further with themes, keybinds, reload-on-edit, and the M4 notification suppression knobs. The file is the source of truth; agent-mux never writes to it.
- **Worktree Manager (M1).** Creates, lists, and removes git worktrees by shelling out to `git worktree`. Used by the new-session flow: given a repo (picked from the Repo Registry), a base branch, and a task name, it creates a worktree alongside the repo and returns its path, which the Attachment Driver then uses to spawn `claude` inside a new tmux window. Conventions settled in M1 design:
  - **Layout.** Worktrees live alongside the parent repo (`<repo>/../<repo>-<task>`). Keeps them discoverable in normal file navigation and out of the parent tree.
  - **Base branch.** The new-session prompt pre-fills the repo's default branch, resolved via `git symbolic-ref --short refs/remotes/origin/HEAD` (strip the `origin/` prefix), falling back to `main` then `master`, and finally prompting with no default if none of those resolve. The user can always override the pre-filled value.
  - **Task metadata.** A `.agent-mux/task.toml` file inside the worktree, with `task`, `base_branch`, and `created_at` fields. TOML keeps room for future structured fields without a format change.
  - **Fate on session close.** The worktree is kept by default; a discard/merge workflow is deferred to post-M5. The user can clean up manually with `git worktree remove`.
- **Remote-session Cache (post-M2 polish).** Per-host snapshot files under `~/.cache/agent-mux/sessions/<host>.json` storing the last-known session list for each `[hosts.<name>]` (id, project_dir, transcript_path, last_activity, title, attention). Read synchronously at startup to seed the catalog *before* any SSH handshake fires — the dashboard paints configured remote rows on first frame instead of waiting seconds for each `ControlMaster` to connect. Written off the UI thread by `connect_and_discover` after a successful live discovery, via `tmp` + rename so a crashed write never leaves a half-truncated file. Reconciled on each `RemoteDiscoveryResult::Ready` via `SessionCatalog::reconcile_host` (drop entries the remote no longer has, overlay live state on the rest, append new). Best-effort throughout: missing, unreadable, or corrupt files silently degrade to an empty list rather than failing startup or discovery. Per-host files keep one bad snapshot from poisoning the others, and the wire format is a parallel `CachedSession` struct (not serde-on-`Session`) so in-memory fields evolve independently of the on-disk schema.
- **Host Abstraction.** Hides the local-vs-SSH distinction from the rest of the system for *read* operations against transcripts and worktree metadata: list `.jsonl` files under a root, read a small file in full, tail the last N bytes of a transcript, and test whether a path is a directory. The surface is deliberately narrow — only the operations the Transcript Watcher, discovery, and the title resolver actually need. `LocalHost` is pure `std::fs`. `SshHost` shells out to the system `ssh` binary with a single `ControlMaster` connection per host — opened in the constructor, reused for every operation, torn down by the `Drop` impl via `ssh -O exit` (with a `ControlPersist=600` belt-and-braces in case the drop guard never fires). Remote ops are deliberately simple: `if [ -d ROOT ]; then find -printf '%T@ %p\0'` for listing (NUL-delimited so paths with whitespace survive), `cat` / `tail -c <n>` for reads, `test -d` for is-dir. GNU `find` is assumed on the remote — a macOS-remote portability item is filed in TODO. Operations that *spawn* (attach into a remote tmux, open a shell in a session's cwd) live in the Attachment Driver instead — that subsystem builds the right argv per host. To support that without leaking SSH-binary/socket/target details outward, the trait also exposes one piece of informational metadata: `ssh_argv(tty, remote_cmd)` returns the argv that would invoke `remote_cmd` over this host's `ControlMaster` (or `None` for local). The Attachment Driver branches on this — `None` → run the command directly; `Some(argv)` → use it as the actual `Command` to spawn. Building argv stays inside `Host`; running it stays inside the Attachment Driver. Post-M5: when remote session creation lands and brings per-host `workspace_folders` with it, the Repo Registry will scan remote workspaces through this same abstraction.

## Disciplines

Architectural rules. Each one stated as a constraint with a reason. These are the basis of Layer 2 review (see `PROCESS.md`).

- **tmux specifics live behind the Attachment Driver.** No `tmux` shell commands, window IDs, or session names appear in the Dashboard, Session Catalog, Transcript Watcher, or Host Abstraction. Reason: if the M2 preview experiment surfaces demand for full chat rendering (Shape B), the pivot is "add a new Attachment Driver implementation" rather than "rewrite the world." A leak of tmux strings into any other module destroys this property.
- **Sessions are host-agnostic on the API surface.** A `Session` carries a `HostId` (the dashboard label / config key), but read operations (transcript discovery, attention derivation, metadata reads) go through the Host Abstraction and spawn/attach operations go through the Attachment Driver. No `if session.host.is_local()` branches outside those modules. Reason: keeps remote and local in lockstep; a feature that works for one works for both by construction.
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

## Process lifecycle

What agent-mux owns at runtime, and what happens to each piece of state across the three shutdown paths: a clean quit (`q` / Ctrl-C), a panic in a background thread, and a process kill (SIGKILL / power loss). The contract here is "no quiet leaks under the common cases; ControlPersist as the safety net for everything else."

### State agent-mux owns

- **SSH `ControlMaster` sockets** — one per configured remote host, at `$TMPDIR/agent-mux-ssh-<host>-<pid>.sock`. PID-namespaced so concurrent agent-mux processes don't collide. Created in `SshHost::connect`, destroyed by `SshHost::Drop` running `ssh -O exit`. Pinned by integration tests in `src/host.rs::tests` so a refactor can't silently break either end.
- **Local tmux windows hosting `ssh -t target tmux attach`** — created on remote attach when running inside local tmux. The window's foreground process is the ssh; when ssh exits (user detaches from remote tmux), the window self-closes via tmux's default `remain-on-exit off`. We rely on the user's tmux config here.
- **Remote `agent-mux-<conv-id>` tmux sessions** — created by the resume fallback inside the *remote* tmux. **Deliberately persistent**: the `-A` flag on `tmux new-session` makes a second attempt re-attach to the same session, which is what makes `claude --resume` idempotent. agent-mux does not clean these up; the README documents `tmux kill-session -t agent-mux-<id>` for the user. A discard affordance is filed in TODO.
- **Background threads** — one notify-driven local watcher, one transcript poller per connected remote host, one pane poller per host (local + each remote), plus one short-lived per-host SSH discovery thread at startup and one per-worktree-create thread. Each holds clones of the same event-channel `Sender`; the catch-all teardown signal is "receiver drops, next `send` errors, thread returns."

### Across the shutdown paths

| Path | SSH master | Local tmux windows | Remote `agent-mux-<id>` | Threads |
| --- | --- | --- | --- | --- |
| `q` / Ctrl-C | `ssh -O exit` if poller-thread Arc drops in time; else ControlPersist (10 min) | Self-close when ssh exits | **Survive** (intentional) | Killed on process exit |
| Background-thread panic | Other hosts unaffected (Arcs not shared across hosts); panicking host's Arc drops on unwind, Drop fires | Unaffected | **Survive** (intentional) | Other threads continue |
| SIGKILL / power loss | ControlPersist (10 min) | Survive (no parent to die), ssh master eventually times out and the window's ssh exits | **Survive** (intentional) | Killed |

### Load-bearing properties

- **ControlPersist=600 is not optional.** A polling thread holding an `Arc<SshHost>` may be mid-`thread::sleep` at process exit, in which case the runtime kills it before the last Arc reference drops and `SshHost::Drop` therefore never runs. The remote master would then linger forever without the timeout. The connect-time test pins `ControlPersist=600` for this reason.
- **PID-namespaced socket paths.** Two concurrent agent-mux processes must not collide on a control socket. They also must not be able to "steal" each other's master. PID-in-path achieves both.
- **The remote `agent-mux-<id>` lifecycle is the user's concern.** Cleaning them up automatically would break the idempotent-resume contract. They are explicitly excluded from any future shutdown sweep.

### Known intentional leaks

- **Stale socket files on disk.** When a previous agent-mux process is killed mid-sleep, its `agent-mux-ssh-<host>-<pid>.sock` file may persist in `$TMPDIR` even after the remote master has timed out. Harmless (a fresh PID gets a fresh path) but it accumulates until `$TMPDIR` is cleaned. A pre-`connect` sweep of stale sockets is filed in TODO.
- **Disk-cache files for removed hosts.** `~/.cache/agent-mux/sessions/<host>.json` is never deleted when the user removes `[hosts.<host>]` from config. Harmless (we only load cache for hosts in current config) but leaks bytes. Filed in TODO.

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
