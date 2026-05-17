# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Status

M1 complete. M2 (remote hosts) substantially shipped: with `[hosts.<name>]` entries in the config, the dashboard surfaces sessions from SSH-reachable machines at startup, attention updates stream live (every 3s, over each host's existing `ControlMaster` connection), and `Enter` / `t` attach into the remote tmux. Remote portability and post-M5 remote session *creation* remain in `TODO.md`. M3 (inline preview) substantially shipped: `p` toggles a right-side pane that reads the selected session's recent transcript activity (user prompts, assistant prose, tool calls, tool results) without requiring an attach. M4 (attention notifications) shipped: when a session moves into `needs-input`, agent-mux fires an OS notification (libnotify on Linux, NSUserNotification on macOS) carrying the session's title and host. Per-episode and time-window suppression keep notification spam in check; user-facing on/off and quiet-hours knobs land in M5.

The dashboard runs (`cargo run`), discovers local Claude Code sessions, groups them under host headers (`── local ──`, then any configured SSH hosts alphabetical) with dim project sub-headers beneath each host, labels each row with its title (from `.agent-mux/task.toml` or Claude's auto-generated `aiTitle`, falling back to a short session-id suffix), shows live attention state (● needs-input, ◐ working, ○ idle), dims the title when no live tmux pane matches the session (Enter will spin up a fresh `claude --resume` rather than fast-switch into an existing pane), and:

- `↑`/`↓` or `j`/`k` — navigate the list
- `Enter` — switch into the tmux pane running the selected session; if there is no live pane, resume the conversation in a fresh `claude --resume` in the session's recorded cwd
- `t` — open a new tmux window in the session's cwd (or, outside tmux, drop into `$SHELL` in the cwd)
- `n` — create a new session: pick a repo, name a task, confirm the base branch. A git worktree is created alongside the parent repo and `claude` is launched in it.
- `/` — search/filter sessions by title, project directory, or host (case-insensitive substring). Type to narrow live, `Enter` to apply (keeps filter and returns focus to the list), `Esc` to clear and exit, `/` again to edit the active filter.
- `p` — toggle the preview pane: a right-side split showing the last entries of the selected session's transcript (your prompts, the assistant's prose, tool calls, and tool results) without attaching. Lazy-fetched per selection; cached so navigating back is instant; auto-invalidated when the transcript advances.
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

# Notification behaviour (M5). Every field has a default; the whole
# section is optional. Master toggle, audible cue, and per-host
# suppression are the M5-shipped knobs; quiet hours and customizable
# sound files are post-M5.
[notifications]
enabled = true          # master on/off
sound = false           # play the OS "default" notification sound
disabled_hosts = []     # host labels to silence entirely

# Theme overrides (M5). Each field is one colour. Empty string (or the
# literal "default") leaves the terminal default in place. Accepts
# named ANSI colours, `bright_*` variants, and `#RRGGBB` hex. Bad
# names fail loudly at config load.
[theme]
needs_input    = "red"     # ● glyph for needs-input sessions
working        = ""        # ◐ glyph for working sessions
idle           = ""        # ○ glyph for idle sessions
unknown        = ""        # · glyph for unknown sessions
tool_use       = "cyan"    # ⚒ Tool: … in preview
tool_result_ok = "green"   # ↳ ok in preview
tool_result_err = "red"    # ↳ error in preview
```

The `n` keybind picks from repos found in `workspace_folders` (depth-1 scan; tilde expansion only — env vars not yet supported).

### Remote sessions

Pressing `Enter` on a remote session runs `ssh -t <target> tmux attach -t <pane>` over the host's existing `ControlMaster` connection. The remote tmux's UI is what you interact with.

**Nested-tmux gotcha:** if you run agent-mux from *inside* a local tmux session and then attach to a remote, you end up with local-tmux > remote-tmux nesting, and both servers will see the same prefix key. To send a prefix to the inner (remote) tmux, send the local prefix twice (e.g. `C-b C-b` if your prefix is `C-b`) — that's the standard tmux passthrough convention. If you live inside tmux all day, the common fix is to configure the remote tmux to use a different prefix (e.g. `C-a`) so the keys don't collide at all.

If you run agent-mux from a bare shell, attaching to a remote is single-layer — agent-mux suspends, ssh runs in the same terminal, only the remote tmux is involved, no collision.

**When there's no remote pane to attach to:** if no remote tmux pane has `pane_current_path` matching the session's directory (e.g. the remote tmux server was restarted, or the original window was killed), agent-mux falls back to `tmux new-session -A -s agent-mux-<conv-id> -c <cwd> claude --resume <conv-id>` on the remote — creating a fresh remote tmux session named after the conversation, running `claude --resume`, and attaching the client. `-A` makes this idempotent: a second attach against the same conversation reuses the same remote tmux session rather than spawning a parallel `claude --resume` that would race on the transcript.

These `agent-mux-<id>` tmux sessions accumulate on the remote over time. Clean them up with `tmux kill-session -t agent-mux-<id>` when a conversation is truly done.

**Startup cache.** After each successful remote discovery, agent-mux writes a per-host snapshot to `~/.cache/agent-mux/sessions/<host>.json` (the list of remote sessions plus their last-known attention/title). On the next launch, those snapshots seed the dashboard immediately so configured remote hosts paint on first frame instead of popping in over the seconds it takes each `ControlMaster` handshake to complete. The live discovery still runs in the background and overlays fresh state when it finishes — entries that no longer exist on the remote drop out. Safe to delete the cache directory at any time; it'll repopulate.

## Setup

The Rust toolchain is pinned via `rust-toolchain.toml` (channel `1.94.0`). If you have rustup, it will auto-fetch this version on first `cargo` invocation; without rustup, any 1.94.x install satisfies the gate (CI uses exactly `1.94.0`). Pinning is what keeps the local pre-commit hook in sync with CI — bumps are deliberate.

After cloning, run `scripts/install-hooks.sh` once to install the pre-commit hook (fmt-check, clippy, tests, release-build — mirrors CI).

## How to run

`cargo run` to start the binary. `cargo build --release` for an optimised build. `cargo test` for the test suite. See `PROCESS.md` for the canonical-commands list.

## Pre-built binaries

Each push to `main` publishes fresh binaries to the [`latest` release](https://github.com/gizmo385/mux/releases/tag/latest):

- `agent-mux-aarch64-apple-darwin.tar.gz` — macOS, Apple Silicon
- `agent-mux-x86_64-unknown-linux-gnu.tar.gz` — Linux x86_64 (glibc)
- `agent-mux-x86_64-unknown-linux-musl.tar.gz` — Linux x86_64 (static, portable)

Install on macOS (Apple Silicon):

```sh
curl -L https://github.com/gizmo385/mux/releases/download/latest/agent-mux-aarch64-apple-darwin.tar.gz \
  | tar -xz -C /tmp \
  && install -m 755 /tmp/agent-mux /usr/local/bin/agent-mux
```

Tagged releases (when present) are the stable pin; `latest` tracks `main` and is overwritten on every push.

## Documents

- `SPEC.md` — what this project is.
- `ARCHITECTURE.md` — how it's built.
- `PROCESS.md` — how we work.
- `FEATURES.md` — what's shipped.
- `TODO.md` — what's planned.
- `ACCEPTANCE.md` — release gates.

## License

MIT
