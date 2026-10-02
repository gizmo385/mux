//! End-to-end contract for `agent-mux hook --agent codex` as Codex sees it.
//!
//! Codex parses a command hook's stdout as a JSON decision and treats exit
//! code 2 as "block" (for `Stop`, stderr is fed back to the model as a
//! continuation prompt). So the producer must print nothing on stdout and
//! exit 0 on every path — success, skipped event, and malformed payload.
//! These tests run the real binary with `HOME` pointed at a temp dir so the
//! codex transcript root (and its `.agent-mux-hooks/` marker dir) resolve
//! inside it.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run_hook(home: &Path, payload: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-mux"))
        .args(["hook", "--agent", "codex"])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("CODEX_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent-mux");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait_with_output().expect("wait agent-mux")
}

fn marker_count(home: &Path) -> usize {
    let dir = home.join(".codex/sessions/.agent-mux-hooks");
    std::fs::read_dir(dir).map_or(0, Iterator::count)
}

#[test]
fn permission_request_writes_marker_with_silent_stdout_and_exit_zero() {
    let home = tempfile::TempDir::new().unwrap();
    let out = run_hook(
        home.path(),
        r#"{"session_id":"cx-e2e","turn_id":"t1","cwd":"/w","hook_event_name":"PermissionRequest","tool_name":"exec_command"}"#,
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "stdout must stay empty: {out:?}");
    assert_eq!(marker_count(home.path()), 1);
}

#[test]
fn stop_is_silent_on_stdout_and_exits_zero() {
    let home = tempfile::TempDir::new().unwrap();
    let out = run_hook(
        home.path(),
        r#"{"session_id":"cx-e2e","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"done"}"#,
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "stdout must stay empty: {out:?}");
}

#[test]
fn malformed_payload_still_exits_zero_with_error_on_stderr_only() {
    let home = tempfile::TempDir::new().unwrap();
    let out = run_hook(home.path(), r#"{"hook_event_name":"Stop"}"#);
    assert_eq!(
        out.status.code(),
        Some(0),
        "never exit non-zero: exit 2 would block the codex stop"
    );
    assert!(out.stdout.is_empty(), "stdout must stay empty: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("codex hook failed"),
        "the failure is reported on stderr: {out:?}"
    );
    assert_eq!(marker_count(home.path()), 0);
}
