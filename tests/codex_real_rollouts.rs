//! Real interactive-TUI Codex rollouts, pinned as fixtures.
//!
//! Both files under `tests/fixtures/codex/` were written by the real
//! `codex` 0.142.5 interactive TUI (not `codex exec`), captured 2026-10-02
//! inside an isolated tmux server against a localhost mock Responses
//! provider (no `OpenAI` traffic; the paths inside point at a throwaway
//! scratch dir). They pin what the synthetic fixtures could only assume:
//! the interactive writer uses the same `task_started` / `task_complete`
//! names as `exec`, an interrupted turn is spelled `turn_aborted` (never
//! renamed), `task_complete` persists `last_agent_message`, and an approved
//! `apply_patch` lands as a legacy-mode `patch_apply_end`. Read through the
//! same host-read orchestration the watcher uses, so the attention, edited
//! files, and turn-end toast body are proven end-to-end against real bytes.

use std::path::{Path, PathBuf};

use agent_mux::agent::AgentKind;
use agent_mux::host::LocalHost;
use agent_mux::session::Attention;
use agent_mux::watcher::derive_attention_detail;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex")
        .join(name)
}

#[test]
fn approved_patch_turn_completes_with_message_and_edit() {
    let d = derive_attention_detail(
        &LocalHost::new(),
        &fixture("codex-0.142.5-interactive-approved-patch-complete.jsonl"),
        AgentKind::Codex,
        Path::new(""),
    );
    assert_eq!(d.attention, Attention::NeedsInput);
    assert!(!d.from_tool_use);
    assert_eq!(d.last_message.as_deref(), Some("mock reply"));
    assert!(
        d.edited_files
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == "patched_by_mock.txt")),
        "the approved apply_patch edit is captured: {:?}",
        d.edited_files
    );
}

#[test]
fn declined_then_aborted_then_completed_turn_reads_as_the_last_turn() {
    // turn 1: approval declined + interrupted (`turn_aborted`); turn 2:
    // plain completion. The derivation follows the *last* turn, and the
    // toast body is turn 2's answer, not anything from the aborted turn.
    let d = derive_attention_detail(
        &LocalHost::new(),
        &fixture("codex-0.142.5-interactive-declined-aborted-then-complete.jsonl"),
        AgentKind::Codex,
        Path::new(""),
    );
    assert_eq!(d.attention, Attention::NeedsInput);
    assert_eq!(d.last_message.as_deref(), Some("mock reply"));
}
