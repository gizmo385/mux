# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
