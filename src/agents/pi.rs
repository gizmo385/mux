//! Pi (`@earendil-works/pi-coding-agent`) — the [`AgentCli`]
//! implementation: the read path (discovery + session parser, WP4) plus
//! the spawn/resume argv (WP6). Spawn/resume is a `PinnedId` contract
//! identical to Claude's, via `pi --session-id <id>` — the flag creates
//! the session if missing and reopens it if present, so the same argv
//! serves both spawn and resume (the direct `claude --session-id` analog).
//!
//! Pi sessions live at
//! `~/.pi/agent/sessions/--<encoded-cwd>--/<ts>_<id>.jsonl` (Appendix B of
//! the multi-agent plan, researched 2026-07-09 against
//! `@earendil-works/pi-coding-agent` v0.80.6). The cwd is encoded into the
//! parent directory name (strip the leading `/`, replace `/` `\` `:` with
//! `-`, wrap in `--…--` — readable, not hashed), and the filename is an ISO
//! timestamp (its `:`/`.` also replaced by `-`) joined to the session id by
//! a single `_`. The id defaults to a uuid but any
//! `[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?` string is legal, so nothing
//! here assumes uuid shape.
//!
//! The file is JSONL, append-only, and *tree-structured* (each entry
//! carries an `id`/`parentId`, enabling in-file branching). Like pi's own
//! preview tooling — and like the other agent parsers here — this reads the
//! transcript **linearly**: branch-awareness is explicitly out of scope
//! (plan §WP4). Line 1 is a `{"type":"session","version":3,…}` header
//! carrying `cwd`; the loader tolerates unknown entry types and unknown
//! `message.role` values silently, which is the documented upstream v3
//! contract (`CURRENT_SESSION_VERSION = 3`, unknown entries skipped).
//!
//! Pi has no built-in permission gates (extensions could add them, WP8),
//! so there is no "blocked" attention state to protect and `from_tool_use`
//! is set purely to mark a genuine in-flight tool (`stopReason == "toolUse"`),
//! matching Claude's hook-clobber-guard semantics.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::agent::{AgentCli, AgentDerivation, AgentKind, ListingSpec, SpawnPlan, TranscriptMeta};
use crate::session::{Attention, EDITED_FILES_CAP, SessionId};

/// Max display length (in chars) for the first-user-message title fallback.
/// Kept in sync with the Claude/Codex reference impls so a pi row and a
/// claude row truncate identically on the same-width dashboard.
const FIRST_USER_MSG_MAX_CHARS: usize = 60;

#[derive(Debug, Default, Clone, Copy)]
pub struct PiAgent;

impl AgentCli for PiAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Pi
    }

    fn label(&self) -> &'static str {
        "pi"
    }

    fn default_binary(&self) -> &'static str {
        "pi"
    }

    fn default_transcript_root(&self) -> Option<PathBuf> {
        // Local-only env handling (plan §WP4 item 1). `PI_CODING_AGENT_SESSION_DIR`
        // (highest) points directly at the sessions directory;
        // `PI_CODING_AGENT_DIR` relocates the whole `~/.pi/agent` tree, with
        // sessions at `<dir>/sessions`; the bare default is
        // `~/.pi/agent/sessions`. These environment variables describe the
        // *local* machine only — a config `transcript_root` still wins at the
        // `Config::transcript_root_for` layer, and *remote* roots come from
        // config, never from this process's env (`home_relative_default_root`
        // re-homes the default under the remote user's `~`; an env-relocated
        // path outside the local home simply yields no remote default, which
        // is correct — a local relocation must not leak to another host).
        if let Some(dir) = non_empty_env("PI_CODING_AGENT_SESSION_DIR") {
            return Some(dir);
        }
        if let Some(dir) = non_empty_env("PI_CODING_AGENT_DIR") {
            return Some(dir.join("sessions"));
        }
        dirs::home_dir().map(|h| h.join(".pi").join("agent").join("sessions"))
    }

    fn listing(&self) -> ListingSpec {
        // `<root>/--<encoded-cwd>--/<ts>_<id>.jsonl` — files exactly two
        // levels below the root (Appendix B).
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
        // `<ts>_<id>`: the id is everything after the FIRST `_` (the
        // timestamp has its `:`/`.` rewritten to `-`, never `_`, so the
        // first `_` is the ts/id boundary; an id may itself contain `_`, so
        // split only once). Validated loosely against pi's id grammar so a
        // stray filename yields `None` rather than a bogus id.
        let stem = path.file_stem()?.to_str()?;
        let (_ts, id) = stem.split_once('_')?;
        if is_valid_pi_id(id) {
            Some(SessionId(id.to_string()))
        } else {
            None
        }
    }

    fn fallback_dir(&self, transcript_path: &Path) -> PathBuf {
        // The parent dir encodes the cwd (`--home-me-proj--` → `/home/me/proj`):
        // strip the wrapping `--…--`, then reverse the `/`→`-` substitution.
        // Reached only when a transcript carries no `session` header `cwd`
        // (malformed / truncated head); discovery's is_dir filter then drops
        // the result if the decoded path isn't a real directory. Best-effort:
        // the `/`→`-` encoding is lossy (a real `-` in a path segment is
        // indistinguishable from an encoded `/`), same caveat as Claude's
        // bucket decode.
        transcript_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .map_or_else(
                || PathBuf::from("<unknown>"),
                |n| {
                    let inner = n.strip_prefix("--").unwrap_or(n);
                    let inner = inner.strip_suffix("--").unwrap_or(inner);
                    PathBuf::from(format!("/{}", inner.replace('-', "/")))
                },
            )
    }

    fn parse_meta(&self, content: &str) -> TranscriptMeta {
        parse_meta_impl(content)
    }

    fn derive(&self, content: &str, cwd: &Path) -> AgentDerivation {
        derive_from_content(content, cwd)
    }

    fn spawn(&self, _cwd: &Path, minted_id: &SessionId) -> SpawnPlan {
        // Pi pins a caller-chosen id, exactly like claude: the minted uuid
        // is the tmux session-name suffix, the `--session-id`, and (after
        // the `<ts>_` filename prefix `session_id_from_path` strips) the
        // transcript id all at once — today's identity contract. Never
        // `--no-session`, which would spawn an ephemeral, untailed session.
        SpawnPlan::PinnedId {
            argv: vec![
                "pi".to_string(),
                "--session-id".to_string(),
                minted_id.0.clone(),
            ],
        }
    }

    fn resume_command(&self, id: &SessionId) -> String {
        // `pi --session-id <id>` reopens an existing session by exact id —
        // the same flag as spawn (creates-if-missing / reopens-if-present),
        // the direct analog of `claude --resume`.
        format!("pi --session-id {}", id.0)
    }
}

/// Read an environment variable as a `PathBuf`, treating unset *and* empty
/// as absent. Keeps the root-resolution chain from producing a bogus
/// relative-to-cwd root when a var is exported but blank.
fn non_empty_env(key: &str) -> Option<PathBuf> {
    let val = std::env::var_os(key)?;
    if val.is_empty() {
        None
    } else {
        Some(PathBuf::from(val))
    }
}

// ---------- transcript-tree shape ----------

/// True iff `path` is `<root>/--<encoded-cwd>--/<ts>_<id>.jsonl`: exactly
/// two components below `root`, the parent directory wrapped in `--…--`
/// (its `--` prefix and suffix non-overlapping, i.e. length ≥ 4), and the
/// filename a `<ts>_<id>.jsonl` — at least one `_` (the ts/id separator)
/// with a `.jsonl` extension. Mirrors the depth-2 listing glob so startup
/// and live discovery filter identically.
fn is_top_level_transcript(path: &Path, root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let mut parts: Vec<&str> = Vec::with_capacity(2);
    for component in rel.components() {
        let Component::Normal(os) = component else {
            return false;
        };
        let Some(s) = os.to_str() else {
            return false;
        };
        parts.push(s);
    }
    if parts.len() != 2 {
        return false;
    }
    let dir = parts[0];
    if dir.len() < 4 || !dir.starts_with("--") || !dir.ends_with("--") {
        return false;
    }
    let name = parts[1];
    let file = Path::new(name);
    let Some(stem) = file.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    file.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        && stem.contains('_')
}

/// Loose validation of a pi session id against
/// `[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?`: non-empty, first and last
/// chars ASCII-alphanumeric, every char in `[A-Za-z0-9._-]`. Deliberately
/// not a uuid check — pi ids are frequently uuids but need not be.
fn is_valid_pi_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    let last = bytes[bytes.len() - 1];
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

// ---------- head-of-file metadata parse ----------

/// Single linear pass over the transcript head: take `cwd` from the line-1
/// `session` header, the title from the *latest* `session_info` entry's
/// `name` (renames refine as the session grows — last wins), and the first
/// human user message for the title-fallback path. Malformed JSON lines are
/// skipped. Tree structure (`id`/`parentId`) is ignored — read linearly
/// (plan §WP4).
fn parse_meta_impl(content: &str) -> TranscriptMeta {
    let mut meta = TranscriptMeta::default();
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let entry_type = value.get("type").and_then(Value::as_str);
        if meta.cwd.is_none()
            && entry_type == Some("session")
            && let Some(cwd) = value.get("cwd").and_then(Value::as_str)
        {
            meta.cwd = Some(PathBuf::from(cwd));
        }
        if entry_type == Some("session_info")
            && let Some(name) = value.get("name").and_then(Value::as_str)
            && !name.trim().is_empty()
        {
            // Latest rename wins (kept raw, like Claude's `ai-title`).
            meta.title = Some(name.to_string());
        }
        if meta.first_user_message.is_none()
            && entry_type == Some("message")
            && let Some(text) = user_message_text(&value)
            && !text.trim().is_empty()
        {
            meta.first_user_message = Some(normalize_for_title(&text));
        }
    }
    meta
}

/// Human user-message text from a `message` entry whose `message.role` is
/// `"user"`. Content may be a plain string or an array of `{type, text}`
/// blocks (non-text blocks skipped) — same tolerance as Claude's
/// `extract_user_text`. `None` for any non-user message or unrecognised
/// content shape.
fn user_message_text(entry: &Value) -> Option<String> {
    let message = entry.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let content = message.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut buf = String::new();
        for block in arr {
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(Value::as_str)
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

/// Collapse whitespace runs to single spaces, trim, and truncate to
/// [`FIRST_USER_MSG_MAX_CHARS`] chars with an ellipsis when shortened.
/// Same shape as the Claude/Codex reference impls' title normaliser.
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

/// The tail classification of the last `message` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PiTail {
    /// Assistant paused to invoke a tool (`stopReason == "toolUse"`) — the
    /// agent is working, awaiting the tool's return.
    AssistantToolUse,
    /// Assistant ended its turn (`stopReason ∈ {stop, length, error,
    /// aborted}`, or missing/unknown — the conservative default, mirroring
    /// Claude) — the session awaits the next human prompt.
    AssistantAwaiting,
    /// A user / toolResult / bashExecution message — conversation progress,
    /// the agent is working.
    Working,
}

/// Walk every parseable line once (linearly, ignoring branch structure):
/// track the *last* classifiable `message` entry for attention, capture the
/// line-1 header `cwd` for relative-path resolution, and accumulate edited
/// paths from assistant `toolCall` blocks named `write`/`edit`. Unknown
/// entry types and unknown `message.role` values are ignored silently (v3
/// loader tolerance). A tail window that begins mid-JSON-line drops its
/// first (unparseable) line via the same `continue`, exactly like the
/// Claude/Codex parsers.
fn derive_from_content(content: &str, cwd: &Path) -> AgentDerivation {
    let mut last: Option<PiTail> = None;
    let mut header_cwd: Option<PathBuf> = None;
    // Raw path strings, oldest-first — resolved to absolute paths *after*
    // the walk, once the base cwd (passed, or header fallback) is known.
    let mut raw_paths: Vec<String> = Vec::new();

    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                if header_cwd.is_none()
                    && let Some(c) = value.get("cwd").and_then(Value::as_str)
                {
                    header_cwd = Some(PathBuf::from(c));
                }
            }
            Some("message") => {
                if let Some(tail) = classify_message(&value) {
                    last = Some(tail);
                }
                collect_edited_paths(&value, &mut raw_paths);
            }
            // Unknown / structural entry types (model_change, compaction,
            // label, custom, …) contribute nothing.
            _ => {}
        }
    }

    // Base cwd for relative-path resolution: the passed cwd wins; if it's the
    // empty placeholder (the watcher's tail path supplies no cwd) fall back to
    // the header cwd when the scanned window actually contained the header
    // (whole-buffer discovery, or a fresh session's first tail). When neither
    // is available a relative path is pushed as-is — a documented degradation.
    let base: Option<&Path> = if cwd.as_os_str().is_empty() {
        header_cwd.as_deref()
    } else {
        Some(cwd)
    };
    let chronological: Vec<PathBuf> = raw_paths
        .iter()
        .map(|raw| resolve_path(raw, base))
        .collect();

    let (attention, from_tool_use) = match last {
        Some(PiTail::AssistantToolUse) => (Attention::Working, true),
        Some(PiTail::AssistantAwaiting) => (Attention::NeedsInput, false),
        Some(PiTail::Working) => (Attention::Working, false),
        None => (Attention::Unknown, false),
    };
    AgentDerivation {
        attention,
        from_tool_use,
        edited_files: dedup_most_recent_first(&chronological),
    }
}

/// Classify a `message` entry by `message.role`. Assistant messages split on
/// `stopReason`; user/toolResult/bashExecution are progress; every other
/// role (custom, branchSummary, compactionSummary, unknown) yields `None` so
/// the last-message decision skips it silently.
fn classify_message(value: &Value) -> Option<PiTail> {
    let message = value.get("message")?;
    match message.get("role").and_then(Value::as_str)? {
        "assistant" => {
            let stop = message.get("stopReason").and_then(Value::as_str);
            Some(match stop {
                Some("toolUse") => PiTail::AssistantToolUse,
                // stop / length / error / aborted / missing / unknown → the
                // conservative "turn ended, awaiting input" default.
                _ => PiTail::AssistantAwaiting,
            })
        }
        "user" | "toolResult" | "bashExecution" => Some(PiTail::Working),
        _ => None,
    }
}

/// Push the edit-target path of each assistant `toolCall` block named
/// `write`/`edit` onto `acc`, in order. The path lives at `arguments.path`;
/// the legacy `arguments.file_path` alias is tolerated. Non-edit tool calls
/// (`read`, `bash`, …) and non-`toolCall` blocks contribute nothing.
fn collect_edited_paths(value: &Value, acc: &mut Vec<String>) {
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
            continue;
        }
        let name = block.get("name").and_then(Value::as_str);
        if !matches!(name, Some("write" | "edit")) {
            continue;
        }
        let Some(args) = block.get("arguments") else {
            continue;
        };
        let path = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(Value::as_str);
        if let Some(p) = path {
            acc.push(p.to_string());
        }
    }
}

/// Resolve a recorded edit path to an absolute path: an absolute path is
/// taken as-is; a relative path is joined onto `base` when one is known,
/// else pushed as-is (the documented degradation when neither the passed nor
/// the header cwd is available).
fn resolve_path(raw: &str, base: Option<&Path>) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(base) = base {
        base.join(p)
    } else {
        p.to_path_buf()
    }
}

/// Dedup a chronological (oldest-first) path list into most-recent-first,
/// keeping each path's latest occurrence, capped at [`EDITED_FILES_CAP`].
/// Same strategy as the Claude/Codex reference impls.
fn dedup_most_recent_first(chronological: &[PathBuf]) -> Vec<PathBuf> {
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

#[cfg(test)]
mod tests {
    //! The JSONL fixtures below are **synthetic**, authored from the
    //! researched Pi session schema (multi-agent plan Appendix B, researched
    //! 2026-07-09 against `@earendil-works/pi-coding-agent` v0.80.6) — no
    //! `pi` binary was installed on the build machine to capture from (an
    //! empty real session dir confirmed only the cwd-encoding directory
    //! shape). They pin the record/field shapes the parser depends on so an
    //! upstream schema break fails loudly here rather than silently in the
    //! field.
    use super::*;

    fn pi() -> PiAgent {
        PiAgent
    }

    // ---- is_transcript ----

    const ROOT: &str = "/home/me/.pi/agent/sessions";

    fn pi_path(rest: &str) -> PathBuf {
        Path::new(ROOT).join(rest)
    }

    #[test]
    fn is_transcript_accepts_encoded_cwd_dir_and_ts_id_file() {
        assert!(pi().is_transcript(
            &pi_path("--home-me-proj--/2026-07-09T14-23-05-000_abc123.jsonl"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_accepts_the_real_observed_dir_name() {
        // The one real directory that existed on the build machine
        // (`~/.pi/agent/sessions/--home-gizmo-workspace-dotfiles--/`),
        // confirming the `--<encoded-cwd>--` shape end-to-end.
        assert!(pi().is_transcript(
            &pi_path(
                "--home-gizmo-workspace-dotfiles--/2026-07-09T14-23-05-000_11111111-2222-3333-4444-555555555555.jsonl"
            ),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_accepts_non_uuid_slug_id() {
        assert!(pi().is_transcript(
            &pi_path("--home-me-proj--/2026-07-09T14-23-05-000_my.session-1.jsonl"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_parent_dir_without_dashes() {
        assert!(!pi().is_transcript(
            &pi_path("home-me-proj/2026-07-09T14-23-05-000_abc.jsonl"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_parent_dir_dashes_one_side_only() {
        assert!(!pi().is_transcript(
            &pi_path("--home-me-proj/2026-07-09T14-23-05-000_abc.jsonl"),
            Path::new(ROOT),
        ));
        assert!(!pi().is_transcript(
            &pi_path("home-me-proj--/2026-07-09T14-23-05-000_abc.jsonl"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_filename_without_underscore() {
        // No `_` separating ts from id — not a pi transcript name.
        assert!(!pi().is_transcript(
            &pi_path("--home-me-proj--/nounderscore.jsonl"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_wrong_extension() {
        assert!(!pi().is_transcript(
            &pi_path("--home-me-proj--/2026-07-09T14-23-05-000_abc.txt"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_wrong_depth() {
        // Too shallow (1 component under root) and too deep (3).
        assert!(!pi().is_transcript(
            &pi_path("2026-07-09T14-23-05-000_abc.jsonl"),
            Path::new(ROOT),
        ));
        assert!(!pi().is_transcript(
            &pi_path("--home-me-proj--/sub/2026-07-09T14-23-05-000_abc.jsonl"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_paths_outside_root() {
        assert!(!pi().is_transcript(
            Path::new("/elsewhere/--home-me-proj--/2026_abc.jsonl"),
            Path::new(ROOT),
        ));
    }

    // ---- session_id_from_path ----

    #[test]
    fn session_id_extracts_uuid_after_first_underscore() {
        let id = pi().session_id_from_path(&pi_path(
            "--home-me-proj--/2026-07-09T14-23-05-000_11111111-2222-3333-4444-555555555555.jsonl",
        ));
        assert_eq!(
            id,
            Some(SessionId(
                "11111111-2222-3333-4444-555555555555".to_string()
            ))
        );
    }

    #[test]
    fn session_id_extracts_non_uuid_slug() {
        let id = pi().session_id_from_path(&pi_path(
            "--home-me-proj--/2026-07-09T14-23-05-000_my.session-1.jsonl",
        ));
        assert_eq!(id, Some(SessionId("my.session-1".to_string())));
    }

    #[test]
    fn session_id_keeps_underscores_inside_the_id() {
        // Split only on the FIRST `_`: an id may itself contain underscores.
        let id = pi().session_id_from_path(&pi_path(
            "--home-me-proj--/2026-07-09T14-23-05-000_slug_with_parts.jsonl",
        ));
        assert_eq!(id, Some(SessionId("slug_with_parts".to_string())));
    }

    #[test]
    fn session_id_none_without_underscore() {
        assert_eq!(
            pi().session_id_from_path(&pi_path("--home-me-proj--/nounderscore.jsonl")),
            None
        );
    }

    #[test]
    fn session_id_none_for_trailing_underscore() {
        // `<ts>_<id>` where the id part is empty (trailing `_`) is invalid —
        // the id's last char must be alphanumeric.
        assert_eq!(
            pi().session_id_from_path(&pi_path("--home-me-proj--/2026-07-09T14-23-05-000_.jsonl")),
            None
        );
    }

    #[test]
    fn session_id_none_for_id_ending_in_separator() {
        assert_eq!(
            pi().session_id_from_path(&pi_path("--home-me-proj--/ts_abc-.jsonl")),
            None
        );
    }

    // ---- fallback_dir ----

    #[test]
    fn fallback_dir_decodes_wrapped_bucket_name() {
        assert_eq!(
            pi().fallback_dir(&pi_path("--home-me-proj--/ts_abc.jsonl")),
            PathBuf::from("/home/me/proj")
        );
    }

    // ---- spawn / resume ----

    #[test]
    fn spawn_pins_the_minted_id() {
        let plan = pi().spawn(Path::new("/w"), &SessionId("uuid-1".to_string()));
        match plan {
            SpawnPlan::PinnedId { argv } => {
                assert_eq!(argv, vec!["pi", "--session-id", "uuid-1"]);
            }
            SpawnPlan::DiscoverAfterSpawn { .. } => panic!("pi must pin its id"),
        }
    }

    #[test]
    fn resume_command_is_pi_session_id() {
        assert_eq!(
            pi().resume_command(&SessionId("abc".to_string())),
            "pi --session-id abc"
        );
    }

    // ---- parse_meta ----

    const HEADER: &str = r#"{"type":"session","version":3,"id":"11111111","timestamp":"2026-07-09T14:23:05Z","cwd":"/work/proj"}"#;

    #[test]
    fn parse_meta_reads_v3_header_cwd() {
        let meta = pi().parse_meta(HEADER);
        assert_eq!(meta.cwd, Some(PathBuf::from("/work/proj")));
        assert_eq!(meta.title, None);
        assert_eq!(meta.first_user_message, None);
    }

    #[test]
    fn parse_meta_first_user_message_string_content() {
        let content = format!(
            "{HEADER}\n{}\n",
            r#"{"type":"message","id":"a1","message":{"role":"user","content":"refactor the parser"}}"#,
        );
        let meta = pi().parse_meta(&content);
        assert_eq!(meta.cwd, Some(PathBuf::from("/work/proj")));
        assert_eq!(
            meta.first_user_message.as_deref(),
            Some("refactor the parser")
        );
    }

    #[test]
    fn parse_meta_first_user_message_block_array_content() {
        let content = format!(
            "{HEADER}\n{}\n",
            r#"{"type":"message","id":"a1","message":{"role":"user","content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}}"#,
        );
        assert_eq!(
            pi().parse_meta(&content).first_user_message.as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn parse_meta_session_info_name_populates_title() {
        let content = format!(
            "{HEADER}\n{}\n",
            r#"{"type":"session_info","id":"s1","name":"my task"}"#,
        );
        assert_eq!(pi().parse_meta(&content).title.as_deref(), Some("my task"));
    }

    #[test]
    fn parse_meta_latest_session_info_rename_wins() {
        let content = format!(
            "{HEADER}\n{}\n{}\n",
            r#"{"type":"session_info","id":"s1","name":"early name"}"#,
            r#"{"type":"session_info","id":"s2","name":"renamed later"}"#,
        );
        assert_eq!(
            pi().parse_meta(&content).title.as_deref(),
            Some("renamed later")
        );
    }

    #[test]
    fn parse_meta_ignores_non_user_messages_for_first_user_message() {
        // An assistant message must not become the first-user-message.
        let content = format!(
            "{HEADER}\n{}\n{}\n",
            r#"{"type":"message","id":"a1","message":{"role":"assistant","content":"hi from bot","stopReason":"stop"}}"#,
            r#"{"type":"message","id":"a2","message":{"role":"user","content":"the real first prompt"}}"#,
        );
        assert_eq!(
            pi().parse_meta(&content).first_user_message.as_deref(),
            Some("the real first prompt")
        );
    }

    #[test]
    fn parse_meta_truncates_long_first_message() {
        let long = "a".repeat(200);
        let content = format!(
            "{HEADER}\n{{\"type\":\"message\",\"id\":\"a1\",\"message\":{{\"role\":\"user\",\"content\":\"{long}\"}}}}\n",
        );
        let title = pi().parse_meta(&content).first_user_message.unwrap();
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), FIRST_USER_MSG_MAX_CHARS + 1);
    }

    #[test]
    fn parse_meta_empty_and_malformed_are_default() {
        assert_eq!(pi().parse_meta(""), TranscriptMeta::default());
        assert_eq!(
            pi().parse_meta("not json\n{also not\n"),
            TranscriptMeta::default()
        );
    }

    #[test]
    fn parse_meta_tolerates_unknown_entry_types() {
        let content = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            r#"{"type":"model_change","id":"m1","model":"x"}"#,
            r#"{"type":"thinking_level_change","id":"t1"}"#,
            r#"{"type":"totally_new_future_type","id":"z1"}"#,
        );
        assert_eq!(
            pi().parse_meta(&content).cwd,
            Some(PathBuf::from("/work/proj"))
        );
    }

    // ---- derive: attention ----

    fn derive(content: &str) -> AgentDerivation {
        pi().derive(content, Path::new("/work/proj"))
    }

    fn assistant(stop_reason: &str) -> String {
        format!(
            r#"{{"type":"message","id":"a1","message":{{"role":"assistant","content":"ok","stopReason":"{stop_reason}"}}}}"#,
        )
    }

    fn user_msg() -> String {
        r#"{"type":"message","id":"u1","message":{"role":"user","content":"do thing"}}"#.to_string()
    }

    #[test]
    fn derive_empty_is_unknown() {
        assert_eq!(derive("").attention, Attention::Unknown);
        assert!(!derive("").from_tool_use);
    }

    #[test]
    fn derive_assistant_tool_use_is_working_and_flags_from_tool_use() {
        let d = derive(&assistant("toolUse"));
        assert_eq!(d.attention, Attention::Working);
        assert!(d.from_tool_use);
    }

    #[test]
    fn derive_assistant_stop_is_needs_input() {
        let d = derive(&assistant("stop"));
        assert_eq!(d.attention, Attention::NeedsInput);
        assert!(!d.from_tool_use);
    }

    #[test]
    fn derive_assistant_error_and_aborted_are_needs_input() {
        assert_eq!(derive(&assistant("error")).attention, Attention::NeedsInput);
        assert_eq!(
            derive(&assistant("aborted")).attention,
            Attention::NeedsInput
        );
    }

    #[test]
    fn derive_assistant_missing_stop_reason_is_needs_input() {
        let d =
            derive(r#"{"type":"message","id":"a1","message":{"role":"assistant","content":"ok"}}"#);
        assert_eq!(d.attention, Attention::NeedsInput);
        assert!(!d.from_tool_use);
    }

    #[test]
    fn derive_assistant_unknown_stop_reason_falls_back_to_needs_input() {
        assert_eq!(
            derive(&assistant("length")).attention,
            Attention::NeedsInput
        );
        assert_eq!(
            derive(&assistant("some_future_reason")).attention,
            Attention::NeedsInput
        );
    }

    #[test]
    fn derive_last_user_message_is_working() {
        let content = format!("{}\n{}\n", assistant("stop"), user_msg());
        assert_eq!(derive(&content).attention, Attention::Working);
    }

    #[test]
    fn derive_tool_result_tail_is_working() {
        let content = format!(
            "{}\n{}\n",
            assistant("toolUse"),
            r#"{"type":"message","id":"tr1","message":{"role":"toolResult","content":"ok"}}"#,
        );
        let d = derive(&content);
        assert_eq!(d.attention, Attention::Working);
        // toolResult is progress, not a live tool call.
        assert!(!d.from_tool_use);
    }

    #[test]
    fn derive_bash_execution_tail_is_working() {
        let content = format!(
            "{}\n{}\n",
            assistant("toolUse"),
            r#"{"type":"message","id":"b1","message":{"role":"bashExecution","content":"$ ls"}}"#,
        );
        assert_eq!(derive(&content).attention, Attention::Working);
    }

    #[test]
    fn derive_uses_last_message_over_earlier_ones() {
        // user → assistant(toolUse) → assistant(stop): the session awaits.
        let content = format!(
            "{}\n{}\n{}\n",
            user_msg(),
            assistant("toolUse"),
            assistant("stop"),
        );
        assert_eq!(derive(&content).attention, Attention::NeedsInput);
    }

    #[test]
    fn derive_ignores_unknown_roles_and_entry_types() {
        // A custom-role message and a non-message entry after the real tail
        // must not disturb the last-classifiable-message decision.
        let content = format!(
            "{}\n{}\n{}\n",
            assistant("toolUse"),
            r#"{"type":"message","id":"c1","message":{"role":"custom","content":"noise"}}"#,
            r#"{"type":"label","id":"l1","name":"x"}"#,
        );
        let d = derive(&content);
        assert_eq!(d.attention, Attention::Working);
        assert!(d.from_tool_use);
    }

    #[test]
    fn derive_skips_partial_first_line() {
        // A tail window starting mid-JSON-line: the first (partial) line
        // fails to parse and drops; the real messages after it still classify.
        let content = format!(
            "Reason\":\"stop\"}}}}\n{}\n{}\n",
            user_msg(),
            assistant("stop"),
        );
        assert_eq!(derive(&content).attention, Attention::NeedsInput);
    }

    // ---- derive: edited files ----

    fn tool_call(name: &str, path_key: &str, path: &str) -> String {
        format!(
            r#"{{"type":"message","id":"a1","message":{{"role":"assistant","stopReason":"toolUse","content":[{{"type":"toolCall","id":"c1","name":"{name}","arguments":{{"{path_key}":"{path}"}}}}]}}}}"#,
        )
    }

    #[test]
    fn edited_files_from_write_and_edit_most_recent_first() {
        let content = format!(
            "{}\n{}\n",
            tool_call("write", "path", "/work/proj/a.rs"),
            tool_call("edit", "path", "/work/proj/b.rs"),
        );
        assert_eq!(
            derive(&content).edited_files,
            vec![
                PathBuf::from("/work/proj/b.rs"),
                PathBuf::from("/work/proj/a.rs"),
            ]
        );
    }

    #[test]
    fn edited_files_resolves_relative_path_against_passed_cwd() {
        let content = format!("{}\n", tool_call("edit", "path", "src/lib.rs"));
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/work/proj/src/lib.rs")]
        );
    }

    #[test]
    fn edited_files_tolerates_file_path_alias() {
        let content = format!(
            "{}\n",
            tool_call("write", "file_path", "/work/proj/aliased.rs")
        );
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/work/proj/aliased.rs")]
        );
    }

    #[test]
    fn edited_files_absolute_path_unchanged_by_cwd() {
        let content = format!("{}\n", tool_call("edit", "path", "/absolute/x.rs"));
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/absolute/x.rs")]
        );
    }

    #[test]
    fn edited_files_relative_resolves_against_header_cwd_when_passed_cwd_empty() {
        // The watcher path: derive is called with an EMPTY cwd placeholder,
        // but the scanned window contains the header, so relative paths
        // resolve against the header cwd.
        let content = format!("{HEADER}\n{}\n", tool_call("edit", "path", "src/lib.rs"));
        let d = pi().derive(&content, Path::new(""));
        assert_eq!(d.edited_files, vec![PathBuf::from("/work/proj/src/lib.rs")]);
    }

    #[test]
    fn edited_files_relative_pushed_as_is_when_no_cwd_available() {
        // Empty passed cwd AND no header in the window: the relative path is
        // pushed unresolved (documented degradation).
        let content = format!("{}\n", tool_call("edit", "path", "src/lib.rs"));
        let d = pi().derive(&content, Path::new(""));
        assert_eq!(d.edited_files, vec![PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn edited_files_ignores_non_edit_tool_calls() {
        let content = format!(
            "{}\n{}\n",
            tool_call("read", "path", "/work/proj/looked.rs"),
            tool_call("edit", "path", "/work/proj/changed.rs"),
        );
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/work/proj/changed.rs")]
        );
    }

    #[test]
    fn edited_files_dedup_moves_reedited_to_front() {
        let content = format!(
            "{}\n{}\n{}\n",
            tool_call("edit", "path", "/w/a.rs"),
            tool_call("edit", "path", "/w/b.rs"),
            tool_call("edit", "path", "/w/a.rs"),
        );
        assert_eq!(
            pi().derive(&content, Path::new("/w")).edited_files,
            vec![PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")]
        );
    }

    #[test]
    fn edited_files_empty_when_no_tool_calls() {
        assert!(derive(&assistant("stop")).edited_files.is_empty());
    }

    #[test]
    fn edited_files_caps_at_the_limit() {
        let lines: Vec<String> = (0..(EDITED_FILES_CAP + 20))
            .map(|i| tool_call("edit", "path", &format!("/w/f{i}.rs")))
            .collect();
        let files = pi().derive(&lines.join("\n"), Path::new("/w")).edited_files;
        assert_eq!(files.len(), EDITED_FILES_CAP);
        assert_eq!(
            files[0],
            PathBuf::from(format!("/w/f{}.rs", EDITED_FILES_CAP + 19))
        );
    }

    #[test]
    fn derive_carries_attention_and_edits_from_one_walk() {
        let content = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            user_msg(),
            tool_call("edit", "path", "/work/proj/x.rs"),
            assistant("stop"),
        );
        let d = derive(&content);
        assert_eq!(d.attention, Attention::NeedsInput);
        assert_eq!(d.edited_files, vec![PathBuf::from("/work/proj/x.rs")]);
    }
}
