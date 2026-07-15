# Agent Mux

Mux is a terminal-first multiplexer for managing multiple terminal-agent conversations across local and remote hosts. Claude Code is the reference agent and the only one enabled without configuration; OpenAI Codex and Pi are supported as opt-in additional agents (see [Other agents](#other-agents-codex-pi)).

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

If you've enabled Codex (`[agents.codex]`), install its lifecycle hooks too — this is what surfaces
Codex's "blocked on approval" state, which is otherwise invisible in its transcript (writes
`~/.codex/hooks.json`; idempotent; `--dry-run` previews):

```bash
agent-mux install-hooks --agent codex
```

## Keybinds

The dashboard discovers local agent sessions (Claude Code by default; Codex/Pi when enabled), groups them under host headers (`── local ──`, then any configured SSH hosts alphabetical) with dim project sub-headers beneath each host. Each row leads with its title (from `.agent-mux/task.toml` or the agent's own title record — Claude's auto-generated `aiTitle`, Pi's `session_info` name, Codex's first user message — falling back to a short session-id suffix); the title dims when no live tmux pane matches the session (Enter will spin up a fresh resume of that agent rather than fast-switch into an existing pane).

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

**Open a file the agent edited.** If a tool's `command` references `{file}`, it becomes *file-scoped*: pressing its key opens a picker listing the files that session's agent has edited (most-recently-edited first), and the file you pick is substituted for `{file}` when the tool launches. Fuzzy-filter by typing, `⏎` to open, `Esc` to cancel. The list is read from the conversation transcript.

```toml
[[tools]]
key = "e"
name = "edit"
command = ["vim", "{file}"]   # pick from the files the agent edited, open in vim
```

### Remote sessions

Pressing `Enter` on a remote session runs `ssh -t <target> tmux attach -t <pane>` over the host's existing `ControlMaster` connection — by default the resulting tmux client lives inside the embedded pane next to the sidebar; with `--no-embed` it takes over your whole terminal as a foreground subprocess. Either way, what you interact with is the remote tmux's UI.

## Other agents (Codex, Pi)

agent-mux is a multiplexer for terminal agent CLIs that persist tail-parseable transcripts to disk and resume a session by id. Claude Code is the reference agent (the richest integration, on by default); **OpenAI Codex** and **Pi** are supported as opt-in additional agents. Enable one and its sessions appear in the same dashboard — discovered, attention-tracked, attachable, and spawnable through the same flows as Claude.

### What works per agent

| Capability | Claude | Codex | Pi |
| --- | --- | --- | --- |
| Discovery (cwd, title) | ✓ `~/.claude/projects` | ✓ `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | ✓ `~/.pi/agent/sessions/--<cwd>--/<ts>_<id>.jsonl` |
| Attention (working / needs-input) | ✓ `stop_reason` | ✓ `turn_started`/`turn_complete`/`turn_aborted` | ✓ `stopReason` |
| Edited-files picker | ✓ Edit/Write/… | ✓ `patch_apply_end` + `FileChange` (apply_patch edits only) | ✓ `write`/`edit` tool calls |
| Spawn (`n`/`N`) | ✓ pinned `--session-id` | ✓ launch + post-spawn adoption | ✓ pinned `--session-id` |
| Resume (no live pane) | ✓ `claude --resume` | ✓ `codex resume <id>` | ✓ `pi --session-id <id>` |
| Blocked-on-approval | ✓ Notification hook | ✓ lifecycle hook (`install-hooks --agent codex`) | n/a (no built-in gates) |

### Enabling an agent

Other agents are opt-in via an `[agents.<label>]` table (valid labels: `claude`, `codex`, `pi` — an unknown label errors at load). With no `[agents]` table at all, behaviour is exactly as before: Claude only.

```toml
[agents.codex]
enabled = true                     # codex/pi default off; claude defaults on
# binary = "codex"                 # PATH override for the agent binary
# transcript_root = "~/.codex/sessions"  # override the default root (local: tilde expands)

[agents.pi]
enabled = true
# binary = "pi"
# transcript_root = "~/.pi/agent/sessions"

[agents.claude]
# enabled = false                  # turn the reference agent off if you only want others
```

Per-host overrides live under `[hosts.<name>.agents.<label>]` with a `transcript_root` key — e.g. a remote where Codex lives somewhere non-default:

```toml
[hosts.gpu.agents.codex]
transcript_root = "/srv/codex/sessions"
```

The transcript-root precedence, highest first: explicit per-host `[hosts.<name>.agents.<label>]` → the legacy per-host `transcript_root` key (a backward-compatible alias for the claude entry) → global `[agents.<label>]` → the agent default. A root whose directory doesn't exist is silently skipped — the directory's presence is the "installed on this host" signal.

When two or more agents are enabled, the dashboard marks each session row (and quickswitcher entry) with a dim agent tag (`claude`/`codex`/`pi`), and the `n` / `N` new-session flow adds an agent-selection step (default = the first enabled agent). With a single enabled agent — the default Claude-only setup — both are suppressed, so the UI is pixel-identical to Claude-only.

### Setup: Codex hooks

Codex's "blocked on approval" state is never written to its transcript, so without a hook a blocked Codex session reads as *working*. Install Codex's lifecycle hooks — on your local machine and every remote host that runs Codex — to surface it (writes `~/.codex/hooks.json`; idempotent; `--dry-run` previews):

```bash
agent-mux install-hooks --agent codex
```

Pi has no built-in permission gates, so it needs no hook for parity; a lower-latency extension is a possible future add.

### Documented gaps

Per-agent capability limits at this release. They are documented here rather than papered over in the UI:

- **Codex `.jsonl.zst` cold rollouts are skipped.** A background worker zstd-compresses cold Codex rollouts; discovery skips `.jsonl.zst` siblings. They're weeks old — far outside the 30-day hot set — and an append re-materialises a plain `.jsonl`, so this only hides long-dormant sessions.
- **Codex shell-command edits are uncaptured.** The edited-files picker sees Codex edits made through `apply_patch` (`patch_apply_end` / `FileChange`), not files a Codex turn changed via a raw shell command.
- **Codex blocked-on-approval requires the hook.** Without `install-hooks --agent codex`, a Codex session waiting on an approval reads as *working* (the approval prompt is never persisted to the rollout). The hook is the only signal for it.
- **Pi permission gates are invisible to the tail.** Pi has no built-in gates; any an extension adds are not written to the transcript, so they don't surface as *blocked*.
- **Codex/Pi are not yet verified live end-to-end.** As of 2026-07-10 the Codex and Pi paths are tested against synthetic fixtures authored from format research (Codex rust-v0.144.1 and Pi v0.80.6, researched 2026-07-09); they have not been run against real `codex`/`pi` binaries. Treat Codex/Pi support as current-release best-effort.
- **Codex `hooks.json` schema is best-effort.** The installer mirrors Claude Code's proven hook layout; the exact Codex `hooks.json` schema is pending validation on a real install.
- **Transcript-root relocation env vars are partially honoured.** Pi's `PI_CODING_AGENT_SESSION_DIR` / `PI_CODING_AGENT_DIR` are honoured for the local process; Codex's `$CODEX_HOME` relocation is *not* auto-detected. For either, the supported cross-host path is the `transcript_root` config override.

The `[agents.<label>] binary` PATH override is read by discovery but is **not yet consumed by spawn/resume** — a new session always launches the bare `claude`/`codex`/`pi` on `PATH` (tracked as a follow-up).
