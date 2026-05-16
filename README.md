# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Status

M0 complete (dogfooding phase). The dashboard runs (`cargo run`), discovers local Claude Code sessions, shows live attention state (● needs-input, ◐ working, ○ idle), and:

- `↑`/`↓` or `j`/`k` — navigate the list
- `Enter` — switch into the tmux pane running the selected session; if there is no live pane, resume the conversation in a fresh `claude --resume` in the session's recorded cwd
- `t` — open a new tmux window in the session's cwd (or, outside tmux, drop into `$SHELL` in the cwd)
- `q` / Ctrl-C — quit

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
