# Specification

What this project is. The authoritative description of scope and behaviour, distinct from `ARCHITECTURE.md` (which is about how it's built).

## Overview

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

The user keeps several Claude Code sessions running in parallel — some on the local workstation, some on remote SSH-reachable machines. agent-mux is the dashboard that tracks them all in one TUI: which projects they cover, which are awaiting input, what they're currently doing. Pressing Enter on a session brings the active session up inside an embedded terminal pane next to the dashboard list; the user sees their existing tmux + Claude Code session and can type, paste, slash-command, anything Claude Code supports. The dashboard sidebar stays visible while the user works in the embedded pane — see other sessions' attention state in real time without detaching. The tool is a control plane on top of `tmux` and Claude Code's on-disk transcripts; it does not re-implement either.

The target user is a single developer who already lives in the terminal, already uses tmux, and runs Claude Code conversations frequently enough that "which window was that conversation in again?" is a real friction point.

## Glossary

- **Session** — one ongoing Claude Code conversation. Each session has an id, a project (working directory), a host, an optional task description, and an on-disk transcript.
- **Project** — the working directory Claude Code runs in for a session. For sessions agent-mux created (M1+) this is usually a worktree of a known Repo — except for the `N` no-worktree flow (post-M5), where agent-mux spawns directly in the repo root. For sessions started externally, it can be any directory.
- **Repo** — a git repository discovered by scanning workspace folders. The unit of organisation for agent-mux-created sessions: every such session lives in a worktree of exactly one Repo. Sessions whose `project_dir` does not match any known Repo are still rendered, just without a repo label.
- **Workspace folder** — a directory the user has designated as containing one or more git repos. agent-mux scans these to populate the Repo Registry. Configured in `~/.config/agent-mux/config.toml` (`workspace_folders = [...]`).
- **Worktree** — a git worktree dedicated to one session. From M1 onward, agent-mux creates and manages worktrees for sessions it spawns via the default `n` flow; sessions started externally — or spawned via the post-M5 no-worktree `N` flow — may use any working directory.
- **Task** — the human-readable description of what a session is for ("refactor the parser"). Optional, persisted alongside the worktree for sessions agent-mux created.
- **Host** — where a session physically runs. Either `local` or a user-configured SSH target (an entry in `~/.ssh/config` or an explicit `user@host`).
- **Dashboard** — the agent-mux TUI: the list of all known sessions and their states.
- **Transcript** — the JSONL file Claude Code writes for each conversation (under `~/.claude/projects/<hash>/<conversation-id>.jsonl`). Source of truth for what a session is doing.
- **Attention state** — a session's current status, derived from its transcript: `needs-input`, `working`, or `idle`.
- **Attachment** — the mechanism by which the user interacts with a session. For M0–M5 this was a foreground `tmux` window (the user's whole terminal took the session's place); post-M5 the default is an embedded PTY pane inside the dashboard, hosting the same `tmux attach` invocation but as a child process rather than a screen handoff. The `AttachmentDriver` trait abstracts both modes so callers don't know the difference; users select via `--no-embed`.

## Functionality

What agent-mux does, in user-observable terms.

- **Dashboard view.** Lists all known sessions with project, host, attention state, and time since last activity. Sessions needing input are visually prominent.
- **Session discovery.** On startup, scans Claude Code's transcript directory for existing sessions and populates the dashboard. The user does not have to register sessions manually.
- **Repo discovery (M1).** At startup, agent-mux scans each configured workspace folder one level deep for git repositories and caches the result in the Repo Registry. The new-session picker reads from this cache rather than re-scanning on every keystroke.
- **Spawn a new session (M1).** From the dashboard, the user picks a repo from the registry, names a task, and picks a base branch; agent-mux creates a git worktree, starts `claude` inside it, and registers the resulting session. Post-M5, the new session lands in the embedded pane next to the sidebar (same path as a normal attach) rather than a fullscreen handoff. The user is in the new session immediately. Worktree-backed sessions group under their parent repo's header in the dashboard (resolved from each cwd's `.git` pointer file at discovery time), so a session in `discord` and one in a `discord-<task>` worktree share one project group instead of fragmenting into per-worktree entries. The new-session picker pre-selects the repo (or at least the host) of the currently-highlighted session so spawning a sibling stays in-context.
- **Spawn a new session without a worktree (post-M5).** A sibling flow bound to `N` (Shift+n): the user picks a repo, and agent-mux spawns `claude` directly in the repo root — no `git worktree add`, no task name, no base branch, no `.agent-mux/task.toml`. For quick exploratory chats or attaching to a checkout the user already has live edits in. Same picker UI as `n` (mode marked in the modal title); same embedded-pane delivery.
- **Attach.** Pressing Enter on a session opens the active session inside an embedded terminal pane occupying the right side of the dashboard; the dashboard list collapses to a compact ~40-column sidebar on the left. Inside the pane the user sees their real tmux + Claude Code session and can type, paste, use slash commands, anything Claude Code supports — agent-mux is just hosting the terminal tmux runs in. The sidebar updates live while the embedded pane has focus, so the user can see attention transitions on other sessions without detaching. `Ctrl-a Esc` returns focus to the sidebar without killing the PTY; pressing Enter on a different row drops the old PTY and attaches to the new session. `--no-embed` opts back into the legacy `tmux switch-client` / `SuspendAndRun` flow for users who prefer it.
- **Spawn terminal in session cwd.** A keybind opens a new tmux window in the session's working directory, so the user can run shell commands alongside Claude Code without leaving agent-mux's mental model.
- **Remote sessions (M2).** Sessions on configured SSH hosts appear in the same dashboard. Attaching opens an `ssh -t target tmux attach …` inside the embedded pane (or as a foreground subprocess with `--no-embed`) — same dispatch as local. Attention state for remote sessions is detected the same way as local: by watching the remote transcript file over the existing SSH connection. Post-M5: `n` against a remote-host repo creates the worktree on the remote and spawns `claude` there, surfacing the new session in the dashboard via the normal discovery pipeline.
- **Attention notifications.** When a session moves from `working` or `idle` into `needs-input`, the dashboard updates and (eventually) the user receives an OS-level notification.
- **Inline preview (M3).** Each session row shows the last few transcript entries — recent tool calls, the most recent assistant message — without requiring the user to attach.

What agent-mux does not do.

- It does not render the conversation in its own UI. The user always sees the real tmux + Claude Code TUI when interacting; the embedded pane just hosts the terminal that tmux runs in, and the content is whatever tmux paints.
- It does not implement panes-within-tmux, splits, scrollback, or copy mode. Those remain tmux's job — even with embedded attach, `prefix [`, `prefix %`, `prefix "`, etc. work as they always have, against the tmux server running inside the embedded pane.
- It does not start, configure, or update Claude Code itself.

## Roadmap

Milestones are ordered. Each builds on the previous. The constraint behind the early milestones is "ship something the user can dogfood within days, not weeks" — fast feedback informs whether Shape A is the right shape or whether to pivot toward B.

**M0 — Local dashboard.**
Goal: see and switch between local Claude Code conversations from one TUI.
Scope: dashboard TUI in ratatui; session discovery from `~/.claude/projects/`; attention detection from local transcript tailing; tmux-based attach; spawn-terminal-in-cwd action.
Out of scope for M0: session *creation* from inside agent-mux (user starts Claude Code the usual way), remote hosts, inline preview, configurable keybinds, themes.

**M1 — Session creation + worktree management.**
Goal: spawn new Claude Code sessions from inside agent-mux, each in its own git worktree. This is the capability that turns the dashboard from a *catalog* into an *orchestrator* and is the defining differentiator from M0's purely-passive dashboard.
Scope: minimal workspace-folders config (`~/.config/agent-mux/config.toml` with `workspace_folders = [...]`); Repo Registry that scans those folders at startup and caches the result in memory; new-session flow (pick repo → task name → base branch); Worktree Manager that creates and registers worktrees via `git worktree`; task metadata persisted in the worktree; tmux integration for launching `claude` inside the new worktree; lifecycle handling when a session is closed (worktree fate — to be decided in design).
Out of scope for M1: diff viewing, merge/discard workflow, remote session creation, the broader M5 config surface (themes, keybinds, reload-on-edit).

**M2 — Remote hosts.**
Goal: same dashboard experience for sessions on SSH targets. (Spawning new sessions on remote hosts is post-M2.)
Scope: host configuration file; SSH ControlMaster lifecycle; remote transcript polling; remote tmux attach via persistent SSH window.

**M3 — Inline preview.**
Goal: see recent transcript activity per session without entering. This is also the experiment that tells us whether richer chat rendering (Shape B/D) is worth pursuing. If reading a transcript line-by-line in the dashboard turns out to be the thing the user actually wants, that's signal to lean toward B.
Scope: transcript renderer (parses Claude Code JSONL into compact display lines); preview pane or per-row preview; configuration for preview verbosity.

**M4 — Attention notifications.**
Goal: surface a session transitioning into `needs-input` even when agent-mux isn't on screen. Closes the promise in this document's Functionality section ("(eventually) the user receives an OS-level notification").
Scope: cross-platform OS notifications (Linux libnotify, macOS NSUserNotification) via `notify-rust`, triggered at the catalog's attention-update boundary when the previous state was `Working`/`Idle`/`Unknown` and the new state is `NeedsInput`; one notification per transition carrying the session's title and host label; minimal in-process suppression at implementation time — debounce against rapid attention flapping, and a per-session "I've seen this, hush" so the notification doesn't re-fire on every transition while the user is mid-decision.
Out of scope for M4: detection of agent-mux's terminal having focus (defer to dogfooding — may not be reliably detectable across terminal emulators); user-facing config knobs (on/off, sound, quiet hours, per-host suppression) which belong in M5's broader config surface alongside themes and keybinds.

**M5 — Customization.**
Goal: themes and custom keybinds.
Scope: TOML config schema for themes and keybindings; reload-on-edit; documented defaults. Also exposes the previously-hardcoded thresholds (idle threshold, remote poll interval) and the M4 notification suppression knobs (on/off, sound, quiet hours, per-host suppression).

**Post-M5.** Two arcs landed off the back of M5 dogfooding:

- **Remote session creation.** `n` against a remote-host repo creates the worktree on the remote (via the `Host` trait's `run` + `write_file` primitives) and spawns `claude` over SSH. Surfaces in the dashboard through normal discovery.
- **Shape B — embedded-PTY dashboard.** The M3 inline-preview experiment surfaced enough signal — "see other sessions while interacting with one" — to justify the Shape B transition the `AttachmentDriver` trait was designed to allow. The default attach now hosts a pseudoterminal inside the dashboard's TUI rather than handing off the whole terminal; tmux still runs as before, just inside our pane. `--no-embed` opts back into the legacy flow.

Still on the table: diff view (what an agent has changed against the base branch), merge / discard workflow for completed sessions, Claude Code hooks integration for richer attention signals, embedded-PTY polish (F5+ key codes, leader-chord config knob, mouse-on-sidebar). Order will follow what dogfooding surfaces.

## Out of scope

- **Replacing tmux.** agent-mux runs on top of tmux. Post-M5's embedded-PTY mode hosts the terminal that tmux runs in, but tmux still owns panes-within-tmux, scrollback, copy mode, and session persistence across agent-mux restarts. A no-tmux Shape B (PTY hosting `claude` directly) is *possible* given the embedded infrastructure but would lose persistence and remote ergonomics; not in scope for the foreseeable plan.
- **Replacing Claude Code.** agent-mux does not implement a chat UI. The user always interacts with the real Claude Code through tmux — the embedded pane just hosts the terminal tmux + Claude Code run in. agent-mux *spawns* Claude Code sessions (M1 locally, post-M5 remotely) but does not configure or update Claude Code itself.
- **Other agents.** Cursor, Aider, generic-LLM CLIs — none of these are in scope. Claude Code only.
- **Windows-native.** Targets Linux and macOS terminals. WSL is the only supported Windows path.
- **Collaboration features.** Single user, single dashboard instance. No shared state across users or machines (other than the user logging in to their own remote hosts).
- **Persistent state beyond what tmux and Claude Code already persist.** agent-mux's own state (dashboard view, focus, recent activity cache) is allowed to be ephemeral.
