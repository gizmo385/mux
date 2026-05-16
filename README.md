# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Status

M0 in progress. The dashboard runs (`cargo run`), discovers local Claude Code sessions from `~/.claude/projects/`, navigates with arrows / `j`/`k`, and shows live attention state (needs-input ●, working ◐, idle ○) driven by a `notify`-based transcript watcher. Next: tmux attach + spawn-terminal.

## Setup

After cloning, run `scripts/install-hooks.sh` once to install the pre-commit hook (fmt-check + clippy + tests).

## How to run

`cargo run` to start the binary. `cargo build --release` for an optimised build. `cargo test` for the test suite. See `PROCESS.md` for the canonical-commands list.

## Documents

- `SPEC.md` — what this project is.
- `ARCHITECTURE.md` — how it's built.
- `PROCESS.md` — how we work.
- `FEATURES.md` — what's shipped.
- `TODO.md` — what's planned.
- `ACCEPTANCE.md` — release gates.

## License

MIT
