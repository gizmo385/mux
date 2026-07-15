//! WP2 integration coverage: with `[agents.codex] enabled = true` and a
//! fake `~/.codex/sessions`-shaped tree, the config resolves codex on, and
//! discovery lists the codex tree *through the codex agent's* `ListingSpec`
//! (depth-4 `rollout-*.jsonl`) rather than the claude depth-2 shape.
//!
//! The codex content parser is still stubbed (WP3), so no `Session` is
//! assembled yet — this test pins the plumbing: root resolution, enablement,
//! and per-agent listing routing.

use std::fs;
use std::path::PathBuf;

use agent_mux::agent::{AgentKind, agent};
use agent_mux::config::Config;
use agent_mux::discovery::discover;
use agent_mux::host::{Host, LocalHost};

/// Write a config with codex enabled and return the parsed `Config`.
fn codex_enabled_config(dir: &std::path::Path) -> Config {
    let path = dir.join("config.toml");
    fs::write(&path, "[agents.codex]\nenabled = true\n").expect("write config");
    Config::load_from(&path).expect("parse config")
}

#[test]
fn codex_enabled_config_lists_and_routes_through_the_codex_agent() {
    let tmp = tempfile::tempdir().unwrap();

    // 1. Config: codex resolves enabled, in registry order after claude.
    let cfg = codex_enabled_config(tmp.path());
    assert_eq!(
        cfg.enabled_agents(),
        vec![AgentKind::Claude, AgentKind::Codex]
    );

    // 2. Fake `~/.codex/sessions` tree: a depth-4 rollout plus a
    //    claude-shaped depth-2 file the codex spec must ignore.
    let root = tmp.path().join(".codex").join("sessions");
    let day = root.join("2026").join("07").join("09");
    fs::create_dir_all(&day).unwrap();
    let rollout =
        day.join("rollout-2026-07-09T10-00-00-00000000-1111-2222-3333-444444444444.jsonl");
    fs::write(&rollout, "{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n").unwrap();
    let shallow = root.join("-encoded-cwd");
    fs::create_dir_all(&shallow).unwrap();
    fs::write(shallow.join("claude-shaped.jsonl"), "{}\n").unwrap();

    // 3. Listing through the codex agent's ListingSpec finds only the
    //    depth-4 rollout — proving discovery routes list_transcripts through
    //    the right per-agent shape.
    let host = LocalHost::new();
    let listed = host
        .list_transcripts(&root, &agent(AgentKind::Codex).listing())
        .unwrap();
    let paths: Vec<PathBuf> = listed.into_iter().map(|s| s.path).collect();
    assert_eq!(paths, vec![rollout]);

    // 4. discover() through the codex agent runs cleanly. The stub parser
    //    surfaces no cwd, so nothing is assembled yet (WP3 fills the parser).
    let sessions = discover(&host, &root, AgentKind::Codex).unwrap();
    assert!(
        sessions.is_empty(),
        "codex content parser is stubbed until WP3: {sessions:?}"
    );
}

#[test]
fn zero_config_is_claude_only() {
    // The load-bearing byte-identical guarantee at the integration layer:
    // an absent config resolves to exactly one enabled agent, claude.
    let cfg = Config::default();
    assert_eq!(cfg.enabled_agents(), vec![AgentKind::Claude]);
    assert_eq!(
        cfg.transcript_root_for(None, AgentKind::Claude),
        agent(AgentKind::Claude).default_transcript_root()
    );
}
