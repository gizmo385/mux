//! WP4 integration coverage for the Pi read path: a synthetic
//! `~/.pi/agent/sessions`-shaped tree, discovered end-to-end through the pi
//! agent, must surface a `Session` with the right cwd, title, attention, and
//! edited files — proving the parser composes with discovery, not just its
//! unit branches.
//!
//! The session bodies below are **synthetic**, authored from the researched
//! Pi session schema (multi-agent plan Appendix B, researched 2026-07-09
//! against `@earendil-works/pi-coding-agent` v0.80.6) — no `pi` binary was
//! installed on the build machine to capture from (only an empty real
//! session directory existed, confirming the `--<encoded-cwd>--` shape).
//! WP2 already pinned the listing/routing plumbing; this file adds only the
//! new content-parse surface (full session assembly).

use std::fs;
use std::path::{Path, PathBuf};

use agent_mux::agent::AgentKind;
use agent_mux::discovery::discover;
use agent_mux::host::LocalHost;
use agent_mux::session::Attention;

/// Lay down a pi session tree under `tmp`: a real cwd directory (so the
/// discovery `is_dir` filter keeps the session) and a
/// `--<encoded-cwd>--/<ts>_<id>.jsonl` transcript with `body`.
fn write_session(tmp: &Path, cwd: &Path, id: &str, body: &str) {
    fs::create_dir_all(cwd).unwrap();
    // The bucket name is cosmetic for discovery (cwd comes from the header);
    // use a plausible encoded form anyway.
    let bucket = tmp.join(".pi").join("agent").join("sessions").join(format!(
        "--{}--",
        cwd.display()
            .to_string()
            .trim_start_matches('/')
            .replace('/', "-")
    ));
    fs::create_dir_all(&bucket).unwrap();
    let name = format!("2026-07-09T14-23-05-000_{id}.jsonl");
    fs::write(bucket.join(name), body).unwrap();
}

fn sessions_root(tmp: &Path) -> PathBuf {
    tmp.join(".pi").join("agent").join("sessions")
}

fn header(cwd: &Path) -> String {
    format!(
        r#"{{"type":"session","version":3,"id":"11111111","timestamp":"2026-07-09T14:23:05Z","cwd":"{}"}}"#,
        cwd.display(),
    )
}

#[test]
fn full_session_surfaces_with_cwd_title_attention_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work-proj");
    let id = "11111111-2222-3333-4444-555555555555";
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        header(&cwd),
        r#"{"type":"message","id":"u1","message":{"role":"user","content":"wire up the pi parser"}}"#,
        r#"{"type":"message","id":"a1","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"c1","name":"edit","arguments":{"path":"src/agents/pi.rs"}}]}}"#,
        r#"{"type":"message","id":"a2","message":{"role":"assistant","content":"done","stopReason":"stop"}}"#,
    );
    write_session(tmp.path(), &cwd, id, &body);

    let sessions = discover(&LocalHost::new(), &sessions_root(tmp.path()), AgentKind::Pi).unwrap();
    assert_eq!(sessions.len(), 1, "got: {sessions:?}");
    let s = &sessions[0];
    assert_eq!(s.agent, AgentKind::Pi);
    assert_eq!(s.id.0, id);
    assert_eq!(s.project_dir, cwd);
    // No session_info rename → title falls back to the first user message.
    assert_eq!(s.title.as_deref(), Some("wire up the pi parser"));
    // Last message is an assistant `stop` → the session awaits input.
    assert_eq!(s.attention, Attention::NeedsInput);
    // The relative edit path resolved against the header cwd.
    assert_eq!(s.edited_files, vec![cwd.join("src/agents/pi.rs")]);
}

#[test]
fn session_info_name_wins_over_first_user_message_as_title() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work-proj");
    let id = "abc123";
    let body = format!(
        "{}\n{}\n{}\n",
        header(&cwd),
        r#"{"type":"message","id":"u1","message":{"role":"user","content":"first prompt text"}}"#,
        r#"{"type":"session_info","id":"s1","name":"explicit session name"}"#,
    );
    write_session(tmp.path(), &cwd, id, &body);

    let sessions = discover(&LocalHost::new(), &sessions_root(tmp.path()), AgentKind::Pi).unwrap();
    assert_eq!(sessions.len(), 1, "got: {sessions:?}");
    assert_eq!(sessions[0].title.as_deref(), Some("explicit session name"));
}

#[test]
fn working_session_surfaces_working_attention() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work-proj");
    let id = "def456";
    let body = format!(
        "{}\n{}\n{}\n",
        header(&cwd),
        r#"{"type":"message","id":"u1","message":{"role":"user","content":"go do it"}}"#,
        r#"{"type":"message","id":"a1","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"text","text":"working"}]}}"#,
    );
    write_session(tmp.path(), &cwd, id, &body);

    let sessions = discover(&LocalHost::new(), &sessions_root(tmp.path()), AgentKind::Pi).unwrap();
    assert_eq!(sessions.len(), 1, "got: {sessions:?}");
    // Last message is an assistant `toolUse` → the agent is Working.
    assert_eq!(sessions[0].attention, Attention::Working);
}

#[test]
fn stillborn_session_without_title_or_user_message_is_filtered() {
    // A header-only session (real cwd, but no session_info name and no user
    // message) is the pi analog of the post-`/clear` stillborn case —
    // discovery drops it via the same signal-less filter every agent shares.
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work-proj");
    let id = "99999999-8888-7777-6666-555555555555";
    let body = format!("{}\n", header(&cwd));
    write_session(tmp.path(), &cwd, id, &body);

    let sessions = discover(&LocalHost::new(), &sessions_root(tmp.path()), AgentKind::Pi).unwrap();
    assert!(sessions.is_empty(), "got: {sessions:?}");
}
