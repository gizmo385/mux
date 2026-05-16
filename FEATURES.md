# Features

Feature ledger. Grouped by milestone. One line per entry. Plain language.

Legend: ✓ shipped · ⋯ in progress

## Milestone — initial work

### M0 — Local dashboard

- ✓ ratatui app skeleton: header / sessions list / footer, `q` and Ctrl-C to quit
- ✓ local session discovery: scans `~/.claude/projects/`, reads `cwd` from transcript, uses file mtime as last-activity, renders a sortable list with arrow / `j`/`k` navigation
- ✓ live attention state: `notify`-driven transcript watcher derives state from JSONL tail (assistant → needs-input, user/tool result → working), with a 1h idle threshold applied in the UI layer
