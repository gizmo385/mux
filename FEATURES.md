# Features

Feature ledger. Grouped by milestone. One line per entry. Plain language.

Legend: ✓ shipped · ⋯ in progress

## Milestone — initial work

### M0 — Local dashboard

- ✓ ratatui app skeleton: header / sessions list / footer, `q` and Ctrl-C to quit
- ✓ local session discovery: scans `~/.claude/projects/`, reads `cwd` from transcript, uses file mtime as last-activity, renders a sortable list with arrow / `j`/`k` navigation
- ✓ live attention state: `notify`-driven transcript watcher derives state from JSONL tail (assistant → needs-input, user/tool result → working), with a 1h idle threshold applied in the UI layer
- ✓ tmux attach: `Enter` switches to the tmux pane whose cwd matches the selected session; `t` opens a new tmux window in that cwd; failures surface in a dashboard status line
- ✓ outside-tmux fallback: when run outside tmux, `Enter` suspends the TUI to run `tmux attach -t <target>` as a foreground subprocess; `t` drops into `$SHELL` in the cwd. TUI resumes when the subprocess exits.
- ✓ resume from transcript: when the selected session has no live tmux pane (e.g. its old window was killed, or there's no tmux server at all), `Enter` spawns a fresh `claude --resume <session-id>` in the session's recorded cwd — new tmux window inside tmux, or new tmux session outside tmux.

### M1 — Session creation + worktree management

- ✓ minimal config: `~/.config/agent-mux/config.toml` with `workspace_folders = [...]`. Tilde expansion; missing file tolerated (empty list).
- ✓ repo registry: depth-1 scan of each workspace folder at startup, identifying real repos (children with `.git/` as a directory). Worktree pointers are excluded so agent-mux-created worktrees don't litter the picker alongside their parent repos.
- ✓ WorktreeManager: `git worktree add -b <slug> <path> <base>` alongside the parent repo (`<repo>/../<repo>-<slug>`); writes `.agent-mux/task.toml` with task / base_branch / created_at.
- ✓ default base-branch resolver: `git symbolic-ref --short refs/remotes/origin/HEAD` → fall back to `main`, then `master`, then prompt with no default.
- ✓ new-session modal: press `n`, pick a repo, name a task, confirm/override the base branch (pre-filled from the resolver). Worktree creation runs on a background thread so the UI never blocks; the footer shows "creating worktree for…" while in flight. On success, `claude` launches in a new tmux window via the existing AttachmentDriver, handing the terminal off via SuspendAndRun outside tmux. **Limitation:** the new session does not appear in the dashboard until agent-mux is restarted — visibility wire-up is tracked as a follow-up in TODO.
- ✓ session titles: each row leads with a human-readable title when available, with cwd dimmed alongside. Precedence: `.agent-mux/task.toml`'s `task` field (for agent-mux-created sessions) → Claude Code's `aiTitle` transcript entry (auto-generated after a few messages) → fallback to cwd-only. Distinguishes multiple sessions sharing a directory.
- ✓ return-to-dashboard hint: footer keybind line ends with a mode-aware `return: prefix+s` (inside tmux — switch-client took the client to the session's window, `prefix+s` lists sessions to switch back) or `return: prefix+d` (outside tmux — agent-mux suspended itself to run `tmux attach` as a subprocess, `prefix+d` detaches it and the TUI resumes).
- ✓ repo registry TTL refresh: opening the new-session picker triggers a re-scan if the cached snapshot is older than 30s, so repos cloned mid-session appear without an agent-mux restart. The depth-1 walk is cheap enough to run synchronously; the cache prevents repeated walks during rapid open/close.
- ✓ live session discovery: the transcript watcher does a single recursive watch on `~/.claude/projects/` instead of per-file watches, so new transcripts (whether agent-mux just spawned them, or `claude` was started externally) surface in the dashboard as soon as their first JSONL line is on disk. New rows append at the tail so the user's current selection is preserved.
- ✓ stale-session filter: transcripts whose recorded `cwd` no longer exists on disk are dropped at discovery time, so deleted worktrees (e.g. cleaned-up claude-squad worktrees) stop cluttering the list with un-attachable entries. Also filters out the legacy "no cwd metadata" case where the fallback decoded-dir-name path doesn't exist.
