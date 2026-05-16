# Architecture

How this project is built. Engineering choices and the disciplines that follow from them. Companion to `SPEC.md` (what it is) and `PROCESS.md` (how we work).

## Status

Greenfield. The product shape is decided (Shape A — tmux-aware dashboard, evolving toward inline preview), and the component boundaries below are designed to keep a Shape B pivot (full TUI chat) cheap should the M2 experiment suggest one. No M0 code is written yet.

## Tech stack

- **Language.** Rust, edition 2024.
- **Package manager.** cargo.
- **TUI framework.** `ratatui` with the `crossterm` backend. The de facto choice; no serious alternative in Rust.
- **Async runtime.** `tokio`. One runtime for the whole process; no blocking shell-outs that hold up the UI thread.
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

- **Dashboard.** The ratatui-based TUI. Subscribes to catalog changes and re-renders. Dispatches user input as actions (`attach`, `open-terminal`, `quit`, etc.) and never touches the attachment mechanism directly — it only sends actions to the Attachment Driver via the catalog's action channel.
- **Session Catalog.** In-memory single source of truth for the set of known sessions and their derived state. Updated by the Transcript Watcher (attention state changes) and by user actions (refresh, dismiss). The Dashboard reads from it; nothing else does.
- **Transcript Watcher.** Tails Claude Code's transcript JSONL files and emits attention events. For local hosts, uses `notify` for filesystem events. For remote hosts (M1), polls the file over the host's existing SSH connection at a configured interval. The watcher is responsible for *deriving* attention state from transcript content; the catalog only stores the result.
- **Attachment Driver.** A trait describing the "attach" and "spawn-terminal" operations. The M0 implementation drives `tmux`. The trait exists so the M2 inline-preview work, and any future Shape B pivot, can introduce alternative implementations without disturbing the Dashboard or Catalog. This is the load-bearing abstraction for the optionality the project depends on.
- **Host Abstraction.** Hides the local-vs-SSH distinction from the rest of the system. Exposes a small set of operations: list a remote path, read a file, run a command, open a long-lived shell. The SSH implementation owns the ControlMaster lifecycle so the rest of the codebase never thinks about connection setup.
- **Config (M3).** TOML loaded at startup from `$XDG_CONFIG_HOME/agent-mux/`. Hosts list, theme, keybinds. No runtime config writing — the file is the source of truth.

## Disciplines

Architectural rules. Each one stated as a constraint with a reason. These are the basis of Layer 2 review (see `PROCESS.md`).

- **tmux specifics live behind the Attachment Driver.** No `tmux` shell commands, window IDs, or session names appear in the Dashboard, Session Catalog, Transcript Watcher, or Host Abstraction. Reason: if the M2 preview experiment surfaces demand for full chat rendering (Shape B), the pivot is "add a new Attachment Driver implementation" rather than "rewrite the world." A leak of tmux strings into any other module destroys this property.
- **Sessions are host-agnostic on the API surface.** A `Session` carries a `Host` field, but operations on it (read transcript, attach, spawn terminal) go through the Host Abstraction. No `if session.host == Local` branches outside that module. Reason: keeps remote and local in lockstep; a feature that works for one works for both by construction.
- **Transcript content is the source of truth for attention.** Attention state is *derived* by the Transcript Watcher from transcript events; nothing else writes attention state into the catalog. Reason: state derived from a single source can't drift. State written from multiple sources will.
- **One filesystem watcher process.** A single `notify` runtime watches all local transcript files. No per-session threads or ad-hoc filesystem polling outside the Transcript Watcher. Reason: avoid resource bloat as the session count grows, and keep file-event ordering centralized for debugging.
- **One async runtime.** A single `tokio` runtime for the whole process. No nested runtimes, no `block_on` inside spawned tasks, no synchronous shell-outs from the UI thread. Reason: predictable scheduling; debuggability.
- **No unsafe.** Enforced at compile time by `unsafe_code = "forbid"` in `Cargo.toml`. There is no scenario in this project where `unsafe` is justified.
- **Errors travel up; the UI decides.** Lower layers return `Result<T, E>` with informative error types. The Dashboard decides how to surface a failure (status bar, modal, log). Panicking is reserved for genuine invariant violations.
- **Tests live where the behaviour does.** Component logic gets unit tests in-tree. Cross-component behaviour goes in `tests/` as integration tests. UI smoke tests use ratatui's test backend so they run headless in CI.

## Open questions

Decisions deferred. Each with a brief reason for the deferral.

- **Attention detection heuristics.** Exact rules for `needs-input` vs `working` vs `idle`. Likely "last transcript entry is an assistant message and N seconds of stillness have passed" for `needs-input`, but the exact predicates depend on transcript shape in practice. Deferred until M0 dogfooding produces real signal.
- **Session discovery vs explicit registration.** Should agent-mux auto-discover every transcript in `~/.claude/projects/` (potentially noisy if the user has dozens of old conversations), or only those touched recently, or only those the user explicitly registers? Leaning "recent + explicit override," but defer until M0 surfaces what's actually annoying.
- **Behaviour when agent-mux is launched from inside a tmux session.** `tmux switch-client` targeting a different session is fine; targeting a different window in the *same* session is also fine. But we have to decide whether agent-mux always assumes it owns its own tmux session, or whether it can attach to whatever the user is already in. Deferred to M0 implementation.
- **Claude Code hooks integration.** Claude Code exposes `Notification`, `Stop`, and other hook events. These would give richer attention signals than transcript tailing. Deferred until M0 transcript-based detection ships and we have a sense of where the gaps are.
- **Keybind config schema.** Action names plus key strings, but the exact key-string grammar (`ctrl+a` vs `C-a` vs the kitty-keyboard protocol's explicit codes) isn't pinned. Deferred to M3.
- **Theme schema.** Likely named colours mapping to ratatui `Style` values. Format details deferred to M3.
- **What to do when a session's tmux window is killed externally.** Drop the session, mark it as "dead," or try to relaunch? Deferred to M0 dogfooding.
