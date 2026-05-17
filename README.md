# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Status

M1 complete. M2 (remote hosts) substantially shipped: with `[hosts.<name>]` entries in the config, the dashboard surfaces sessions from SSH-reachable machines at startup, attention updates stream live (every 3s, over each host's existing `ControlMaster` connection), and `Enter` / `t` attach into the remote tmux. Remote portability and post-M4 remote session *creation* remain in `TODO.md`.

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

### Remote sessions

Pressing `Enter` on a remote session runs `ssh -t <target> tmux attach -t <pane>` over the host's existing `ControlMaster` connection. The remote tmux's UI is what you interact with.

**Nested-tmux gotcha:** if you run agent-mux from *inside* a local tmux session and then attach to a remote, you end up with local-tmux > remote-tmux nesting, and both servers will see the same prefix key. To send a prefix to the inner (remote) tmux, send the local prefix twice (e.g. `C-b C-b` if your prefix is `C-b`) — that's the standard tmux passthrough convention. If you live inside tmux all day, the common fix is to configure the remote tmux to use a different prefix (e.g. `C-a`) so the keys don't collide at all.

If you run agent-mux from a bare shell, attaching to a remote is single-layer — agent-mux suspends, ssh runs in the same terminal, only the remote tmux is involved, no collision.

**When there's no remote pane to attach to:** if no remote tmux pane has `pane_current_path` matching the session's directory (e.g. the remote tmux server was restarted, or the original window was killed), agent-mux falls back to `tmux new-session -A -s agent-mux-<conv-id> -c <cwd> claude --resume <conv-id>` on the remote — creating a fresh remote tmux session named after the conversation, running `claude --resume`, and attaching the client. `-A` makes this idempotent: a second attach against the same conversation reuses the same remote tmux session rather than spawning a parallel `claude --resume` that would race on the transcript.

These `agent-mux-<id>` tmux sessions accumulate on the remote over time. Clean them up with `tmux kill-session -t agent-mux-<id>` when a conversation is truly done.

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
