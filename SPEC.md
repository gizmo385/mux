# Specification

What this project is. The authoritative description of scope and behaviour, distinct from `ARCHITECTURE.md` (which is about how it's built).

## Overview

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

The user keeps several Claude Code sessions running in parallel — some on the local workstation, some on remote SSH-reachable machines. agent-mux is the dashboard that tracks them all in one TUI: which projects they cover, which are awaiting input, what they're currently doing. Pressing Enter on a session drops the user into the real Claude Code TUI for that session; detaching returns them to the dashboard. The tool is a control plane on top of `tmux` and Claude Code's on-disk transcripts; it does not re-implement either.

The target user is a single developer who already lives in the terminal, already uses tmux, and runs Claude Code conversations frequently enough that "which window was that conversation in again?" is a real friction point.

## Glossary

- **Session** — one ongoing Claude Code conversation. Each session has an id, a project (working directory), a host, and an on-disk transcript.
- **Project** — the working directory Claude Code runs in for a session. Usually a git repository root or a worktree.
- **Host** — where a session physically runs. Either `local` or a user-configured SSH target (an entry in `~/.ssh/config` or an explicit `user@host`).
- **Dashboard** — the agent-mux TUI: the list of all known sessions and their states.
- **Transcript** — the JSONL file Claude Code writes for each conversation (under `~/.claude/projects/<hash>/<conversation-id>.jsonl`). Source of truth for what a session is doing.
- **Attention state** — a session's current status, derived from its transcript: `needs-input`, `working`, or `idle`.
- **Attachment** — the mechanism by which the user interacts with a session. For M0–M3 this is a `tmux` window; the abstraction allows for other backends later.

## Functionality

What agent-mux does, in user-observable terms.

- **Dashboard view.** Lists all known sessions with project, host, attention state, and time since last activity. Sessions needing input are visually prominent.
- **Session discovery.** On startup, scans Claude Code's transcript directory for existing sessions and populates the dashboard. The user does not have to register sessions manually.
- **Attach.** Pressing Enter on a session takes the user into that session's tmux window — they are now in the real Claude Code TUI and can type, paste, use slash commands, anything Claude Code supports. Detaching from tmux returns them to the dashboard.
- **Spawn terminal in session cwd.** A keybind opens a new tmux window in the session's working directory, so the user can run shell commands alongside Claude Code without leaving agent-mux's mental model.
- **Remote sessions.** Sessions on configured SSH hosts appear in the same dashboard. Attaching SSH-tunnels into the remote tmux. Attention state for remote sessions is detected the same way as local — by watching the remote transcript file — over the existing SSH connection.
- **Attention notifications.** When a session moves from `working` or `idle` into `needs-input`, the dashboard updates and (eventually) the user receives an OS-level notification.
- **Inline preview (M2).** Each session row shows the last few transcript entries — recent tool calls, the most recent assistant message — without requiring the user to attach.

What agent-mux does not do.

- It does not render the full conversation in its own UI. The user always sees the real Claude Code TUI when interacting.
- It does not own panes, splits, scrollback, or copy mode. tmux does those.
- It does not start, configure, or update Claude Code itself.

## Roadmap

Milestones are ordered. Each builds on the previous. The constraint behind the early milestones is "ship something the user can dogfood within days, not weeks" — fast feedback informs whether Shape A is the right shape or whether to pivot toward B.

**M0 — Local dashboard.**
Goal: see and switch between local Claude Code conversations from one TUI.
Scope: dashboard TUI in ratatui; session discovery from `~/.claude/projects/`; attention detection from local transcript tailing; tmux-based attach; spawn-terminal-in-cwd action.
Out of scope for M0: session creation from inside agent-mux (user starts Claude Code the usual way), remote hosts, inline preview, configurable keybinds, themes.

**M1 — Remote hosts.**
Goal: same experience for sessions on SSH targets.
Scope: host configuration file; SSH ControlMaster lifecycle; remote transcript polling; remote tmux attach via persistent SSH window.

**M2 — Inline preview.**
Goal: see recent transcript activity per session without entering. This is also the experiment that tells us whether richer chat rendering (Shape B/D) is worth pursuing. If reading a transcript line-by-line in the dashboard turns out to be the thing the user actually wants, that's signal to lean toward B.
Scope: transcript renderer (parses Claude Code JSONL into compact display lines); preview pane or per-row preview; configuration for preview verbosity.

**M3 — Customization.**
Goal: themes and custom keybinds.
Scope: TOML config schema for themes and keybindings; reload-on-edit; documented defaults.

**Post-M3.** Direction depends on M2 findings. If the preview experiment suggests a full chat surface is worth building, plan a Shape B/D transition. If the dashboard alone is enough, focus on polish, attention-heuristic quality, and possibly Claude Code hooks integration for richer signals.

## Out of scope

- **Replacing tmux.** agent-mux runs on top of tmux through M3. A future post-M3 milestone might add an alternative backend, but tmux is the only attachment in scope for the foreseeable plan.
- **Replacing Claude Code.** agent-mux does not implement a chat UI in M0–M3. The user always interacts with the real Claude Code.
- **Other agents.** Cursor, Aider, generic-LLM CLIs — none of these are in scope. Claude Code only.
- **Windows-native.** Targets Linux and macOS terminals. WSL is the only supported Windows path.
- **Collaboration features.** Single user, single dashboard instance. No shared state across users or machines (other than the user logging in to their own remote hosts).
- **Persistent state beyond what tmux and Claude Code already persist.** agent-mux's own state (dashboard view, focus, recent activity cache) is allowed to be ephemeral.
