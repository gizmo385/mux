# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Status

M0 in progress. The dashboard runs (`cargo run`), discovers local Claude Code sessions from `~/.claude/projects/`, and lets you navigate the list with arrows / `j`/`k`. Next: live attention state and tmux attach.

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
