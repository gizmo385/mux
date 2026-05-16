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
