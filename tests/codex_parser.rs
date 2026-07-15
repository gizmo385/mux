//! WP3 integration coverage for the Codex read path: a synthetic
//! `~/.codex/sessions`-shaped tree, discovered end-to-end through the codex
//! agent, must surface a `Session` with the right cwd, title, attention,
//! and edited files — proving the parser composes with discovery, not just
//! its unit branches.
//!
//! The rollout bodies below are **synthetic**, authored from the researched
//! Codex schema (multi-agent plan Appendix A, researched 2026-07-09 against
//! rust-v0.144.1) — no real `codex` installation was available to capture
//! from. WP2 already pinned the listing/routing plumbing in
//! `tests/multi_agent.rs`; this file adds only the new content-parse
//! surface (full session assembly), rather than re-testing the routing.

use std::fs;
use std::path::Path;

use agent_mux::agent::AgentKind;
use agent_mux::discovery::discover;
use agent_mux::host::LocalHost;
use agent_mux::session::Attention;

/// Lay down a codex session tree under `tmp`: a real cwd directory (so the
/// discovery `is_dir` filter keeps the session) and a dated rollout file
/// with `body`.
fn write_rollout(tmp: &Path, cwd: &Path, uuid: &str, body: &str) {
    fs::create_dir_all(cwd).unwrap();
    let day = tmp.join(".codex").join("sessions").join("2026/07/09");
    fs::create_dir_all(&day).unwrap();
    let name = format!("rollout-2026-07-09T14-23-05-{uuid}.jsonl");
    fs::write(day.join(name), body).unwrap();
}

fn sessions_root(tmp: &Path) -> std::path::PathBuf {
    tmp.join(".codex").join("sessions")
}

#[test]
fn legacy_rollout_surfaces_full_session() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work-proj");
    let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let session_meta = format!(
        r#"{{"timestamp":"2026-07-09T14:23:05Z","type":"session_meta","payload":{{"id":"{uuid}","cwd":"{}","originator":"cli","cli_version":"0.144.1","history_mode":"legacy"}}}}"#,
        cwd.display(),
    );
    let body = format!(
        "{session_meta}\n\
         {}\n{}\n{}\n{}\n",
        r#"{"type":"event_msg","payload":{"type":"user_message","payload":{"message":"wire up the codex parser"}}}"#,
        r#"{"type":"event_msg","payload":{"type":"turn_started","payload":{"turn_id":"1"}}}"#,
        r#"{"type":"event_msg","payload":{"type":"patch_apply_end","changes":{"src/agents/codex.rs":{"update":{"unified_diff":"@@"}}},"success":true}}"#,
        r#"{"type":"event_msg","payload":{"type":"turn_complete","payload":{"turn_id":"1"}}}"#,
    );
    write_rollout(tmp.path(), &cwd, uuid, &body);

    let sessions = discover(
        &LocalHost::new(),
        &sessions_root(tmp.path()),
        AgentKind::Codex,
    )
    .unwrap();
    assert_eq!(sessions.len(), 1, "got: {sessions:?}");
    let s = &sessions[0];
    assert_eq!(s.agent, AgentKind::Codex);
    assert_eq!(s.id.0, uuid);
    assert_eq!(s.project_dir, cwd);
    assert_eq!(s.title.as_deref(), Some("wire up the codex parser"));
    // Last turn event is `turn_complete` → the session awaits input.
    assert_eq!(s.attention, Attention::NeedsInput);
    // Path resolved against the session cwd (it was recorded relative).
    assert_eq!(s.edited_files, vec![cwd.join("src/agents/codex.rs")]);
}

#[test]
fn paginated_rollout_surfaces_full_session() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work-proj");
    let uuid = "11111111-2222-3333-4444-555555555555";
    let session_meta = format!(
        r#"{{"timestamp":"t","type":"session_meta","payload":{{"id":"{uuid}","cwd":"{}","history_mode":"paginated"}}}}"#,
        cwd.display(),
    );
    let body = format!(
        "{session_meta}\n\
         {}\n{}\n{}\n",
        r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"user_message","text":"paginated task"}}}"#,
        r#"{"type":"event_msg","payload":{"type":"turn_started","payload":{"turn_id":"1"}}}"#,
        r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"file_change","changes":{"README.md":{"add":{"content":"hi"}}},"status":"completed"}}}"#,
    );
    write_rollout(tmp.path(), &cwd, uuid, &body);

    let sessions = discover(
        &LocalHost::new(),
        &sessions_root(tmp.path()),
        AgentKind::Codex,
    )
    .unwrap();
    assert_eq!(sessions.len(), 1, "got: {sessions:?}");
    let s = &sessions[0];
    assert_eq!(s.id.0, uuid);
    assert_eq!(s.project_dir, cwd);
    assert_eq!(s.title.as_deref(), Some("paginated task"));
    // Open `turn_started` with no later completion → Working.
    assert_eq!(s.attention, Attention::Working);
    assert_eq!(s.edited_files, vec![cwd.join("README.md")]);
}

#[test]
fn stillborn_rollout_without_user_message_is_filtered() {
    // A rollout with a real cwd but no user message, no title, and no
    // task.toml is the codex analog of the post-`/clear` stillborn case —
    // discovery drops it (same filter every agent shares).
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work-proj");
    let uuid = "99999999-8888-7777-6666-555555555555";
    let body = format!(
        r#"{{"type":"session_meta","payload":{{"id":"{uuid}","cwd":"{}"}}}}"#,
        cwd.display(),
    );
    write_rollout(tmp.path(), &cwd, uuid, &body);

    let sessions = discover(
        &LocalHost::new(),
        &sessions_root(tmp.path()),
        AgentKind::Codex,
    )
    .unwrap();
    assert!(sessions.is_empty(), "got: {sessions:?}");
}
