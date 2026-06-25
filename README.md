# agent-mux

A fast, terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

## Features

- **One dashboard for every Claude Code session you have running** — locally and on any SSH-reachable host you've configured. Sessions group by host, then by project. Each session is a two-line row: line 1 is the title; line 2 is a status line led by a coloured state icon + word — `✓ done` / `! blocked` / `◐ working` / `○ idle` — followed by how long it's been in that state and the task's age (e.g. `◐ working 18m · 45m old`). The icon + word is the attention signal: it's **bold and themed-coloured when the session wants you** (`! blocked` in red, `✓ done` in green by default), and dim when it doesn't (`working` / `idle`), so the ones needing attention pop at a glance; the word stays beside the glyph so the state still reads on the uncoloured `mono` palette. `blocked` (the agent is waiting on your answer to a permission/elicitation prompt) reads apart from `done` (finished its turn) — telling them apart requires the `Notification` hook (below), since the transcript alone can't. The `<n> old` age comes from agent-mux's task metadata, so it shows for sessions agent-mux created (it's the worktree's age, not active runtime); externally-started sessions show just the state + time-in-state.
- **Attach without leaving the dashboard.** Pressing Enter hosts the active session inside an embedded terminal pane next to the sidebar: you see tmux + Claude Code on the right, the live session list on the left, and the sidebar keeps updating other sessions' attention state while you work. `--no-embed` reverts to the legacy "take over the whole terminal" behaviour for users who prefer it.
- **Create new sessions from inside the dashboard.** Press `n` to pick a repo, name a task, and pick a base branch — agent-mux creates a git worktree under `<workspace>/.agent-mux-worktrees/`, writes task metadata, and launches `claude` inside it. Press `N` instead to skip the worktree step and open `claude` straight in the repo root (for quick exploratory chats where a fresh worktree would just be in the way). Either way, the new session lands in the embedded pane next to the sidebar, not a fullscreen handoff. Works equally for local and remote-host repos.
- **Search / filter** the session list by title, project, or host with `/`.
- **OS notifications** when a session moves into `needs-input` (libnotify on Linux, `osascript` on macOS, `wsl-notify-send.exe` on WSL).
- **Themes** — eight built-in palettes (`default`, `bright`, `mono`, `warm`, `cool`, `solarized`, `gruvbox`, `nord`), each colouring the state icons (green `done`, red `blocked`, amber `working`, …) plus the focus-border and selection highlight. Per-element overrides on top of any preset; `mono` is the fully-uncoloured option. Run `agent-mux themes` for a swatch preview.

## Keybinds

The dashboard discovers local Claude Code sessions, groups them under host headers (`── local ──`, then any configured SSH hosts alphabetical) with dim project sub-headers beneath each host. Each row leads with its title (from `.agent-mux/task.toml` or Claude's auto-generated `aiTitle`, falling back to a short session-id suffix); the title dims when no live tmux pane matches the session (Enter will spin up a fresh `claude --resume` rather than fast-switch into an existing pane).

- `↑`/`↓` or `j`/`k` — navigate the list
- `J`/`K` — jump to the next / previous **project**. Lands on the first session of the target project; wraps. No-op when only one project is on screen.
- `Ctrl-j`/`Ctrl-k` — jump to the next / previous top-level **section**: the Favorites group, the Tools group, or a host. The coarsest jump, one level up from `J`/`K`. Lands on the section's first row; wraps at the edges; no-op when only one section is on screen.
- `Ctrl-P` — **quickswitcher**: a fuzzy-jump modal over *everything* you can attach to — every session (most-recently-active first), running tool launches, and offline favorites. Type a few characters of the title, project, or host; the list ranks best-match-first (contiguous and word-start hits win). `Enter` attaches to the highlighted entry immediately; `↑`/`↓` (or `Ctrl-p`/`Ctrl-n`) move; `Esc` cancels. Pure in-memory filtering over a snapshot taken when the modal opens, so it never blocks on I/O. This is the "which window was that conversation in?" answer: one chord, a few letters, you're there. (Picking an offline favorite jumps to its placeholder and reports "waiting for host" rather than attaching, since there's no live session yet.)
- `Enter` — attach to the selected session inside an embedded terminal pane on the right side of the dashboard; the list collapses to a 40-column sidebar on the left while the pane has focus. Press `Ctrl-a Esc` to return focus to the sidebar without killing the PTY; press `Enter` on a different row to attach to that session instead. (`--no-embed` reverts to the legacy behaviour: `tmux switch-client` from inside tmux, `tmux attach` as a subprocess outside.)
- `t` — open `$SHELL` in the session's cwd, hosted inside the embedded pane. The terminal joins the Tools sidebar group (`⚒ terminal · <project> · [<host>]`) so you can navigate back to it any time without having to find the source session row again. Each press creates a new tools row; the row vanishes on `exit`.
- `n` — create a new session: pick a repo, name a task, confirm the base branch. A git worktree is created in `<workspace>/.agent-mux-worktrees/<repo>-<task>/` (a hidden sibling of the parent repo) and `claude` is launched in it. The picker pre-selects the repo of the currently-highlighted session (or at least its host) so spawning siblings stays in-context.
- `N` (Shift+n) — same picker, no worktree: pick a repo and `claude` launches in the repo root directly. No task name, no base branch, no `.agent-mux/task.toml`. For quick exploratory chats or attaching to a checkout where you already have live edits. The new session lands in the embedded pane the same way `n` does.
- `r` — rename the selected session. Opens an inline overlay above the footer; type the new name, `Enter` saves, `Esc` cancels. The override persists to `~/.cache/agent-mux/session_names.json` and survives restarts. Committing an empty string clears the override and lets the auto-derived title (`task.toml` task / Claude's `aiTitle` / first user message / cwd basename) take back over. A later-arriving AI title does not clobber your rename — once you named something, you meant it.
- `f` — toggle favorite for the selected session. Favorited sessions surface in a pinned `── favorites ──` group at the very top of the sidebar (recency desc, host label suffixed) so frequently-attended conversations stay in a stable position even as the activity-driven natural-tree ordering reshuffles. A `★` glyph marks both the pinned copy and the natural-tree copy. Favorites persist to `~/.cache/agent-mux/favorites.json` and survive restarts. A favorite whose live session isn't loaded yet — its remote host is still connecting after a restart, or is offline — renders as a dimmed *unconfirmed* placeholder (last-known title, `⋯` in place of activity) rather than vanishing; it turns into a normal row the moment discovery surfaces the session. Pressing `f` on a placeholder removes a favorite whose session is gone for good. Search narrowing also hides favorites (live or placeholder) that don't match the query. Inside the delete-worktree modal, `f` instead flips the force toggle (modal-local binding).
- `d` — delete the selected session's worktree (worktree-backed sessions only — sessions started outside a worktree are skipped with a status line). Opens a confirmation modal showing the task, path, and host plus a `[ ] force` toggle (`f` to flip on/off); `Enter` confirms, `Esc` cancels. Without force, git refuses on uncommitted changes — re-confirm with force on if that's what you want. The branch the worktree was on, the transcript file, and any remote `agent-mux-<id>` tmux session are left alone — clean those up yourself if you want them gone.
- Custom tool keybinds — add `[[tools]]` entries to `~/.config/agent-mux/config.toml` and the dashboard will dispatch user-defined keybinds that launch a terminal tool in the selected session's cwd. Same dispatch family as `t: terminal`: inside tmux a new window opens with the command; outside tmux the TUI suspends and runs the command directly. `{cwd}` and `{host}` in command tokens are substituted at fire time. Keys are validated against built-ins at load (collisions are rejected, not silently overridden). Example: a binding `key = "g"`, `command = ["lazygit"]` makes `g` open lazygit in the selected worktree.
- `/` — search/filter sessions by title, project directory, or host (case-insensitive substring). Type to narrow live, `Enter` to apply (keeps filter and returns focus to the list), `Esc` to clear and exit, `/` again to edit the active filter.
- `q` / Ctrl-C — quit (in sidebar focus only; inside the embedded pane, Ctrl-C interrupts the running child, the standard tty behaviour)

### Embedded pane

When you press Enter on a session, agent-mux spawns `tmux attach -t <pane>` (or `tmux new-session -A -s agent-mux-<id> claude --resume <id>` if no live pane matches) into a pseudoterminal hosted inside the dashboard's right pane. tmux + Claude Code still own the rendered content — agent-mux is just the surrounding window. Mouse capture and bracketed paste are enabled while embedded so clicks, scroll, and pastes flow through to the child; with `--no-embed`, mouse capture stays off so your terminal's native text-selection works as usual.

**Selecting text from the embedded pane.** Hold `Shift` while clicking or dragging — agent-mux drops Shift-mouse events so your host terminal (iTerm2, kitty, wezterm, Alacritty, …) can do native selection as if mouse capture weren't on. This is the same convention those terminals already implement at the OS level. For host terminals without that convention, or for selection over SSH where local selection won't reach the local clipboard, an in-app copy mode with OSC52 dispatch is filed in TODO.

**Mouse-wheel scrollback in the embedded pane.** agent-mux forwards SGR wheel events into the inner tmux, but tmux only acts on them when its own mouse mode is enabled. Add `set -g mouse on` to `~/.tmux.conf` (or `tmux set -g mouse on` for the current server) and the wheel will scroll the embedded pane's history naturally — tmux enters copy-mode on scroll-up and exits when you reach the live edge or press `q`/`Enter`. Without that setting, wheel events pass through to the running program (Claude Code), which doesn't have its own scrollback.

`n` (new session) uses the same embedded-pane path: agent-mux spawns claude into a detached tmux session (`tmux new-session -d -P -F '#{session_name}' -c <cwd> claude`), lets tmux pick the session name, then attaches the embedded pane to it. The dashboard later discovers the new session via the transcript watcher and a normal Enter on that row re-attaches by matching the pane's cwd.

Border style reflects focus: bold border = the embedded pane has the keyboard; dim border = the sidebar does. The sidebar's top title bar shows the session count and the movement keybind triplet (`j/k`, `J/K`, `⌃jk`); the bottom footer carries the action-verb keybinds (`⏎/t/n/N/r/f/d/q`) and any user-configured `[[tools]]` bindings.

`--no-embed` disables the embedded pane entirely and reverts to the legacy `tmux switch-client` / `SuspendAndRun` behaviour.

## Configuration

Optional `~/.config/agent-mux/config.toml`:

```toml
# Top-level workspace_folders must be absolute paths — they're fed to
# every host's scan (local + remotes that inherit), and a tilde here
# would bake in the local user's home for everyone. Tilde at the top
# level errors loudly at load. Use a per-host block (below) for
# tilde-relative paths.
workspace_folders = ["/home/gizmo/workspace", "/home/gizmo/code"]

# Remote hosts. Each table is one SSH-reachable machine whose Claude
# Code sessions show up alongside your local ones at startup. Per-host
# `workspace_folders` lets `n` create sessions on the remote; omit it
# to inherit the top-level list.
[hosts.alpenglow]
ssh = "alpenglow"  # ~/.ssh/config alias, or "user@host"
# transcript_root = "~/.claude/projects"  # default; tilde survives, remote shell expands
# workspace_folders = ["~/workspace"]     # per-host tildes preserved; remote shell expands

# Notification behaviour. Every field has a default; the whole section
# is optional. Quiet-hours and per-event sound customization are not
# yet supported.
[notifications]
enabled = true          # master on/off
sound = false           # play the OS "default" notification sound
disabled_hosts = []     # host labels to silence entirely
# sound_file = "/System/Library/Sounds/Tink.aiff"
                        # path to an audio file to play instead of the
                        # OS default. Must be an absolute path — tildes
                        # are rejected at load (consistent with the
                        # top-level `workspace_folders` rule).
                        # macOS uses `afplay` (handles mp3/wav/aiff/m4a);
                        # Linux tries ffplay then paplay. When set, takes
                        # precedence over sound=true and the OS
                        # notification itself stays silent so the file
                        # plays alone. Test it via `agent-mux notify-test`.
# backend = "auto"      # one of: auto, dbus, osascript, wsl-toast.
                        # auto picks per-OS at startup; explicit values
                        # override the probe. The picked backend is
                        # logged to stderr at startup so silent failures
                        # become visible in your scrollback.

# Theme overrides. Pick a built-in palette via `preset`, then optionally
# override individual fields. Each value is a string: named ANSI colour,
# `bright_*` variant, or `#RRGGBB` hex. Empty string (or the literal
# "default") clears that field — useful for subtracting a colour from a
# preset. Bad names fail loudly at load.
#
# Elements: needs_input (the green "done" accent), blocked (the red
# "answer me" accent; falls back to needs_input when unset), working,
# idle, unknown — plus two structural colours: focus_border (the focused
# pane's border, default cyan) and selection (the selected-row highlight
# background, default ANSI 238). The attention accents show in
# `agent-mux themes`; the structural ones are config-only.
#
# Built-in presets:
#   "default"   — green done / red blocked / amber working / dim idle (out-of-box colour).
#   "bright"    — high contrast; every attention state in bright_* variants.
#   "mono"      — no colours at all (modifiers like bold/dim still apply).
#   "warm"      — sunset palette: reds, ambers, earthy browns, olive done.
#   "cool"      — ocean palette: blues, teals, sea-green done (blocked stays rose).
#   "solarized" — canonical Solarized accents (works on dark or light bg).
#   "gruvbox"   — Gruvbox bright variants; earthy / retro on dark terminals.
#   "nord"      — Nord aurora + frost; slate tones with aurora-coloured events.
#
# With no `[theme]` section, the "default" preset applies.
[theme]
preset = "bright"
needs_input = "#5fd75f"    # override one field on top of the preset
blocked = "#ff5555"        # colour "answer me" apart from "done"

[ui]
sessions_per_project = 5   # cap rows per project; the rest collapse behind
                           # "+ K more". 0 = no cap. Lifted while searching.
```

Each project in the sidebar shows at most `[ui] sessions_per_project` of its most-recent sessions (default 5); any extras collapse behind a dim `+ K more` line. The cap keeps tall two-line rows from letting a busy project flood the sidebar — and it's lifted whenever you open search (`/`), so filtering always shows every match. Favorited sessions are unaffected (they're pinned at the top regardless).

The `n` keybind picks from repos found in `workspace_folders` (depth-1 scan). Top-level paths must be absolute; per-host `workspace_folders` accept tildes (the remote shell expands them against the remote user's home). Env-var expansion is not supported.

### Remote sessions

Pressing `Enter` on a remote session runs `ssh -t <target> tmux attach -t <pane>` over the host's existing `ControlMaster` connection — by default the resulting tmux client lives inside the embedded pane next to the sidebar; with `--no-embed` it takes over your whole terminal as a foreground subprocess. Either way, what you interact with is the remote tmux's UI.

**Prefix collisions when nesting tmux.** Running agent-mux from inside an outer tmux puts you in a multi-layer prefix situation:

- Outer tmux owns its prefix (default `C-b`) — your normal way to manage that session.
- agent-mux's embedded-pane leader is `C-a esc` — pressing `C-a` then `esc` returns focus to the sidebar without killing the inner tmux. `C-a` was picked to *avoid* colliding with tmux's `C-b` default.
- The tmux server inside the embedded pane (or the remote tmux you SSH-attached to) owns its own prefix.

If you've already remapped your outer or remote tmux to use `C-a`, that will collide with agent-mux's leader. Workarounds: remap one side (the most common move is configuring the remote tmux to use a separate prefix like `C-x`), or use the standard tmux passthrough — send the conflicting prefix twice. A leader-chord config knob is planned for users who can't avoid the collision.

**When there's no remote pane to attach to.** If no remote tmux pane has `pane_current_path` matching the session's directory (e.g. the remote tmux server was restarted, or the original window was killed), agent-mux falls back to `tmux new-session -A -s agent-mux-<conv-id> -c <cwd> claude --resume <conv-id>` on the remote — creating a fresh remote tmux session named after the conversation, running `claude --resume`, and attaching the client. `-A` makes this idempotent: a second attach against the same conversation reuses the same remote tmux session rather than spawning a parallel `claude --resume` that would race on the transcript.

These `agent-mux-<id>` tmux sessions accumulate on the remote over time. Clean them up with `tmux kill-session -t agent-mux-<id>` when a conversation is truly done.

**Startup cache.** After each successful remote discovery, agent-mux writes a per-host snapshot to `~/.cache/agent-mux/sessions/<host>.json` (the list of remote sessions plus their last-known attention/title). On the next launch, those snapshots seed the dashboard immediately so configured remote hosts paint on first frame instead of popping in over the seconds it takes each `ControlMaster` handshake to complete. The live discovery still runs in the background and overlays fresh state when it finishes — entries that no longer exist on the remote drop out. Safe to delete the cache directory at any time; it'll repopulate.

**Reconnect after sleep.** Each remote host's `ControlMaster` is a long-lived multiplexed SSH connection, but it doesn't survive a laptop suspend — after an overnight sleep the connection has dropped. agent-mux's background poller notices on its next 3-second tick and rebuilds the master automatically, so it's warm again before you switch into a remote session rather than making that switch pay a fresh SSH handshake. When a reconnect happens you'll see `agent-mux: re-established connection to host '<name>'` on stderr. A host that's genuinely unreachable (or whose proxy can't sustain a multiplexed master) backs off rather than retrying every tick — the retry window grows from 6s to a 60s cap — so a permanently-down host doesn't spew a fresh connection attempt every few seconds; it quietly retries on the backoff schedule until it's back.

### Claude Code `Notification` hook (recommended)

agent-mux's attention heuristic derives state from the transcript JSONL, but the transcript alone can't distinguish "the assistant is running a tool" from "the assistant is waiting for you to approve a permission prompt." Wiring up Claude Code's `Notification` hook closes that gap — Claude Code fires the hook exactly when it needs your attention (permission prompts, idle-awaiting-input), and agent-mux uses the hook event as the authoritative signal until the transcript advances.

Run once:

```
agent-mux install-hooks
```

That edits `~/.claude/settings.json` to register `agent-mux hook` as a Notification handler, pointing at the binary that ran the command (resolved via `current_exe()`). It's idempotent — re-running it is a no-op when the entry is already there, and it updates the path in place if you've moved or reinstalled the binary. Your existing settings (theme, permissions, other hook types) are preserved; a one-time backup lands at `~/.claude/settings.json.bak` before the first write. Use `--dry-run` to preview the change without writing.

Behind the scenes: the hook command writes a marker file under `<Claude Code transcripts root>/.agent-mux-hooks/` (typically `~/.claude/projects/.agent-mux-hooks/`); the running dashboard watches that directory and forces the affected session into `NeedsInput`, firing a notification. The hook's `notification_type` also drives the `blocked` vs `done` state word: `permission_prompt` and `elicitation_dialog` show as `blocked` ("answer me"), while an idle nudge and the transcript heuristic show `done`. Both fire the same notification.

To verify your wiring without waiting for a real permission prompt: run `agent-mux notify-test` to confirm the dispatcher fires end-to-end, then trigger any permission prompt in a Claude Code session and watch the dashboard.

### Remote hosts

The hook command is the same on remote machines. SSH to each remote host you've configured in `agent-mux`, install the binary somewhere on `$PATH`, and run `agent-mux install-hooks` once. Hook events fired by Claude Code on the remote write markers to the remote's `<transcript_root>/.agent-mux-hooks/`; the local dashboard's per-host SSH poller picks them up on its 3-second cadence (same connection it already uses for transcript polling — no extra round-trip cost).

Caveats: agent-mux must be on the remote's `$PATH` so `install-hooks` resolves the command correctly via `current_exe()`. If you rebuild or reinstall the binary on the remote, re-run `install-hooks` there to update the path; the installer is idempotent and only updates when needed.

## Setup

The Rust toolchain is pinned via `rust-toolchain.toml` (channel `1.94.0`). If you have rustup, it will auto-fetch this version on first `cargo` invocation; without rustup, any 1.94.x install satisfies the gate (CI uses exactly `1.94.0`).

After cloning, run `scripts/install-hooks.sh` once to install the pre-commit hook (fmt-check, clippy, tests, release-build — mirrors CI).

## How to run

`cargo run` to start the binary. `cargo build --release` for an optimised build. `cargo test` for the test suite.

## CLI subcommands and flags

`agent-mux` with no arguments launches the dashboard with the embedded pane enabled. Two read-only subcommands surface what's tunable without making you dig through this README:

- `agent-mux themes` — coloured browser of every built-in theme preset, each element rendered in its actual colour so you can pick a palette by eye before editing the config.
- `agent-mux config` — prints the current resolved config (which path was loaded, parsed `workspace_folders` / `hosts` / `notifications` / theme). Diagnostic-only — answers "is my config actually being read?" without log-spelunking.
- `agent-mux notify-test` — fires one test notification using the current config (backend + sound choices), useful for verifying your `sound_file` plays without provoking a real session transition.
- `agent-mux help` / `--help` — subcommand overview plus a one-screen reference of every config key, default, and accepted value.

Flags for the dashboard:

- `--no-embed` — disable the embedded PTY pane and revert to the legacy attach behaviour (`tmux switch-client` from inside tmux, `tmux attach` as a foreground subprocess outside). For users who prefer agent-mux to hand off the whole terminal rather than host a pane.

Stdout-detection: `themes` emits ANSI escapes only when stdout is a real terminal; piping to `less -R` works, piping to a non-`-R` pager or a file produces plain text.

## Pre-built binaries

Two release channels, both shipping the same artifacts:

- **Tagged (`vX.Y.Z`)** — stable, immutable. Cut by [release-plz](https://release-plz.ieni.dev/) from the conventional-commit log. Use these if you want predictable installs. Browse them at [releases](https://github.com/gizmo385/mux/releases).
- **`latest`** (rolling) — replaced on every push to `main`, marked as a prerelease. Use this if you want the bleeding edge.

Artifacts:

- `agent-mux-aarch64-apple-darwin.tar.gz` — macOS, Apple Silicon
- `agent-mux-x86_64-unknown-linux-gnu.tar.gz` — Linux x86_64 (glibc)
- `agent-mux-x86_64-unknown-linux-musl.tar.gz` — Linux x86_64 (static, portable)

Install a tagged release on macOS (Apple Silicon):

```sh
# Pick a version from https://github.com/gizmo385/mux/releases
VERSION=vX.Y.Z
curl -L "https://github.com/gizmo385/mux/releases/download/${VERSION}/agent-mux-aarch64-apple-darwin.tar.gz" \
  | tar -xz -C /tmp \
  && install -m 755 /tmp/agent-mux /usr/local/bin/agent-mux
```

Or substitute `latest` for `${VERSION}` to track `main`.

## Install via cargo

Published to [crates.io](https://crates.io/crates/agent-mux) alongside every tagged release:

```sh
cargo install agent-mux
```

Builds from source against your local toolchain; takes a couple of minutes. Requires Rust 1.94+ (matches the repo's `rust-toolchain.toml`). Re-run with `--force` to upgrade, or pin a version with `--version`.

## Install via nix flake

The repository is a flake. From another flake, add it as an input and reference `packages.<system>.default`:

```nix
{
  inputs.agent-mux.url = "github:gizmo385/mux";

  # in a home-manager / nixos module:
  # home.packages = [ inputs.agent-mux.packages.${pkgs.system}.default ];
}
```

Pinning is the consumer's `flake.lock` — `nix flake update agent-mux` to pull a new commit, otherwise the same SHA every rebuild.

For one-shot or non-flake usage:

- `nix run github:gizmo385/mux` — try it without installing.
- `nix profile install github:gizmo385/mux` — persistent install into the user profile; `nix profile upgrade` to pull a new build.
- `nix develop` inside a clone — shell with cargo/clippy/rustfmt/rust-analyzer.

Runtime dependencies (`tmux`, `ssh`, `git`, `claude`) are intentionally not in the closure — supply them from your normal environment.

## Project documents

These describe how the project is built and worked on, rather than how to use it — useful if you're contributing or just curious:

- `SPEC.md` — what this project is.
- `ARCHITECTURE.md` — how it's built.
- `PROCESS.md` — how we work.
- `FEATURES.md` — feature ledger by milestone.
- `TODO.md` — backlog.
- `ACCEPTANCE.md` — release gates.

## License

MIT
