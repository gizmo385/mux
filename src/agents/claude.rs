//! Claude Code — the reference [`AgentCli`] implementation. Every
//! Claude-specific concern lives here: the `~/.claude/projects` tree
//! shape, the transcript-JSONL parser family (attention, title, edited
//! files), the `--session-id` spawn / `--resume` argv, and session-id
//! derivation from the transcript stem.

use std::path::{Path, PathBuf};

use crate::agent::{AgentCli, AgentDerivation, AgentKind, ListingSpec, SpawnPlan, TranscriptMeta};
use crate::session::{Attention, EDITED_FILES_CAP, SessionId};

/// Unit-struct [`AgentCli`] for Claude Code. Registered as a `&'static` in
/// [`crate::agent::agent`]; carries no state.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeAgent;

impl AgentCli for ClaudeAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn label(&self) -> &'static str {
        "claude"
    }

    fn default_binary(&self) -> &'static str {
        "claude"
    }

    fn default_transcript_root(&self) -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".claude").join("projects"))
    }

    fn listing(&self) -> ListingSpec {
        // `root/<project-hash>/<session-id>.jsonl` — files exactly two
        // levels below the root. Reproduces the pre-trait hardcoded
        // depth-2 `read_dir` walk (local) and `find -mindepth 2 -maxdepth
        // 2 -type f -name '*.jsonl'` (remote) byte-for-byte.
        ListingSpec {
            mindepth: 2,
            maxdepth: 2,
            name_glob: "*.jsonl",
        }
    }

    fn is_transcript(&self, path: &Path, root: &Path) -> bool {
        is_top_level_transcript(path, root)
    }

    fn session_id_from_path(&self, path: &Path) -> Option<SessionId> {
        Some(SessionId(path.file_stem()?.to_str()?.to_string()))
    }

    fn fallback_dir(&self, transcript_path: &Path) -> PathBuf {
        fallback_dir(transcript_path)
    }

    fn parse_meta(&self, content: &str) -> TranscriptMeta {
        parse_transcript_meta(content)
    }

    fn derive(&self, content: &str, _cwd: &Path) -> AgentDerivation {
        derive_from_content(content)
    }

    fn spawn(&self, _cwd: &Path, minted_id: &SessionId) -> SpawnPlan {
        // Claude pins a caller-chosen id: the minted uuid is the tmux
        // session-name suffix, the `--session-id`, and the transcript stem
        // all at once — today's identity contract. The Attachment Driver
        // wraps this tail in its `tmux new-session … -c <cwd>` shell.
        SpawnPlan::PinnedId {
            argv: vec![
                "claude".to_string(),
                "--session-id".to_string(),
                minted_id.0.clone(),
            ],
        }
    }

    fn resume_command(&self, id: &SessionId) -> String {
        format!("claude --resume {}", id.0)
    }
}

// ---------- transcript-tree shape ----------

/// True iff `path` sits exactly one bucket below `projects_root` — i.e.
/// `<projects_root>/<bucket>/<file>.jsonl`. Drops sidechain transcripts
/// Claude Code writes at `<bucket>/<parent-session-id>/subagents/agent-<id>.jsonl`;
/// the bulk discovery path enforces the same shape via the depth-2
/// listing, so startup and live discovery filter identically.
fn is_top_level_transcript(path: &Path, projects_root: &Path) -> bool {
    path.parent()
        .and_then(Path::parent)
        .is_some_and(|p| p == projects_root)
}

/// Decode a `project_dir` from a transcript path when the transcript
/// carried no `cwd` line: Claude names the bucket after the cwd with `/`
/// replaced by `-`, so reverse that. Falls back to the `<unknown>` literal
/// when the path has no bucket component (which discovery's stale-cwd
/// filter then rejects, since it isn't a real directory).
fn fallback_dir(transcript_path: &Path) -> PathBuf {
    transcript_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .map_or_else(
            || PathBuf::from("<unknown>"),
            |n| PathBuf::from(n.replace('-', "/")),
        )
}

// ---------- head-of-file metadata parse ----------

/// Max display length (in chars, not bytes) for the first-user-message
/// title fallback. Long enough to be useful on a 100-col terminal next to
/// the dimmed cwd/host/age trailing spans; short enough that a rambling
/// first message doesn't dominate the row.
const FIRST_USER_MSG_MAX_CHARS: usize = 60;

/// Single-pass scan over an already-fetched transcript: take cwd from the
/// first line that has one, title from the *last* `ai-title` entry (titles
/// refine as the session grows), and the first non-empty user message for
/// the title-fallback path. Malformed JSON lines are skipped.
fn parse_transcript_meta(raw: &str) -> TranscriptMeta {
    let mut meta = TranscriptMeta::default();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if meta.cwd.is_none()
            && let Some(cwd) = value.get("cwd").and_then(serde_json::Value::as_str)
        {
            meta.cwd = Some(PathBuf::from(cwd));
        }
        if value.get("type").and_then(serde_json::Value::as_str) == Some("ai-title")
            && let Some(title) = value.get("aiTitle").and_then(serde_json::Value::as_str)
        {
            meta.title = Some(title.to_string());
        }
        if meta.first_user_message.is_none()
            && value.get("type").and_then(serde_json::Value::as_str) == Some("user")
            && value.get("toolUseResult").is_none()
            && let Some(text) = extract_user_text(&value)
            && !text.trim().is_empty()
            && !is_slash_command_envelope(&text)
        {
            meta.first_user_message = Some(normalize_for_title(&text));
        }
    }
    meta
}

/// Pull the human-authored text out of a `{"type":"user", ...}` entry.
/// Accepts the three shapes seen in practice: `message` as a plain string,
/// `message.content` as a string, or `message.content` as an array of
/// `{"type":"text", "text":"..."}` blocks (with non-text blocks silently
/// skipped). Returns `None` for shapes we don't recognise rather than
/// guessing.
fn extract_user_text(entry: &serde_json::Value) -> Option<String> {
    let message = entry.get("message")?;
    if let Some(s) = message.as_str() {
        return Some(s.to_string());
    }
    let content = message.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut buf = String::new();
        for block in arr {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(serde_json::Value::as_str)
            {
                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(text);
            }
        }
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    None
}

/// True if `text` is Claude Code's slash-command wrapper (e.g.
/// `<local-command-caveat>…</local-command-caveat>` or
/// `<command-name>/clear</command-name>`) rather than human-typed prose.
/// Same family of "user entry but not human content" as `toolUseResult`:
/// surfacing it as a session title produces noise.
fn is_slash_command_envelope(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<local-command-caveat>") || trimmed.starts_with("<command-name>")
}

/// Collapse all-whitespace runs to a single space, trim, and truncate to
/// `FIRST_USER_MSG_MAX_CHARS` chars (not bytes) with an ellipsis suffix
/// when shortened. The list row renders on a single line, so a multi-line
/// first message has to be flattened before it lands there.
fn normalize_for_title(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut iter = collapsed.chars();
    let mut taken: String = iter.by_ref().take(FIRST_USER_MSG_MAX_CHARS).collect();
    if iter.next().is_some() {
        taken.push('…');
    }
    taken
}

// ---------- tail-of-file attention + edited-files derivation ----------

/// Walk every parseable JSONL line, keeping the last classifiable entry,
/// then map it to an [`AgentDerivation`]. Also collects the files edited
/// within the buffer (same walk, one pass over the lines).
fn derive_from_content(transcript: &str) -> AgentDerivation {
    let mut last: Option<EntryKind> = None;
    for line in transcript.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(kind) = classify(&value) {
            last = Some(kind);
        }
    }
    let attention = match last {
        Some(EntryKind::AssistantAwaiting) => Attention::NeedsInput,
        Some(EntryKind::AssistantToolUse | EntryKind::UserMessage | EntryKind::ToolResult) => {
            Attention::Working
        }
        None => Attention::Unknown,
    };
    AgentDerivation {
        attention,
        from_tool_use: matches!(last, Some(EntryKind::AssistantToolUse)),
        edited_files: edited_files_from_content(transcript),
    }
}

/// Tool names whose `tool_use` blocks represent a file edit. `Read`,
/// `Bash`, `Grep`, etc. are deliberately excluded — the picker is "files
/// Claude *changed*", not "files Claude looked at". `MultiEdit` is the
/// legacy batch-edit tool; `NotebookEdit` targets `.ipynb` cells and
/// carries the path under `notebook_path` rather than `file_path`.
const EDIT_TOOL_NAMES: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Extract the files edited in `transcript`, most recently edited first,
/// deduplicated, capped at [`EDITED_FILES_CAP`]. Walks every parseable
/// JSONL line for assistant `tool_use` blocks whose tool name is in
/// [`EDIT_TOOL_NAMES`] and pulls the target path (`input.file_path`, or
/// `input.notebook_path` for `NotebookEdit`).
fn edited_files_from_content(transcript: &str) -> Vec<PathBuf> {
    // Accumulate in chronological (oldest-first) order, then dedup keeping
    // each path's *most recent* occurrence, then reverse to
    // most-recent-first. Doing the dedup after the walk (rather than a
    // HashSet during it) is what lets a re-edited file move to the front.
    let mut chronological: Vec<PathBuf> = Vec::new();
    for line in transcript.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        collect_edited_paths(&value, &mut chronological);
    }
    let mut seen: std::collections::HashSet<&Path> = std::collections::HashSet::new();
    let mut recent_first: Vec<PathBuf> = Vec::new();
    for path in chronological.iter().rev() {
        if seen.insert(path.as_path()) {
            recent_first.push(path.clone());
            if recent_first.len() >= EDITED_FILES_CAP {
                break;
            }
        }
    }
    recent_first
}

/// Append the edit-target path(s) from one JSONL entry's `tool_use` blocks
/// to `acc`, in the order they appear. A single assistant entry can carry
/// several `tool_use` blocks, so this pushes each match.
fn collect_edited_paths(value: &serde_json::Value, acc: &mut Vec<PathBuf>) {
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for block in content {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
            continue;
        }
        let name = block.get("name").and_then(serde_json::Value::as_str);
        if !name.is_some_and(|n| EDIT_TOOL_NAMES.contains(&n)) {
            continue;
        }
        let Some(input) = block.get("input") else {
            continue;
        };
        // `NotebookEdit` uses `notebook_path`; every other edit tool uses
        // `file_path`. Try `file_path` first (the common case), fall back
        // to `notebook_path`.
        let path = input
            .get("file_path")
            .or_else(|| input.get("notebook_path"))
            .and_then(serde_json::Value::as_str);
        if let Some(p) = path {
            acc.push(PathBuf::from(p));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// Assistant message whose turn has actually ended — `stop_reason` is
    /// `end_turn`, `stop_sequence`, `max_tokens`, or missing/unknown (the
    /// conservative fallback so a partial or unfamiliar entry keeps the
    /// previous "assistant is the last word" behaviour rather than
    /// silently demoting to Working).
    AssistantAwaiting,
    /// Assistant message that paused only to invoke a tool (`stop_reason ==
    /// "tool_use"`). The assistant is not awaiting human input; it's
    /// waiting on the tool to return.
    AssistantToolUse,
    UserMessage,
    ToolResult,
}

fn classify(value: &serde_json::Value) -> Option<EntryKind> {
    let entry_type = value.get("type")?.as_str()?;
    match entry_type {
        "assistant" => Some(classify_assistant(value)),
        "user" => {
            if value.get("toolUseResult").is_some() {
                Some(EntryKind::ToolResult)
            } else {
                Some(EntryKind::UserMessage)
            }
        }
        _ => None,
    }
}

/// Decide whether an assistant entry represents an end-of-turn (the
/// session is now awaiting user input) or a tool-use pause (the assistant
/// is still working, the tool just hasn't returned yet).
///
/// `stop_reason` lives at `message.stop_reason` in the Claude Code JSONL
/// shape. Missing / non-string / unfamiliar values fall back to
/// `AssistantAwaiting` so a malformed line stays conservative.
fn classify_assistant(value: &serde_json::Value) -> EntryKind {
    let stop_reason = value
        .get("message")
        .and_then(|m| m.get("stop_reason"))
        .and_then(|s| s.as_str());
    match stop_reason {
        Some("tool_use") => EntryKind::AssistantToolUse,
        _ => EntryKind::AssistantAwaiting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> ClaudeAgent {
        ClaudeAgent
    }

    // ---- is_transcript ----

    #[test]
    fn is_transcript_accepts_depth_two_paths() {
        let root = Path::new("/r/projects");
        assert!(claude().is_transcript(Path::new("/r/projects/-foo/abc.jsonl"), root));
    }

    #[test]
    fn is_transcript_rejects_nested_subagent_paths() {
        // The exact shape Claude Code writes for sidechain transcripts:
        // <bucket>/<parent-session-id>/subagents/agent-<id>.jsonl. Without
        // this filter the recursive notify watch would surface every
        // subagent as a flapping standalone session.
        let root = Path::new("/r/projects");
        assert!(!claude().is_transcript(
            Path::new("/r/projects/-foo/parent-sess/subagents/agent-xyz.jsonl"),
            root
        ));
    }

    #[test]
    fn is_transcript_rejects_root_or_above() {
        let root = Path::new("/r/projects");
        assert!(!claude().is_transcript(Path::new("/r/projects/loose.jsonl"), root));
        assert!(!claude().is_transcript(Path::new("/elsewhere/foo/bar.jsonl"), root));
    }

    // ---- session_id_from_path / fallback_dir / spawn / resume ----

    #[test]
    fn session_id_from_path_uses_file_stem() {
        let id = claude().session_id_from_path(Path::new("/r/projects/-foo/abc-123.jsonl"));
        assert_eq!(id, Some(SessionId("abc-123".to_string())));
    }

    #[test]
    fn fallback_dir_decodes_bucket_name() {
        assert_eq!(
            claude().fallback_dir(Path::new("/r/projects/-home-me-proj/x.jsonl")),
            PathBuf::from("/home/me/proj")
        );
    }

    #[test]
    fn spawn_pins_the_minted_id_via_session_id_flag() {
        let plan = claude().spawn(Path::new("/w"), &SessionId("uuid-1".to_string()));
        match plan {
            SpawnPlan::PinnedId { argv } => {
                assert_eq!(argv, vec!["claude", "--session-id", "uuid-1"]);
            }
            SpawnPlan::DiscoverAfterSpawn { .. } => panic!("claude must pin its id"),
        }
    }

    #[test]
    fn resume_command_is_claude_resume_id() {
        assert_eq!(
            claude().resume_command(&SessionId("abc".to_string())),
            "claude --resume abc"
        );
    }

    // ---- parse_meta ----

    #[test]
    fn parse_meta_extracts_cwd_and_latest_title() {
        let meta = claude().parse_meta(concat!(
            "{\"type\":\"user\",\"cwd\":\"/w/proj\",\"message\":\"hi\"}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"early\"}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"refined\"}\n",
        ));
        assert_eq!(meta.cwd, Some(PathBuf::from("/w/proj")));
        assert_eq!(meta.title.as_deref(), Some("refined"));
        assert_eq!(meta.first_user_message.as_deref(), Some("hi"));
    }

    #[test]
    fn parse_meta_skips_slash_command_envelope_for_first_user_message() {
        let meta = claude().parse_meta(concat!(
            "{\"type\":\"user\",\"message\":\"<command-name>/clear</command-name>\"}\n",
            "{\"type\":\"user\",\"message\":\"real prompt\"}\n",
        ));
        assert_eq!(meta.first_user_message.as_deref(), Some("real prompt"));
    }

    #[test]
    fn parse_meta_ignores_tool_result_user_entries() {
        let meta = claude().parse_meta(concat!(
            "{\"type\":\"user\",\"toolUseResult\":{\"stdout\":\"ok\"}}\n",
            "{\"type\":\"user\",\"message\":\"the real first prompt\"}\n",
        ));
        assert_eq!(
            meta.first_user_message.as_deref(),
            Some("the real first prompt")
        );
    }

    #[test]
    fn parse_meta_first_user_message_is_truncated_with_ellipsis() {
        let long = "a".repeat(200);
        let meta = claude().parse_meta(&format!("{{\"type\":\"user\",\"message\":\"{long}\"}}\n"));
        let title = meta.first_user_message.unwrap();
        assert!(title.ends_with('…'), "got: {title}");
        assert_eq!(title.chars().count(), FIRST_USER_MSG_MAX_CHARS + 1);
    }

    // ---- derive: attention ----

    fn derive(content: &str) -> AgentDerivation {
        claude().derive(content, Path::new("/w"))
    }

    #[test]
    fn derive_empty_is_unknown() {
        assert_eq!(derive("").attention, Attention::Unknown);
    }

    #[test]
    fn derive_last_assistant_means_needs_input() {
        let d = derive(concat!(
            "{\"type\":\"user\",\"message\":\"hi\"}\n",
            "{\"type\":\"assistant\",\"message\":\"hello\"}\n",
        ));
        assert_eq!(d.attention, Attention::NeedsInput);
    }

    #[test]
    fn derive_last_user_message_means_working() {
        let d = derive(concat!(
            "{\"type\":\"assistant\",\"message\":\"hello\"}\n",
            "{\"type\":\"user\",\"message\":\"do thing\"}\n",
        ));
        assert_eq!(d.attention, Attention::Working);
    }

    #[test]
    fn derive_last_tool_result_means_working() {
        let d = derive(concat!(
            "{\"type\":\"assistant\",\"message\":\"running tool\"}\n",
            "{\"type\":\"user\",\"toolUseResult\":{\"stdout\":\"ok\"}}\n",
        ));
        assert_eq!(d.attention, Attention::Working);
    }

    #[test]
    fn derive_tool_use_stop_reason_is_working_not_needs_input() {
        let d = derive(concat!(
            "{\"type\":\"user\",\"message\":\"do thing\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"tool_use\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\"}]}}\n",
        ));
        assert_eq!(d.attention, Attention::Working);
    }

    #[test]
    fn derive_end_turn_stop_reason_is_needs_input() {
        let d = derive(
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\",\"content\":[]}}",
        );
        assert_eq!(d.attention, Attention::NeedsInput);
    }

    #[test]
    fn derive_unknown_stop_reason_falls_back_to_needs_input() {
        let d = derive(
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"pause_turn\",\"content\":[]}}",
        );
        assert_eq!(d.attention, Attention::NeedsInput);
    }

    #[test]
    fn derive_flags_from_tool_use_only_for_trailing_tool_use() {
        let tool_use = derive(
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"tool_use\",\"content\":[{\"type\":\"tool_use\"}]}}",
        );
        assert_eq!(tool_use.attention, Attention::Working);
        assert!(tool_use.from_tool_use);

        let tool_result = derive(concat!(
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"tool_use\",\"content\":[{\"type\":\"tool_use\"}]}}\n",
            "{\"type\":\"user\",\"toolUseResult\":{\"ok\":true},\"message\":\"r\"}",
        ));
        assert_eq!(tool_result.attention, Attention::Working);
        assert!(
            !tool_result.from_tool_use,
            "tool_result is progress, not a tool_use"
        );

        let done = derive(
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}",
        );
        assert_eq!(done.attention, Attention::NeedsInput);
        assert!(!done.from_tool_use);
    }

    // ---- derive: edited files ----

    /// Build one assistant JSONL line invoking `tool` on `path`.
    fn edit_line(tool: &str, path_key: &str, path: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"{tool}\",\"input\":{{\"{path_key}\":\"{path}\"}}}}]}}}}"
        )
    }

    #[test]
    fn edited_files_extracts_edit_and_write_paths_most_recent_first() {
        let content = [
            edit_line("Edit", "file_path", "/w/a.rs"),
            edit_line("Write", "file_path", "/w/b.rs"),
        ]
        .join("\n");
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/w/b.rs"), PathBuf::from("/w/a.rs")]
        );
    }

    #[test]
    fn edited_files_ignores_read_and_bash() {
        let content = [
            edit_line("Read", "file_path", "/w/looked.rs"),
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}]}}".to_string(),
            edit_line("Edit", "file_path", "/w/changed.rs"),
        ]
        .join("\n");
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/w/changed.rs")]
        );
    }

    #[test]
    fn edited_files_dedups_and_moves_reedited_to_front() {
        let content = [
            edit_line("Edit", "file_path", "/w/a.rs"),
            edit_line("Edit", "file_path", "/w/b.rs"),
            edit_line("Edit", "file_path", "/w/a.rs"),
        ]
        .join("\n");
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")]
        );
    }

    #[test]
    fn edited_files_reads_notebook_path_for_notebook_edit() {
        let content = edit_line("NotebookEdit", "notebook_path", "/w/nb.ipynb");
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/w/nb.ipynb")]
        );
    }

    #[test]
    fn edited_files_handles_multiple_tool_use_blocks_in_one_entry() {
        let content = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Edit\",\"input\":{\"file_path\":\"/w/a.rs\"}},{\"type\":\"text\",\"text\":\"and\"},{\"type\":\"tool_use\",\"name\":\"Write\",\"input\":{\"file_path\":\"/w/b.rs\"}}]}}";
        assert_eq!(
            derive(content).edited_files,
            vec![PathBuf::from("/w/b.rs"), PathBuf::from("/w/a.rs")]
        );
    }

    #[test]
    fn edited_files_empty_when_no_edits() {
        assert!(
            derive("{\"type\":\"user\",\"message\":\"hi\"}")
                .edited_files
                .is_empty()
        );
        assert!(derive("not json").edited_files.is_empty());
    }

    #[test]
    fn edited_files_caps_at_the_limit() {
        let content: String = (0..(EDITED_FILES_CAP + 50))
            .map(|i| edit_line("Edit", "file_path", &format!("/w/f{i}.rs")))
            .collect::<Vec<_>>()
            .join("\n");
        let files = derive(&content).edited_files;
        assert_eq!(files.len(), EDITED_FILES_CAP);
        assert_eq!(
            files[0],
            PathBuf::from(format!("/w/f{}.rs", EDITED_FILES_CAP + 49))
        );
    }

    #[test]
    fn derive_carries_attention_and_edited_files_from_one_walk() {
        let content = [
            edit_line("Edit", "file_path", "/w/a.rs"),
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\",\"content\":[]}}"
                .to_string(),
        ]
        .join("\n");
        let d = derive(&content);
        assert_eq!(d.attention, Attention::NeedsInput);
        assert_eq!(d.edited_files, vec![PathBuf::from("/w/a.rs")]);
    }
}
