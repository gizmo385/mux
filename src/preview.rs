//! Transcript renderer for the M3 inline preview.
//!
//! Reads Claude Code's JSONL transcript bytes and returns a compact
//! ordered list of [`PreviewLine`]s ready for display in the dashboard's
//! preview pane. Filters housekeeping/attachment entries so the output
//! reflects the user-visible conversation only.
//!
//! The parser is pure — no I/O, no async, no awareness of dashboard
//! layout. The Dashboard owns how many lines to render, how to truncate
//! to the available width, and how to style each variant. That split
//! keeps the M3 Shape A renderer cheap to evolve and makes any future
//! Shape B (full chat) pivot a matter of consuming the same `PreviewLine`
//! stream with a richer renderer.

use serde_json::Value;

/// One compact display entry extracted from a transcript. Variants
/// correspond to what the user reads when skimming a session: their own
/// prompts, the assistant's prose replies, each tool the assistant
/// invoked, and the bare success/failure of each tool result.
///
/// Tool-call detail (arguments, full stdout, diffs) is deliberately not
/// surfaced — preview is the "what's happening" pane, not a debugger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewLine {
    /// Human-authored prompt. Whitespace-collapsed.
    User(String),
    /// Assistant prose response. Excludes `thinking` blocks, which are
    /// the model's internal reasoning and not useful in a glance-preview.
    Assistant(String),
    /// One tool invocation. `summary` is a short, tool-specific hint
    /// (file path for Read/Edit, description or command for Bash, etc.)
    /// or empty when the tool's input does not have an obvious one-line
    /// signal.
    ToolUse { name: String, summary: String },
    /// One tool result, classified only as success or failure. The
    /// transcript shape varies enough by tool that we don't try to
    /// summarise the body here — the user attaches if they want detail.
    ToolResult { ok: bool },
}

/// Parse a JSONL transcript into the trailing `limit` preview lines.
///
/// Best-effort throughout: malformed JSON lines and unrecognised entry
/// shapes are skipped silently rather than propagated. An empty input,
/// a transcript containing only housekeeping, or `limit == 0` all
/// return an empty `Vec`.
///
/// Returned lines are in chronological order (oldest of the kept entries
/// first). The dashboard renders newest at the bottom so the user reads
/// the preview pane top-to-bottom in transcript order.
#[must_use]
pub fn parse_preview(text: &str, limit: usize) -> Vec<PreviewLine> {
    if limit == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        lines.extend(extract(&value));
    }
    if lines.len() > limit {
        let start = lines.len() - limit;
        lines.drain(..start);
    }
    lines
}

fn extract(value: &Value) -> Vec<PreviewLine> {
    // Sidechain entries are sub-agent runs; the M3 preview shows only
    // the primary conversation. Including sidechain would crowd the
    // pane with output the user did not directly ask for. Revisit if
    // dogfooding asks for it.
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    let Some(entry_type) = value.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    match entry_type {
        "user" => extract_user(value),
        "assistant" => extract_assistant(value),
        // Skip: ai-title, permission-mode, file-history-snapshot,
        // attachment, system/local_command, and anything we don't know.
        _ => Vec::new(),
    }
}

fn extract_user(value: &Value) -> Vec<PreviewLine> {
    // The `toolUseResult` shape is `{"type":"user", ...,
    // "toolUseResult":{...}}` — a tool result wrapped in a user entry.
    // Same family as `discovery.rs` skip-list, kept separate here so
    // we can surface success/failure rather than dropping the entry.
    if let Some(result) = value.get("toolUseResult") {
        return vec![PreviewLine::ToolResult {
            ok: !is_error_result(result),
        }];
    }
    let Some(message) = value.get("message") else {
        return Vec::new();
    };
    let text = extract_user_text(message);
    if text.is_empty() || is_command_envelope(&text) {
        return Vec::new();
    }
    vec![PreviewLine::User(collapse_whitespace(&text))]
}

fn extract_user_text(message: &Value) -> String {
    if let Some(s) = message.get("content").and_then(Value::as_str) {
        return s.to_string();
    }
    let Some(arr) = message.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for block in arr {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(t) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t);
    }
    out
}

fn is_command_envelope(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<command-name>") || trimmed.starts_with("<local-command-caveat>")
}

fn extract_assistant(value: &Value) -> Vec<PreviewLine> {
    let Some(message) = value.get("message") else {
        return Vec::new();
    };
    if let Some(s) = message.get("content").and_then(Value::as_str) {
        let collapsed = collapse_whitespace(s);
        return if collapsed.is_empty() {
            Vec::new()
        } else {
            vec![PreviewLine::Assistant(collapsed)]
        };
    }
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for block in blocks {
        let Some(block_type) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        match block_type {
            "text" => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    let collapsed = collapse_whitespace(t);
                    if !collapsed.is_empty() {
                        lines.push(PreviewLine::Assistant(collapsed));
                    }
                }
            }
            "tool_use" => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let summary = tool_use_summary(&name, block.get("input"));
                lines.push(PreviewLine::ToolUse { name, summary });
            }
            // thinking, image, redacted_thinking, etc. — not surfaced.
            _ => {}
        }
    }
    lines
}

/// Tool-specific extraction of the most useful one-line hint from
/// `input`. Per-tool because no single field generalises: Bash has
/// `description`/`command`, file tools have `file_path`, search tools
/// have `pattern`/`query`, and the long tail (`TodoWrite`, `AskUserQuestion`)
/// has nothing scannable.
fn tool_use_summary(name: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    match name {
        "Bash" => {
            if let Some(d) = input.get("description").and_then(Value::as_str)
                && !d.is_empty()
            {
                return d.to_string();
            }
            input
                .get("command")
                .and_then(Value::as_str)
                .and_then(|c| c.lines().next())
                .unwrap_or("")
                .to_string()
        }
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => input
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "Glob" | "Grep" => input
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "WebFetch" => input
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "WebSearch" => input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn is_error_result(result: &Value) -> bool {
    if let Some(b) = result.get("is_error").and_then(Value::as_bool) {
        return b;
    }
    result.get("error").is_some()
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(jsonl: &[&str]) -> String {
        jsonl.join("\n")
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(parse_preview("", 10).is_empty());
    }

    #[test]
    fn limit_zero_returns_empty_regardless_of_input() {
        let input = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        assert!(parse_preview(input, 0).is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped_not_panicked() {
        let input = lines(&[
            "not json",
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
            "{not closed",
        ]);
        assert_eq!(
            parse_preview(&input, 10),
            vec![PreviewLine::User("hello".to_string())]
        );
    }

    #[test]
    fn housekeeping_entries_are_dropped() {
        let input = lines(&[
            r#"{"type":"permission-mode","permissionMode":"default"}"#,
            r#"{"type":"file-history-snapshot"}"#,
            r#"{"type":"ai-title","aiTitle":"x"}"#,
            r#"{"type":"attachment","attachment":{"type":"skill_listing"}}"#,
            r#"{"type":"system","subtype":"local_command","content":"<command-name>/usage</command-name>"}"#,
        ]);
        assert!(parse_preview(&input, 10).is_empty());
    }

    #[test]
    fn sidechain_entries_are_dropped() {
        let input = lines(&[
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"sub-agent"}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"shh"}]}}"#,
        ]);
        assert!(parse_preview(&input, 10).is_empty());
    }

    #[test]
    fn user_with_string_content_yields_one_user_line() {
        let input = r#"{"type":"user","message":{"role":"user","content":"refactor parser"}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::User("refactor parser".to_string())]
        );
    }

    #[test]
    fn user_with_content_blocks_concatenates_text_blocks() {
        let input = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"first"},{"type":"image","source":{}},{"type":"text","text":"second"}]}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::User("first second".to_string())]
        );
    }

    #[test]
    fn user_message_with_command_envelope_is_skipped() {
        let input = r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#;
        assert!(parse_preview(input, 10).is_empty());
    }

    #[test]
    fn user_message_with_local_command_caveat_is_skipped() {
        let input = r#"{"type":"user","message":{"role":"user","content":"<local-command-caveat>blah</local-command-caveat>"}}"#;
        assert!(parse_preview(input, 10).is_empty());
    }

    #[test]
    fn user_text_with_internal_whitespace_is_collapsed() {
        let input =
            r#"{"type":"user","message":{"role":"user","content":"hello\n\n  world\t\there"}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::User("hello world here".to_string())]
        );
    }

    #[test]
    fn assistant_with_string_content_yields_one_assistant_line() {
        let input =
            r#"{"type":"assistant","message":{"role":"assistant","content":"reading parser"}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::Assistant("reading parser".to_string())]
        );
    }

    #[test]
    fn assistant_thinking_blocks_are_skipped_text_is_kept() {
        let input = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hidden reasoning"},{"type":"text","text":"on it"}]}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::Assistant("on it".to_string())]
        );
    }

    #[test]
    fn assistant_tool_use_yields_tool_use_line() {
        let input = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"Bash","input":{"command":"ls -la","description":"List repo root"}}]}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::ToolUse {
                name: "Bash".to_string(),
                summary: "List repo root".to_string(),
            }]
        );
    }

    #[test]
    fn bash_tool_falls_back_to_command_first_line_when_description_missing() {
        let input = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"echo one\necho two"}}]}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::ToolUse {
                name: "Bash".to_string(),
                summary: "echo one".to_string(),
            }]
        );
    }

    #[test]
    fn file_tools_use_file_path_as_summary() {
        let input = lines(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/x/a.rs"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/x/b.rs"}}]}}"#,
        ]);
        assert_eq!(
            parse_preview(&input, 10),
            vec![
                PreviewLine::ToolUse {
                    name: "Read".to_string(),
                    summary: "/x/a.rs".to_string(),
                },
                PreviewLine::ToolUse {
                    name: "Edit".to_string(),
                    summary: "/x/b.rs".to_string(),
                },
            ]
        );
    }

    #[test]
    fn search_tools_use_pattern_as_summary() {
        let input = lines(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Grep","input":{"pattern":"TODO"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Glob","input":{"pattern":"**/*.rs"}}]}}"#,
        ]);
        assert_eq!(
            parse_preview(&input, 10),
            vec![
                PreviewLine::ToolUse {
                    name: "Grep".to_string(),
                    summary: "TODO".to_string(),
                },
                PreviewLine::ToolUse {
                    name: "Glob".to_string(),
                    summary: "**/*.rs".to_string(),
                },
            ]
        );
    }

    #[test]
    fn unknown_tool_gets_empty_summary() {
        let input = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"AskUserQuestion","input":{"questions":[]}}]}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::ToolUse {
                name: "AskUserQuestion".to_string(),
                summary: String::new(),
            }]
        );
    }

    #[test]
    fn assistant_with_multiple_tool_uses_yields_one_line_per_call() {
        let input = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"a"}},{"type":"tool_use","name":"Read","input":{"file_path":"b"}}]}}"#;
        let parsed = parse_preview(input, 10);
        assert_eq!(parsed.len(), 2);
        assert!(matches!(parsed[0], PreviewLine::ToolUse { .. }));
        assert!(matches!(parsed[1], PreviewLine::ToolUse { .. }));
    }

    #[test]
    fn tool_result_entry_yields_ok_when_no_error_marker() {
        let input = r#"{"type":"user","toolUseResult":{"stdout":"ok"},"message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::ToolResult { ok: true }]
        );
    }

    #[test]
    fn tool_result_with_is_error_flag_yields_error() {
        let input = r#"{"type":"user","toolUseResult":{"is_error":true,"stderr":"boom"},"message":{"role":"user","content":[]}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::ToolResult { ok: false }]
        );
    }

    #[test]
    fn tool_result_with_error_field_yields_error() {
        let input = r#"{"type":"user","toolUseResult":{"error":"oops"},"message":{"role":"user","content":[]}}"#;
        assert_eq!(
            parse_preview(input, 10),
            vec![PreviewLine::ToolResult { ok: false }]
        );
    }

    #[test]
    fn limit_keeps_the_trailing_n_in_chronological_order() {
        let input = lines(&[
            r#"{"type":"user","message":{"role":"user","content":"one"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"two"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"three"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"four"}}"#,
        ]);
        assert_eq!(
            parse_preview(&input, 2),
            vec![
                PreviewLine::User("three".to_string()),
                PreviewLine::User("four".to_string()),
            ]
        );
    }

    #[test]
    fn limit_counts_individual_preview_lines_not_jsonl_entries() {
        // Two tool uses in one assistant entry → both must count toward
        // the limit; the limit is on display lines, not source rows.
        let input = lines(&[
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"a"}},{"type":"tool_use","name":"Read","input":{"file_path":"b"}}]}}"#,
        ]);
        assert_eq!(parse_preview(&input, 2).len(), 2);
    }

    #[test]
    fn empty_user_content_yields_no_lines() {
        let input = r#"{"type":"user","message":{"role":"user","content":""}}"#;
        assert!(parse_preview(input, 10).is_empty());
    }

    #[test]
    fn empty_assistant_text_block_yields_no_lines() {
        let input = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":""}]}}"#;
        assert!(parse_preview(input, 10).is_empty());
    }

    #[test]
    fn unrecognised_top_level_type_is_dropped() {
        let input = r#"{"type":"made-up-kind","foo":"bar"}"#;
        assert!(parse_preview(input, 10).is_empty());
    }

    #[test]
    fn realistic_short_transcript_parses_in_chronological_order() {
        // Approximates the head of a real session: housekeeping,
        // user prompt, assistant thinking + tool_use, tool result.
        let input = lines(&[
            r#"{"type":"permission-mode","permissionMode":"default"}"#,
            r#"{"type":"file-history-snapshot"}"#,
            r#"{"type":"user","message":{"role":"user","content":"check the README"}}"#,
            r#"{"type":"attachment","attachment":{"type":"deferred_tools_delta"}}"#,
            r#"{"type":"ai-title","aiTitle":"check README"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"redacted"},{"type":"text","text":"reading it now"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"README.md"}}]}}"#,
            r#"{"type":"user","toolUseResult":{"stdout":"ok"},"message":{"role":"user","content":[]}}"#,
        ]);
        assert_eq!(
            parse_preview(&input, 10),
            vec![
                PreviewLine::User("check the README".to_string()),
                PreviewLine::Assistant("reading it now".to_string()),
                PreviewLine::ToolUse {
                    name: "Read".to_string(),
                    summary: "README.md".to_string(),
                },
                PreviewLine::ToolResult { ok: true },
            ]
        );
    }
}
