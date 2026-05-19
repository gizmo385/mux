# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Status

M0–M5 shipped; post-M5 work continues on the Shape B (embedded-PTY) dashboard and remote session creation. Highlights:

- **M1 (session creation).** `n` picks a repo, names a task, picks a base branch; agent-mux creates a git worktree under `<workspace>/.agent-mux-worktrees/` and launches `claude` inside it.
- **M2 (remote hosts).** Configured `[hosts.<name>]` entries surface in the dashboard at startup. Attention updates stream live (3s polling over each host's `ControlMaster`); `Enter` / `t` route through the host abstraction so attach + spawn-terminal work locally and remotely with the same dispatch. Remote `find` portability for macOS remotes is the lone open item — see `TODO.md`.
- **M3 (inline preview).** `p` toggles a right-side pane showing the selected session's recent transcript activity (prompts, assistant prose, tool calls, results) without attaching.
- **M4 (attention notifications).** OS notifications (libnotify on Linux, NSUserNotification on macOS) fire when a session transitions into `needs-input`. Per-episode and time-window suppression keep the spam in check.
- **M5 (customization).** `[notifications]` (`enabled`, `sound`, `disabled_hosts`) and `[theme]` (eight presets + per-element overrides) live in `~/.config/agent-mux/config.toml`. Run `agent-mux config` to see what's loaded; `agent-mux themes` for a coloured preview of every preset. Quiet-hours, keybind config, and reload-on-edit are deferred — see `TODO.md`.
- **Post-M5 remote session creation.** `n` against a remote-host repo creates the worktree on the remote and launches `claude` over SSH; the new session surfaces in the dashboard via the normal discovery pipeline.
- **Post-M5 embedded-PTY dashboard (Shape B), shipped 2026-05-18.** Pressing Enter hosts the active session inside a ratatui-rendered PTY widget alongside the dashboard sidebar instead of taking over the whole terminal. The sidebar stays live while you work — attention transitions on other sessions surface without detaching. `--no-embed` opts back into the legacy `tmux switch-client` flow.

The dashboard runs (`cargo run`), discovers local Claude Code sessions, groups them under host headers (`── local ──`, then any configured SSH hosts alphabetical) with dim project sub-headers beneath each host, labels each row with its title (from `.agent-mux/task.toml` or Claude's auto-generated `aiTitle`, falling back to a short session-id suffix), shows live attention state (● needs-input, ◐ working, ○ idle), dims the title when no live tmux pane matches the session (Enter will spin up a fresh `claude --resume` rather than fast-switch into an existing pane), and:

- `↑`/`↓` or `j`/`k` — navigate the list
- `J`/`K` — jump to the next / previous **project**. Lands on the first session of the target project; wraps. No-op when only one project is on screen.
- `Ctrl-j`/`Ctrl-k` — jump to the next / previous **host**. Same semantics as `J`/`K` one level up. No-op with only one host.
- `Enter` — attach to the selected session inside an embedded terminal pane on the right side of the dashboard; the list collapses to a 40-column sidebar on the left while the pane has focus. Press `Ctrl-a Esc` to return focus to the sidebar without killing the PTY; press `Enter` on a different row to attach to that session instead. (`--no-embed` reverts to the legacy behaviour: `tmux switch-client` from inside tmux, `tmux attach` as a subprocess outside.)
- `t` — open a new tmux window in the session's cwd (or, outside tmux, drop into `$SHELL` in the cwd)
- `n` — create a new session: pick a repo, name a task, confirm the base branch. A git worktree is created in `<workspace>/.agent-mux-worktrees/<repo>-<task>/` (a hidden sibling of the parent repo, so the workspace folder stays uncluttered) and `claude` is launched in it.
- `/` — search/filter sessions by title, project directory, or host (case-insensitive substring). Type to narrow live, `Enter` to apply (keeps filter and returns focus to the list), `Esc` to clear and exit, `/` again to edit the active filter.
- `p` — toggle the preview pane: a right-side split showing the last entries of the selected session's transcript (your prompts, the assistant's prose, tool calls, and tool results) without attaching. Lazy-fetched per selection; cached so navigating back is instant; auto-invalidated when the transcript advances. (Disabled while the embedded pane is active — the terminal *is* the preview, in HD.)
- `q` / Ctrl-C — quit (in sidebar focus only; inside the embedded pane, Ctrl-C interrupts the running child, the standard tty behaviour)

### Embedded pane (default)

When you press Enter on a session, agent-mux spawns `tmux attach -t <pane>` (or `tmux new-session -A -s agent-mux-<id> claude --resume <id>` if no live pane matches) into a pseudoterminal hosted inside the dashboard's right pane. tmux + Claude Code still own the rendered content — agent-mux is just the surrounding window. Mouse capture and bracketed paste are enabled while embedded so clicks, scroll, and pastes flow through to the child; outside embedded mode they're off so your terminal's native text-selection still works.

Border style reflects focus: bold border = the embedded pane has the keyboard; dim border = the sidebar does. Footer shows the relevant keybinds for current focus.

`--no-embed` disables the embedded pane and reverts to the M2-era `tmux switch-client` / `SuspendAndRun` behaviour for users who prefer it.

## Configuration

Optional `~/.config/agent-mux/config.toml`:

```toml
workspace_folders = ["~/workspace", "~/code"]

# Remote hosts (M2). Each table is one SSH-reachable machine whose
# Claude Code sessions show up alongside your local ones at startup.
# Per-host `workspace_folders` (post-M5) lets `n` create sessions on
# the remote; omit it to inherit the top-level workspaces.
[hosts.alpenglow]
ssh = "alpenglow"  # ~/.ssh/config alias, or "user@host"
# transcript_root = "~/.claude/projects"  # default; tilde-expanded
# workspace_folders = ["~/work"]          # post-M5; inherits top-level if omitted

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

Pressing `Enter` on a remote session runs `ssh -t <target> tmux attach -t <pane>` over the host's existing `ControlMaster` connection — by default the resulting tmux client lives inside the embedded pane next to the sidebar; with `--no-embed` it takes over your whole terminal as a foreground subprocess. Either way, what you interact with is the remote tmux's UI.

**Prefix collisions when nesting tmux.** Running agent-mux from inside an outer tmux puts you in a multi-layer prefix situation:

- Outer tmux owns its prefix (default `C-b`) — the user's normal way to manage their tmux session.
- agent-mux's embedded-PTY leader is `C-a esc` — pressing `C-a` then `esc` returns focus to the sidebar without killing the inner tmux. `C-a` was picked to *avoid* colliding with tmux's `C-b` default.
- The tmux server running inside the embedded pane (or the remote tmux you SSH-attached to) owns its own prefix.

If you've already remapped your outer or remote tmux to use `C-a`, that *will* collide with agent-mux's leader. Workarounds: remap one side (the most common move is configuring remote tmux to use a separate prefix like `C-x`), or use the standard tmux passthrough — send the conflicting prefix twice to pass it through to the inner tmux.

A leader-chord config knob is on the roadmap (`#post-m5 #embedded-pty` in `TODO.md`) for users who can't avoid the `C-a` collision.

**When there's no remote pane to attach to.** If no remote tmux pane has `pane_current_path` matching the session's directory (e.g. the remote tmux server was restarted, or the original window was killed), agent-mux falls back to `tmux new-session -A -s agent-mux-<conv-id> -c <cwd> claude --resume <conv-id>` on the remote — creating a fresh remote tmux session named after the conversation, running `claude --resume`, and attaching the client. `-A` makes this idempotent: a second attach against the same conversation reuses the same remote tmux session rather than spawning a parallel `claude --resume` that would race on the transcript.

These `agent-mux-<id>` tmux sessions accumulate on the remote over time. Clean them up with `tmux kill-session -t agent-mux-<id>` when a conversation is truly done.

**Startup cache.** After each successful remote discovery, agent-mux writes a per-host snapshot to `~/.cache/agent-mux/sessions/<host>.json` (the list of remote sessions plus their last-known attention/title). On the next launch, those snapshots seed the dashboard immediately so configured remote hosts paint on first frame instead of popping in over the seconds it takes each `ControlMaster` handshake to complete. The live discovery still runs in the background and overlays fresh state when it finishes — entries that no longer exist on the remote drop out. Safe to delete the cache directory at any time; it'll repopulate.

## Setup

The Rust toolchain is pinned via `rust-toolchain.toml` (channel `1.94.0`). If you have rustup, it will auto-fetch this version on first `cargo` invocation; without rustup, any 1.94.x install satisfies the gate (CI uses exactly `1.94.0`). Pinning is what keeps the local pre-commit hook in sync with CI — bumps are deliberate.

After cloning, run `scripts/install-hooks.sh` once to install the pre-commit hook (fmt-check, clippy, tests, release-build — mirrors CI).

## How to run

`cargo run` to start the binary. `cargo build --release` for an optimised build. `cargo test` for the test suite. See `PROCESS.md` for the canonical-commands list.

## CLI subcommands and flags

`agent-mux` with no arguments launches the dashboard in embedded-PTY mode (post-M5 default). Two read-only subcommands surface what's tunable without making the user dig through this README:

- `agent-mux themes` — coloured browser of every built-in theme preset, each element rendered in its actual colour so you can pick a palette by eye before editing the config.
- `agent-mux config` — prints the current resolved config (which path was loaded, parsed `workspace_folders` / `hosts` / `notifications` / theme) followed by a reference TOML skeleton documenting every key with its default. The status block answers "is my config actually being read?" without log-spelunking; the reference is copy-pasteable into `~/.config/agent-mux/config.toml`.
- `agent-mux help` / `--help` — subcommand overview.

Flags for the dashboard:

- `--no-embed` — disable the embedded PTY pane and revert to the legacy attach behaviour (`tmux switch-client` from inside tmux, `tmux attach` as a foreground subprocess outside). For users who prefer agent-mux to hand off the whole terminal rather than host a pane.

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
