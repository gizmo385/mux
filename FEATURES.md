# Features

Feature ledger. Grouped by milestone. One line per entry. Plain language.

Legend: ✓ shipped · ⋯ in progress

## Milestone — initial work

### M0 — Local dashboard

- ✓ ratatui app skeleton: header / sessions list / footer, `q` and Ctrl-C to quit
- ✓ local session discovery: scans `~/.claude/projects/`, reads `cwd` from transcript, uses file mtime as last-activity, renders a sortable list with arrow / `j`/`k` navigation
