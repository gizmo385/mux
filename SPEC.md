# Specification

What this project is. The authoritative description of scope and behaviour, distinct from `ARCHITECTURE.md` (which is about how it's built).

## Overview

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

The user keeps several Claude Code sessions running in parallel — some on the local workstation, some on remote SSH-reachable machines. agent-mux is the dashboard that tracks them all in one TUI: which projects they cover, which are awaiting input, what they're currently doing. Pressing Enter on a session drops the user into the real Claude Code TUI for that session; detaching returns them to the dashboard. The tool is a control plane on top of `tmux` and Claude Code's on-disk transcripts; it does not re-implement either.

The target user is a single developer who already lives in the terminal, already uses tmux, and runs Claude Code conversations frequently enough that "which window was that conversation in again?" is a real friction point.

## Glossary

- **Session** — one ongoing Claude Code conversation. Each session has an id, a project (working directory), a host, an optional task description, and an on-disk transcript.
- **Project** — the working directory Claude Code runs in for a session. Usually a git repository root or a worktree.
- **Worktree** — a git worktree dedicated to one session. From M1 onward, agent-mux creates and manages worktrees for sessions it spawns; sessions started externally may use any working directory.
- **Task** — the human-readable description of what a session is for ("refactor the parser"). Optional, persisted alongside the worktree for sessions agent-mux created.
- **Host** — where a session physically runs. Either `local` or a user-configured SSH target (an entry in `~/.ssh/config` or an explicit `user@host`).
- **Dashboard** — the agent-mux TUI: the list of all known sessions and their states.
- **Transcript** — the JSONL file Claude Code writes for each conversation (under `~/.claude/projects/<hash>/<conversation-id>.jsonl`). Source of truth for what a session is doing.
- **Attention state** — a session's current status, derived from its transcript: `needs-input`, `working`, or `idle`.
- **Attachment** — the mechanism by which the user interacts with a session. For M0–M4 this is a `tmux` window; the abstraction allows for other backends later.

## Functionality

What agent-mux does, in user-observable terms.

- **Dashboard view.** Lists all known sessions with project, host, attention state, and time since last activity. Sessions needing input are visually prominent.
- **Session discovery.** On startup, scans Claude Code's transcript directory for existing sessions and populates the dashboard. The user does not have to register sessions manually.
- **Spawn a new session (M1).** From the dashboard, the user names a task and picks a base branch; agent-mux creates a git worktree, starts `claude` inside it in a new tmux window, and registers the resulting session. The user is in the new session immediately.
- **Attach.** Pressing Enter on a session takes the user into that session's tmux window — they are now in the real Claude Code TUI and can type, paste, use slash commands, anything Claude Code supports. Detaching from tmux returns them to the dashboard.
- **Spawn terminal in session cwd.** A keybind opens a new tmux window in the session's working directory, so the user can run shell commands alongside Claude Code without leaving agent-mux's mental model.
- **Remote sessions (M2).** Sessions on configured SSH hosts appear in the same dashboard. Attaching SSH-tunnels into the remote tmux. Attention state for remote sessions is detected the same way as local — by watching the remote transcript file — over the existing SSH connection.
- **Attention notifications.** When a session moves from `working` or `idle` into `needs-input`, the dashboard updates and (eventually) the user receives an OS-level notification.
- **Inline preview (M3).** Each session row shows the last few transcript entries — recent tool calls, the most recent assistant message — without requiring the user to attach.

What agent-mux does not do.

- It does not render the full conversation in its own UI. The user always sees the real Claude Code TUI when interacting.
- It does not own panes, splits, scrollback, or copy mode. tmux does those.
- It does not start, configure, or update Claude Code itself.

## Roadmap

Milestones are ordered. Each builds on the previous. The constraint behind the early milestones is "ship something the user can dogfood within days, not weeks" — fast feedback informs whether Shape A is the right shape or whether to pivot toward B.

**M0 — Local dashboard.**
Goal: see and switch between local Claude Code conversations from one TUI.
Scope: dashboard TUI in ratatui; session discovery from `~/.claude/projects/`; attention detection from local transcript tailing; tmux-based attach; spawn-terminal-in-cwd action.
Out of scope for M0: session *creation* from inside agent-mux (user starts Claude Code the usual way), remote hosts, inline preview, configurable keybinds, themes.

**M1 — Session creation + worktree management.**
Goal: spawn new Claude Code sessions from inside agent-mux, each in its own git worktree. This is the capability that turns the dashboard from a *catalog* into an *orchestrator* and is the defining differentiator from M0's purely-passive dashboard.
Scope: new-session flow (task name, base branch, worktree location); Worktree Manager that creates and registers worktrees via `git worktree`; task metadata persisted in the worktree; tmux integration for launching `claude` inside the new worktree; lifecycle handling when a session is closed (worktree fate — to be decided in design).
Out of scope for M1: diff viewing, merge/discard workflow, remote session creation.

**M2 — Remote hosts.**
Goal: same dashboard experience for sessions on SSH targets. (Spawning new sessions on remote hosts is post-M2.)
Scope: host configuration file; SSH ControlMaster lifecycle; remote transcript polling; remote tmux attach via persistent SSH window.

**M3 — Inline preview.**
Goal: see recent transcript activity per session without entering. This is also the experiment that tells us whether richer chat rendering (Shape B/D) is worth pursuing. If reading a transcript line-by-line in the dashboard turns out to be the thing the user actually wants, that's signal to lean toward B.
Scope: transcript renderer (parses Claude Code JSONL into compact display lines); preview pane or per-row preview; configuration for preview verbosity.

**M4 — Customization.**
Goal: themes and custom keybinds.
Scope: TOML config schema for themes and keybindings; reload-on-edit; documented defaults.

**Post-M4.** Direction depends on M3 findings and dogfooding signal. Likely candidates: diff view (what an agent has changed against the base branch), merge / discard workflow for completed sessions, remote session creation, Claude Code hooks integration for richer attention signals. If the M3 preview experiment suggests a full chat surface is worth building, also plan a Shape B/D transition. The order will follow what M0–M4 dogfooding surfaces.

## Out of scope

- **Replacing tmux.** agent-mux runs on top of tmux through M4. A future post-M4 milestone might add an alternative backend, but tmux is the only attachment in scope for the foreseeable plan.
- **Replacing Claude Code.** agent-mux does not implement a chat UI in M0–M4. The user always interacts with the real Claude Code. agent-mux *spawns* Claude Code sessions in M1 but does not configure or update Claude Code itself.
- **Other agents.** Cursor, Aider, generic-LLM CLIs — none of these are in scope. Claude Code only.
- **Windows-native.** Targets Linux and macOS terminals. WSL is the only supported Windows path.
- **Collaboration features.** Single user, single dashboard instance. No shared state across users or machines (other than the user logging in to their own remote hosts).
- **Persistent state beyond what tmux and Claude Code already persist.** agent-mux's own state (dashboard view, focus, recent activity cache) is allowed to be ephemeral.
