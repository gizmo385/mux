# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.7](https://github.com/gizmo385/mux/compare/v0.1.6...v0.1.7) - 2026-07-08

### Added

- *(notifications)* richer toast content + attention-sorted quickswitcher
- *(tools)* open a Claude-edited file in your editor via {file}-scoped tools

### Fixed

- *(attachment)* cwd fallback skips other sessions' agent-mux panes
- *(attachment)* pin spawned sessions to agent-mux-<uuid> so same-dir sessions don't collide

### Other

- More README upates

## [0.1.6](https://github.com/gizmo385/mux/compare/v0.1.5...v0.1.6) - 2026-07-04

### Added

- *(embedded-pty)* surface failed attaches instead of flashing away

### Fixed

- *(ssh)* raise master-spawn ConnectTimeout to 15s for proxied hosts

### Other

- Rewrite README
- *(todo)* file ConnectTimeout=5 vs Coder proxy tunnel-establishment latency

## [0.1.5](https://github.com/gizmo385/mux/compare/v0.1.4...v0.1.5) - 2026-06-30

### Fixed

- *(attachment)* stop returning-to-session from re-opening a launched tool

## [0.1.4](https://github.com/gizmo385/mux/compare/v0.1.3...v0.1.4) - 2026-06-26

### Added

- *(ui)* widen the sidebar on Ctrl-a Esc
- *(ui)* sort favorites alphabetically, not by recency
- *(theme)* distinct sidebar panel via [theme] sidebar_bg
- *(theme)* themeable frame background + render-layer terminal harmonisation
- *(ui)* coloured state icons + done/blocked colour split + [theme] expansion
- *(ui)* extend Ctrl-j/Ctrl-k group-jump to Favorites and Tools sections
- *(ui)* quickswitcher fuzzy-jump modal (Ctrl-P)
- *(ui)* cap session rows per project with a "+ K more" overflow
- *(ui)* two-line session rows with state, time-in-state, total age
- *(attention)* distinguish blocked-on-a-prompt from done in sidebar
- *(favorites)* render offline favorites as placeholders, not gaps
- *(remote)* auto-reconnect a dead SSH ControlMaster

### Fixed

- *(theme)* paint session-pane background + rescue idle legibility
- *(attention)* protect blocked pin from tool_use clobber; recover done from oversized final message
- *(remote)* strip $TMUX from background ssh subprocesses
- *(remote)* back off reconnect/poll for an unsustainable master
- *(worktree)* keep .agent-mux/task.toml out of git

### Other

- *(ui)* dedent session blocks flush-left
- *(ui)* sidebar readability pass from dogfooding
- *(todo)* pull 2026-06-23 dogfooding batch to top as priority queue
- *(lint)* satisfy clippy 0.1.95 duration/map_or/into_iter lints

## [0.1.3](https://github.com/gizmo385/mux/compare/v0.1.2...v0.1.3) - 2026-05-30

### Added

- *(attachment)* t-terminal launches surface in Tools sidebar group
- *(discovery)* filter stillborn /clear transcripts at the discovery boundary

### Fixed

- *(attachment)* confirm before spawning a parallel claude --resume
- *(sidebar)* keep cursor on tool rows across re-seats
- *(discovery)* filter subagent transcripts from live watcher
- *(remote)* verify SshHost master actually established after spawn

### Other

- *(todo)* file persist-t-terminal-in-tools-sidebar idea
- *(todo)* refine duplicate-row-after-clear entry — stillborn rows
- *(todo)* file two-sessions-share-one-tmux-pane routing bug
- *(todo)* record root cause of stray-newline-on-swap repro
- *(embedded-pty)* drop /bin/true and /bin/false dependence
- *(main)* apply cargo fmt to pick_reseat_target
- *(todo)* file parallel-claude-resume on Enter against a live conversation
- *(todo)* file SshHost::connect silent-master-failure investigation

## [0.1.2](https://github.com/gizmo385/mux/compare/v0.1.1...v0.1.2) - 2026-05-27

### Added

- *(ui)* move movement keybinds to sidebar title bar; surface r/f in footer
- *(favorites)* ★ glyph render + `f` keybind + store load (steps 4-6)
- *(favorites)* DisplayRow plumbing + reseat disambiguation (step 2/3)
- *(favorites)* add FavoritesStore (step 1 of the spec)
- *(notifications)* deliver belated toast on focus-loss for actively-viewed sessions

### Fixed

- *(notifications)* suppress pre-launch attention toasts at startup
- *(ui)* pin selected session's project header on scroll
- *(dashboard)* re-seat sidebar selection after catalog drains
- *(notifications)* honour user rename override in notification titles

### Other

- *(todo)* resolve favorites spec from open-questions stub

## [0.1.1](https://github.com/gizmo385/mux/compare/v0.1.0...v0.1.1) - 2026-05-24

### Other

- *(process)* note the first-publish bootstrap requirement
