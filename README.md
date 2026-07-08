# Agent Mux

Mux is a terminal-first multiplexer for managing multiple Claude Code conversations across local and remote hosts.

![Mux Screenshot](./images/mux_screenshot.png)

## Installation & Setup

Mux can be installed via Cargo:

```bash
cargo install agent-mux
```

Alternatively, you can run it via nix:
```bash
nix run github:gizmo385/mux
```

Or install it via nix:
```bash
nix profile install github:gizmo385/mux
```

To enable notifications, you'll want to install the Claude code hooks on your local machine and any
remote hosts that you want notifications on:

```bash
agent-mux install-hooks
```

## Keybinds

The dashboard discovers local Claude Code sessions, groups them under host headers (`── local ──`, then any configured SSH hosts alphabetical) with dim project sub-headers beneath each host. Each row leads with its title (from `.agent-mux/task.toml` or Claude's auto-generated `aiTitle`, falling back to a short session-id suffix); the title dims when no live tmux pane matches the session (Enter will spin up a fresh `claude --resume` rather than fast-switch into an existing pane).

| Key | Action |
| --- | --- |
| `↑`/`↓` or `j`/`k` | Navigate the list |
| `J`/`K` | Jump to next / previous **project** |
| `Ctrl-j`/`Ctrl-k` | Jump to next / previous top-level **section** (Favorites, Tools, or a host) |
| `Ctrl-P` | **Quickswitcher**: fuzzy-jump modal over everything attachable. Rows show each session's status (`! blocked`, `✓ done`, `◐ working`, `○ idle`) and float the ones that need you to the top; type to filter |
| `Enter` | Attach to the selected session in the embedded pane |
| `t` | Open `$SHELL` in the session's cwd inside the embedded pane |
| `n` | New  worktree session |
| `N` (Shift+n) | New non-worktree session |
| `r` | Rename the selected session |
| `f` | Toggle favorite. Favorites pin to a `── favorites ──` group at the top |
| `d` | Delete the selected worktree (worktree-backed sessions only) |
| `/` | Search/filter by title, project, or host (case-insensitive substring) |
| `q` / `Ctrl-C` | Quit  |

**Inside an attached session**, the terminal owns the keyboard; the `Ctrl-a` leader is the way back out: `Ctrl-a Esc` returns focus to the sidebar (the session stays alive), and `Ctrl-a p` opens the quickswitcher over the live session — `Esc` drops you back in, `Enter` attaches the picked one. (`Ctrl-a p` overrides an inner tmux's `prefix p`; use `Ctrl-a Ctrl-p` to pass a prefix through.)


## Configuration

Optional `~/.config/agent-mux/config.toml` — every section is optional.

```toml
# Repos scanned (depth-1) for the `n` picker. Absolute only; top-level tildes error at load.
workspace_folders = ["/home/gizmo/workspace", "/home/gizmo/code"]

# Remote hosts: one table per SSH-reachable machine, sessions shown alongside local.
[hosts.alpenglow]
ssh = "alpenglow"                          # ~/.ssh/config alias, or "user@host"
# transcript_root = "~/.claude/projects"   # default; remote shell expands the tilde
# workspace_folders = ["~/workspace"]      # per-host; tildes kept. Omit to inherit top-level

[notifications]
enabled = true          # master on/off
sound = false           # play the OS default notification sound
disabled_hosts = []     # host labels to silence
# sound_file = "/abs/path.aiff"  # play a file instead (absolute path). Test: agent-mux notify-test
# backend = "auto"      # auto | dbus | osascript | wsl-toast. Picked backend logged to stderr

[theme]
preset = "bright"       # default, bright, mono, warm, cool, solarized, gruvbox, nord
needs_input = "#5fd75f" # override a field: ANSI name, bright_* variant, or #RRGGBB. "" clears it
blocked = "#ff5555"

[ui]
sessions_per_project = 5   # rows per project; extras collapse behind "+ K more". 0 = no cap
```

`sessions_per_project` (default 5) is lifted while searching; favorites are always pinned regardless. Env-var expansion in paths is not supported.

### Themes

Presets set five attention accents (`needs_input`, `blocked`, `working`, `idle`, `unknown`) plus four structural colours (`focus_border`, `selection`, `background`, `sidebar_bg`). Override any field with an ANSI name, `bright_*` variant, or `#RRGGBB`; `""` (or `"default"`) clears it. Coloured presets ship a dark `background` + lighter `sidebar_bg`; `default`/`mono` leave both unset. The embedded terminal only picks up `background` in cells Claude Code leaves at terminal-default. Preview accents with `agent-mux themes`.

### Custom tool keybinds

Add `[[tools]]` entries to bind a key that launches a terminal tool in the selected session's cwd. `{cwd}`/`{host}` are substituted at fire time (and `{file}` marks a *file-scoped* tool — see below); collisions with built-in keys are rejected at load.

```toml
[[tools]]
key = "g"
command = ["lazygit"]   # opens lazygit in the selected worktree
```

**Open a file Claude edited.** If a tool's `command` references `{file}`, it becomes *file-scoped*: pressing its key opens a picker listing the files that session's Claude has edited (most-recently-edited first), and the file you pick is substituted for `{file}` when the tool launches. Fuzzy-filter by typing, `⏎` to open, `Esc` to cancel. The list is read from the conversation transcript.

```toml
[[tools]]
key = "e"
name = "edit"
command = ["vim", "{file}"]   # pick from the files Claude edited, open in vim
```

### Remote sessions

Pressing `Enter` on a remote session runs `ssh -t <target> tmux attach -t <pane>` over the host's existing `ControlMaster` connection — by default the resulting tmux client lives inside the embedded pane next to the sidebar; with `--no-embed` it takes over your whole terminal as a foreground subprocess. Either way, what you interact with is the remote tmux's UI.
