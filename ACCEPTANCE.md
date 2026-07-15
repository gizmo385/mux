# Acceptance

Acceptance criteria are tracked at the milestone level in `FEATURES.md` and as `#acceptance`-tagged items in `TODO.md`. Version tags (`vX.Y.Z`) are cut by release-plz from the conventional-commit log (see `PROCESS.md` § Versioning and releases) and are orthogonal to milestone acceptance — they reflect what's shipped, not what's been accepted.

## Multi-agent CLI milestone (2026-07-09/10)

The `AgentCli` extraction plus Codex and Pi as the first additional agents, shipped as WP0–WP9 per `docs/plans/2026-07-09-multi-agent-cli.md`. This milestone spans several work packages, so its acceptance is recorded here explicitly rather than folded into one `FEATURES.md` line.

Accepted:

- **Claude-only is unchanged.** With no `[agents]` table, discovery, watching, the disk cache, the dashboard, and the `n`/`N` flow are byte-identical to pre-milestone Claude-only behaviour (existing snapshot / `build_display_rows` / discovery tests pass unmodified).
- **Enabling an agent surfaces its sessions.** `[agents.<label>] enabled = true` makes that agent's sessions appear in the dashboard with correct cwd, title, attention state, and edited-files list, discovered from and attention-tracked through that agent's own transcript tree and semantics. A root that doesn't exist on a host is silently skipped.
- **Config precedence and validation.** Transcript-root resolution follows explicit per-host `[hosts.<name>.agents.<label>]` → legacy per-host `transcript_root` (claude alias) → global `[agents.<label>]` → agent default; unknown labels are rejected at load.
- **Spawn + resume per agent.** `n`/`N` spawns the selected agent and it surfaces as its own row: Pi via a pinned `--session-id` (identity contract intact); Codex via launch + post-spawn rollout adoption (no id pinning upstream). Resume with no live pane runs the agent's own resume command (`claude --resume` / `codex resume <id>` / `pi --session-id <id>`).
- **UI gates at ≥2 agents.** The per-row agent tag and quickswitcher tag, and the new-session agent-picker step, appear only when two or more agents are enabled; a single enabled agent suppresses both.
- **Codex hook parity.** `agent-mux install-hooks --agent codex` writes `~/.codex/hooks.json`; a Codex session blocked on an approval surfaces as `◆ blocked` within one poll tick via the same marker pipeline as the Claude Notification hook; installer is idempotent with `--dry-run`.

Dogfood-gated (not yet accepted — deferred pending real binaries):

- **Live end-to-end verification against real `codex` and `pi` binaries.** All Codex/Pi paths are unit-tested against synthetic fixtures only (no `codex`/`pi` on the build box as of 2026-07-10). Live verification of discovery, adoption, spawn/resume, and the Codex hook against real binaries is the top follow-up (`TODO.md`, "Other agent CLIs").
- **Codex `hooks.json` schema validated on a real install** (installer mirrors Claude Code's layout best-effort).
- **Pi hook-parity extension** (deferred — pi has no built-in gates, so it's a latency nicety, not a correctness gap).
