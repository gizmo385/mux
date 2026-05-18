# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Status

M1 complete. M2 (remote hosts) substantially shipped: with `[hosts.<name>]` entries in the config, the dashboard surfaces sessions from SSH-reachable machines at startup, attention updates stream live (every 3s, over each host's existing `ControlMaster` connection), and `Enter` / `t` attach into the remote tmux. Remote portability and post-M5 remote session *creation* remain in `TODO.md`. M3 (inline preview) substantially shipped: `p` toggles a right-side pane that reads the selected session's recent transcript activity (user prompts, assistant prose, tool calls, tool results) without requiring an attach. M4 (attention notifications) shipped: when a session moves into `needs-input`, agent-mux fires an OS notification (libnotify on Linux, NSUserNotification on macOS) carrying the session's title and host. Per-episode and time-window suppression keep notification spam in check; user-facing on/off and quiet-hours knobs land in M5.

The dashboard runs (`cargo run`), discovers local Claude Code sessions, groups them under host headers (`── local ──`, then any configured SSH hosts alphabetical) with dim project sub-headers beneath each host, labels each row with its title (from `.agent-mux/task.toml` or Claude's auto-generated `aiTitle`, falling back to a short session-id suffix), shows live attention state (● needs-input, ◐ working, ○ idle), dims the title when no live tmux pane matches the session (Enter will spin up a fresh `claude --resume` rather than fast-switch into an existing pane), and:

- `↑`/`↓` or `j`/`k` — navigate the list
- `J`/`K` — jump to the next / previous **project**. Lands on the first session of the target project; wraps. No-op when only one project is on screen.
- `Ctrl-j`/`Ctrl-k` — jump to the next / previous **host**. Same semantics as `J`/`K` one level up. No-op with only one host.
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

# Theme overrides (M5). Pick a built-in palette via `preset`, then
# optionally override individual fields. Each value is a string:
# named ANSI colour, `bright_*` variant, or `#RRGGBB` hex. Empty
# string (or the literal "default") clears that field — useful for
# subtracting a colour from a preset. Bad names fail loudly at load.
#
# Built-in presets:
#   "default"   — pre-M5 scheme: cyan/green/red preview, uncoloured glyphs.
#   "bright"    — high contrast; every attention state coloured,
#                 preview switches to bright_* variants.
#   "mono"      — no colours at all (modifiers like bold/dim still apply).
#   "warm"      — sunset palette: reds, ambers, earthy browns.
#   "cool"      — ocean palette: blues, teals, sea greens (errors stay rose).
#   "solarized" — canonical Solarized accents (works on dark or light bg).
#   "gruvbox"   — Gruvbox bright variants; earthy / retro on dark terminals.
#   "nord"      — Nord aurora + frost; slate tones with aurora-coloured events.
#
# With no `[theme]` section, the "default" preset applies.
[theme]
preset = "bright"
needs_input = "#ff5555"    # override one field on top of the preset
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

## CLI subcommands

`agent-mux` with no arguments launches the dashboard. Two read-only subcommands surface what's tunable without making the user dig through this README:

- `agent-mux themes` — coloured browser of every built-in theme preset, each element rendered in its actual colour so you can pick a palette by eye before editing the config.
- `agent-mux config` — prints the current resolved config (which path was loaded, parsed `workspace_folders` / `hosts` / `notifications` / theme) followed by a reference TOML skeleton documenting every key with its default. The status block answers "is my config actually being read?" without log-spelunking; the reference is copy-pasteable into `~/.config/agent-mux/config.toml`.
- `agent-mux help` / `--help` — subcommand overview.

Stdout-detection: `themes` emits ANSI escapes only when stdout is a real terminal; piping to `less -R` works, piping to a non-`-R` pager or a file produces plain text.

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

## Install via nix flake

The repository is a flake. From another flake, add it as an input and reference `packages.<system>.default`:

```nix
{
  inputs.agent-mux.url = "github:gizmo385/mux";

  # in a home-manager / nixos module:
  # home.packages = [ inputs.agent-mux.packages.${pkgs.system}.default ];
}
```

Pinning is the consumer's `flake.lock` — `nix flake update agent-mux` to pull a new commit, otherwise the same SHA every rebuild. `latest`-the-release staying mutable doesn't affect this path; the flake input resolves to a commit, not a release tag.

For one-shot or non-flake usage:

- `nix run github:gizmo385/mux` — try it without installing.
- `nix profile install github:gizmo385/mux` — persistent install into the user profile; `nix profile upgrade` to pull a new build.
- `nix develop` inside a clone — shell with cargo/clippy/rustfmt/rust-analyzer.

Runtime dependencies (`tmux`, `ssh`, `git`, `claude`) are intentionally not in the closure — supply them from your normal environment.

## Documents

- `SPEC.md` — what this project is.
- `ARCHITECTURE.md` — how it's built.
- `PROCESS.md` — how we work.
- `FEATURES.md` — what's shipped.
- `TODO.md` — what's planned.
- `ACCEPTANCE.md` — release gates.

## License

MIT
