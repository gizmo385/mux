# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Status

M1 complete. M2 (remote hosts) partial: with `[hosts.<name>]` entries in the config, the dashboard surfaces sessions from SSH-reachable machines at startup. **Limitations until the next M2 chunks land:** remote-session attention is frozen at the startup reading (no live updates), and pressing Enter on a remote session does not yet attach into the remote tmux.

The dashboard runs (`cargo run`), discovers local Claude Code sessions, groups them under host headers (`── local ──`, then any configured SSH hosts alphabetical) with dim project sub-headers beneath each host, labels each row with its title (from `.agent-mux/task.toml` or Claude's auto-generated `aiTitle`, falling back to a short session-id suffix), shows live attention state (● needs-input, ◐ working, ○ idle), and:

- `↑`/`↓` or `j`/`k` — navigate the list
- `Enter` — switch into the tmux pane running the selected session; if there is no live pane, resume the conversation in a fresh `claude --resume` in the session's recorded cwd
- `t` — open a new tmux window in the session's cwd (or, outside tmux, drop into `$SHELL` in the cwd)
- `n` — create a new session: pick a repo, name a task, confirm the base branch. A git worktree is created alongside the parent repo and `claude` is launched in it.
- `q` / Ctrl-C — quit

## Configuration

Optional `~/.config/agent-mux/config.toml`:

```toml
workspace_folders = ["~/workspace", "~/code"]

# Remote hosts (M2 partial — see Status above).
# Each table is one SSH-reachable machine whose Claude Code
# sessions show up alongside your local ones at startup.
[hosts.alpenglow]
ssh = "alpenglow"  # ~/.ssh/config alias, or "user@host"
# transcript_root = "~/.claude/projects"  # default; tilde-expanded
```

The `n` keybind picks from repos found in `workspace_folders` (depth-1 scan; tilde expansion only — env vars not yet supported).

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
