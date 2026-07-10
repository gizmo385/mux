//! Codex CLI (`OpenAI`) — the read path (discovery + rollout parser).
//! WP3 fills the on-disk-tree predicate, the id-from-path derivation, and
//! the head/tail JSONL parsers; spawn/resume (the `DiscoverAfterSpawn`
//! adoption protocol) lands in WP5.
//!
//! Codex rollouts live at `~/.codex/sessions/YYYY/MM/DD/rollout-<local-ts>-<uuid>.jsonl`
//! (Appendix A of the multi-agent plan, researched 2026-07-09 against
//! rust-v0.144.1). Each line is `{"timestamp","type","payload"}` with
//! `type` ∈ `session_meta | response_item | compacted | turn_context |
//! world_state | event_msg | inter_agent_communication…`. The one field we
//! anchor attention on — the turn-lifecycle events nested under an
//! `event_msg` payload, spelled `task_started` / `task_complete` /
//! `task_aborted` in the shipping 0.142.5 writer (verified 2026-07-10) and
//! `turn_started` / `turn_complete` / `turn_aborted` in the rust-v0.144.1
//! research; the parser matches both — is persisted in *both* history modes
//! (legacy and paginated), which is the
//! churn-resilience bet: Codex releases ~daily and the rollout schema has
//! already broken compatibly several times, so the parser ignores every
//! record type and event type it does not recognise *silently* and keys
//! only on the handful documented as stable.
//!
//! Documented gaps at WP3 (plan §2.5): `.jsonl.zst` cold rollouts are
//! outside the listing glob (skipped); "blocked on approval" is invisible
//! in the rollout (approvals are never persisted) so it reads as `Working`
//! until the WP8 lifecycle-hook path lands; shell-command file edits (not
//! via `apply_patch`) are not captured.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::agent::{AgentCli, AgentDerivation, AgentKind, ListingSpec, SpawnPlan, TranscriptMeta};
use crate::session::{Attention, EDITED_FILES_CAP, SessionId};

/// Length of a canonical UUID string (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`),
/// the trailing component of a codex rollout filename.
const UUID_LEN: usize = 36;

/// Max display length (in chars) for the first-user-message title fallback.
/// Kept in sync with the Claude reference impl's cap so a codex row and a
/// claude row truncate identically on the same-width dashboard.
const FIRST_USER_MSG_MAX_CHARS: usize = 60;

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAgent;

impl AgentCli for CodexAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn label(&self) -> &'static str {
        "codex"
    }

    fn default_binary(&self) -> &'static str {
        "codex"
    }

    fn default_transcript_root(&self) -> Option<PathBuf> {
        // WP2: the root shape is needed so multi-root discovery/watch
        // plumbing can resolve and list codex's tree.
        dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
    }

    fn listing(&self) -> ListingSpec {
        // <root>/YYYY/MM/DD/rollout-*.jsonl — files exactly four levels
        // below the root (Appendix A). `.jsonl.zst` cold rollouts fall
        // outside the glob (documented gap, plan §2.5).
        ListingSpec {
            mindepth: 4,
            maxdepth: 4,
            name_glob: "rollout-*.jsonl",
        }
    }

    fn is_transcript(&self, path: &Path, root: &Path) -> bool {
        is_top_level_transcript(path, root)
    }

    fn session_id_from_path(&self, path: &Path) -> Option<SessionId> {
        // `rollout-<local-ts>-<uuid>.jsonl`; the timestamp itself carries
        // dashes (`2026-07-09T14-23-05`), so the uuid can't be found by
        // splitting on `-` — it's the trailing 36 chars of the stem,
        // validated loosely as uuid-shaped so a truncated or unfamiliar
        // filename yields `None` rather than a bogus id.
        let stem = path.file_stem()?.to_str()?;
        if !stem.starts_with("rollout-") {
            return None;
        }
        let start = stem.len().checked_sub(UUID_LEN)?;
        let candidate = stem.get(start..)?;
        if !is_uuid_shaped(candidate) {
            return None;
        }
        Some(SessionId(candidate.to_string()))
    }

    fn fallback_dir(&self, _transcript_path: &Path) -> PathBuf {
        // Unlike Claude's tree, the codex path (`YYYY/MM/DD/rollout-…`)
        // encodes no cwd, so there is nothing to decode. Line 1
        // (`session_meta`) always carries `cwd`, so this is only reached
        // for a transcript missing its header — which discovery's is_dir
        // filter then drops, since `<unknown>` is not a real directory.
        PathBuf::from("<unknown>")
    }

    fn parse_meta(&self, content: &str) -> TranscriptMeta {
        parse_meta_impl(content)
    }

    fn derive(&self, content: &str, cwd: &Path) -> AgentDerivation {
        derive_from_content(content, cwd)
    }

    fn spawn(&self, _cwd: &Path, _minted_id: &SessionId) -> SpawnPlan {
        // Codex refuses id pinning (upstream declined `--session-id`,
        // plan Appendix A / §2.4), so there is no flag to carry
        // `minted_id` — the launch argv is the bare binary. The
        // Attachment Driver spawns it under a provisional
        // `agent-mux-pending-<nonce>` tmux name and the real id is
        // *adopted* from the rollout that appears (the WP5 correlation
        // protocol in `main.rs`/`adoption.rs`). NEVER `--ephemeral`:
        // that suppresses the rollout we correlate on (Appendix A).
        SpawnPlan::DiscoverAfterSpawn {
            argv: vec!["codex".to_string()],
        }
    }

    fn resume_command(&self, id: &SessionId) -> String {
        // `codex resume <id>` takes the id directly — the tmux resume
        // fallback wraps this in `tmux new-session -A -s agent-mux-<id>
        // -c <cwd> …` via the shared `tmux_resume_argv` machinery.
        format!("codex resume {}", id.0)
    }
}

// ---------- transcript-tree shape ----------

/// True iff `path` is `<root>/YYYY/MM/DD/rollout-*.jsonl`: exactly four
/// components below `root`, the first three numeric (the date dirs — a
/// cheap numeric check, not a full date parse), a filename beginning
/// `rollout-` and ending `.jsonl`. The `.jsonl` suffix check is what
/// rejects the `.jsonl.zst` cold rollouts (their name ends `.zst`), and
/// the four-component check rejects anything shallower or deeper. Mirrors
/// the depth-4 listing glob so startup and live discovery filter
/// identically.
fn is_top_level_transcript(path: &Path, root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let mut parts: Vec<&str> = Vec::with_capacity(4);
    for component in rel.components() {
        let Component::Normal(os) = component else {
            return false;
        };
        let Some(s) = os.to_str() else {
            return false;
        };
        parts.push(s);
    }
    if parts.len() != 4 {
        return false;
    }
    let date_dirs_numeric = parts[..3]
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !date_dirs_numeric {
        return false;
    }
    let name = parts[3];
    // `.jsonl` (not `.jsonl.zst`, whose extension is `zst`) — the
    // case-insensitive `Path::extension` form keeps clippy happy and still
    // rejects the cold-compressed rollouts.
    name.starts_with("rollout-")
        && Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

/// Loose UUID-shape check: 36 chars, dashes at 8/13/18/23, hex elsewhere.
/// Deliberately not a full RFC-4122 parse — the point is to reject a
/// filename whose trailing 36 chars are a timestamp fragment rather than
/// an id, not to validate variant/version bits.
fn is_uuid_shaped(s: &str) -> bool {
    if s.len() != UUID_LEN {
        return false;
    }
    s.bytes().enumerate().all(|(i, b)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            b == b'-'
        } else {
            b.is_ascii_hexdigit()
        }
    })
}

// ---------- head-of-file metadata parse ----------

/// Single pass over the transcript head: take `cwd` from the first line
/// carrying one (`session_meta`, or a later `turn_context` refinement),
/// and the first human user message for the title fallback. Codex writes
/// no title record, so [`TranscriptMeta::title`] stays `None` — discovery
/// falls through to `first_user_message`. Malformed / unrecognised lines
/// are skipped.
fn parse_meta_impl(content: &str) -> TranscriptMeta {
    let mut meta = TranscriptMeta::default();
    // A user-role `response_item` is the last-resort title source, used
    // only when neither a `user_message` event (legacy) nor an
    // `item_completed` `UserMessage` (paginated) is present. Tracked
    // separately so event-based sources always win regardless of file
    // order.
    let mut response_item_fallback: Option<String> = None;
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if meta.cwd.is_none()
            && let Some(cwd) = payload_field(&value, "cwd").and_then(Value::as_str)
        {
            meta.cwd = Some(PathBuf::from(cwd));
        }
        if meta.first_user_message.is_none() {
            if let Some(text) = user_message_text(&value) {
                if !text.trim().is_empty() {
                    meta.first_user_message = Some(normalize_for_title(&text));
                }
            } else if response_item_fallback.is_none()
                && let Some(text) = response_item_user_text(&value)
                && !text.trim().is_empty()
            {
                response_item_fallback = Some(normalize_for_title(&text));
            }
        }
    }
    if meta.first_user_message.is_none() {
        meta.first_user_message = response_item_fallback;
    }
    meta
}

/// Human user-message text for the title fallback, from either history
/// mode: a legacy `event_msg` of type `user_message`, or a paginated
/// `item_completed` wrapping a `UserMessage` item. `None` for any other
/// line. Field extraction is nesting-tolerant (see [`event_text`]) because
/// the exact `message` vs `payload.message` placement has drifted across
/// releases.
fn user_message_text(line: &Value) -> Option<String> {
    let event = event_msg_payload(line)?;
    match event_type(event)? {
        "user_message" => event_text(event),
        "item_completed" => {
            let item = event_field(event, "item")?;
            if item_type_is(item, "user_message") {
                event_text(item)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Last-resort title text: a `response_item` carrying a user-role message
/// (Codex records conversation items in the Responses API shape —
/// `payload.role == "user"`, text under `content[].text`).
fn response_item_user_text(line: &Value) -> Option<String> {
    if line.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = line.get("payload")?;
    if payload.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    event_text(payload)
}

/// Collapse whitespace runs to single spaces, trim, and truncate to
/// [`FIRST_USER_MSG_MAX_CHARS`] chars with an ellipsis when shortened.
/// Same shape as the Claude reference impl's title normaliser (each agent
/// module keeps its own copy — the trait surface is deliberately narrow
/// and this is a few lines).
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

/// The three turn-lifecycle states we can read from the rollout. Persisted
/// in both history modes, so this is the churn-stable attention signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    /// An open `task_started`/`turn_started` with no later
    /// complete/abort — the agent is working (or blocked on an approval,
    /// which the rollout can't distinguish; §2.5).
    Working,
    /// `task_complete`/`turn_complete` or `task_aborted`/`turn_aborted` —
    /// the turn ended, the session is awaiting the next human prompt.
    NeedsInput,
}

/// Walk every parseable line once: track the *last* turn-lifecycle event
/// for attention, and accumulate edited-file paths from `patch_apply_end`
/// (legacy) and `item_completed` `FileChange` (paginated) events. Unknown
/// record types and unknown `event_msg` types are ignored silently at both
/// nesting levels — churn resilience is the design requirement. A tail
/// window that begins mid-JSON-line drops its first (unparseable) line via
/// the same `continue`, exactly like the Claude parser.
fn derive_from_content(content: &str, cwd: &Path) -> AgentDerivation {
    let mut last_turn: Option<TurnState> = None;
    let mut chronological: Vec<PathBuf> = Vec::new();
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(event) = event_msg_payload(&value) else {
            continue;
        };
        let Some(event_type) = event_type(event) else {
            continue;
        };
        match event_type {
            // Turn-lifecycle events. Codex has renamed these across releases:
            // the plan's Appendix-A research (rust-v0.144.1) named `turn_*`,
            // but the shipping 0.142.5 writer emits `task_started` /
            // `task_complete` (each carrying a `turn_id` field) — confirmed
            // 2026-07-10 against a real `codex exec` rollout. Both spellings
            // are matched so attention survives the rename in either
            // direction, which is exactly the ~daily schema churn this parser
            // is built to absorb.
            "turn_started" | "task_started" => last_turn = Some(TurnState::Working),
            "turn_complete" | "turn_aborted" | "task_complete" | "task_aborted" => {
                last_turn = Some(TurnState::NeedsInput);
            }
            // Legacy mode: `patch_apply_end` carries a `changes` map keyed
            // by edited path.
            "patch_apply_end" => collect_changes(event, cwd, &mut chronological),
            // Paginated mode: a completed `FileChange` item carries the
            // same `changes` map.
            "item_completed" => collect_item_completed(event, cwd, &mut chronological),
            _ => {}
        }
    }
    let attention = match last_turn {
        Some(TurnState::Working) => Attention::Working,
        Some(TurnState::NeedsInput) => Attention::NeedsInput,
        // No turn event in the window — nothing parseable, same semantics
        // as the Claude parser's `None` case.
        None => Attention::Unknown,
    };
    AgentDerivation {
        attention,
        // An *open* turn (`turn_started` with no matching end) reports
        // `from_tool_use = true` — WP8. Semantically it is claude's
        // `tool_use` wait: the agent is mid-work awaiting something that
        // is NOT transcript-visible user input (a tool result, or an
        // approval the rollout never records). The catalog's
        // `apply_heuristic_attention` uses this discriminator to protect
        // a live `PermissionRequest` hook pin — without it, every poll
        // during a blocked approval would derive Working with
        // `from_tool_use = false`, clear the pin, and flicker the row out
        // of "blocked" (the exact clobber claude's discriminator fixed).
        // A completed/aborted turn (NeedsInput) is genuine progress past
        // the prompt, so it stays `false` and releases the pin.
        from_tool_use: matches!(last_turn, Some(TurnState::Working)),
        edited_files: dedup_most_recent_first(&chronological),
    }
}

/// Push each path key of an `event_msg` payload's `changes` map (the
/// legacy `patch_apply_end` shape) onto `acc`, resolved against `cwd` so a
/// relative key lands as an absolute path (an absolute key is unchanged by
/// the join).
fn collect_changes(event: &Value, cwd: &Path, acc: &mut Vec<PathBuf>) {
    let Some(changes) = event_field(event, "changes").and_then(Value::as_object) else {
        return;
    };
    for key in changes.keys() {
        acc.push(cwd.join(key));
    }
}

/// Push the edited paths from a paginated `item_completed` event whose item
/// is a `FileChange` (its `changes` map keyed by path). Non-`FileChange`
/// items (`UserMessage`, `AgentMessage`, `CommandExecution`, …) contribute
/// nothing.
fn collect_item_completed(event: &Value, cwd: &Path, acc: &mut Vec<PathBuf>) {
    let Some(item) = event_field(event, "item") else {
        return;
    };
    if !item_type_is(item, "file_change") {
        return;
    }
    let Some(changes) = item.get("changes").and_then(Value::as_object) else {
        return;
    };
    for key in changes.keys() {
        acc.push(cwd.join(key));
    }
}

/// Dedup a chronological (oldest-first) path list into most-recent-first,
/// keeping each path's latest occurrence, capped at [`EDITED_FILES_CAP`].
/// Same strategy as the Claude reference impl.
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

// ---------- shared JSON-shape helpers ----------

/// The `EventMsg` object of an `event_msg` rollout line, or `None` for any
/// other record type.
fn event_msg_payload(line: &Value) -> Option<&Value> {
    if line.get("type").and_then(Value::as_str) == Some("event_msg") {
        line.get("payload")
    } else {
        None
    }
}

/// The `type` discriminator of an `EventMsg` (`turn_started`,
/// `user_message`, …).
fn event_type(event: &Value) -> Option<&str> {
    event.get("type").and_then(Value::as_str)
}

/// Look a field up on an `EventMsg`, tolerating the one-more-level-of-
/// `payload` nesting the schema has carried in some releases: try
/// `event.<key>` first, then `event.payload.<key>`. This is what keeps the
/// parser working across the `payload.message` vs `payload.payload.message`
/// drift documented for `user_message`.
fn event_field<'a>(event: &'a Value, key: &str) -> Option<&'a Value> {
    event
        .get(key)
        .or_else(|| event.get("payload").and_then(|p| p.get(key)))
}

/// A `cwd`-style field that lives under a line's `payload` (`session_meta`,
/// `turn_context`), falling back to a top-level key for tolerance.
fn payload_field<'a>(line: &'a Value, key: &str) -> Option<&'a Value> {
    line.get("payload")
        .and_then(|p| p.get(key))
        .or_else(|| line.get(key))
}

/// Extract message text from an object that may hold it as a `message` /
/// `text` string (directly or under a nested `payload`), or as a `content`
/// array of `{type, text}` blocks (the Responses-API item shape). `None`
/// when no text is found.
fn event_text(obj: &Value) -> Option<String> {
    for candidate in [Some(obj), obj.get("payload")].into_iter().flatten() {
        for key in ["message", "text"] {
            if let Some(s) = candidate.get(key).and_then(Value::as_str)
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
        }
        if let Some(text) = content_array_text(candidate.get("content")) {
            return Some(text);
        }
    }
    None
}

/// Join the `text` of every `{type, text}` block in a `content` array into
/// one space-separated string (non-text blocks skipped), or `None` if the
/// value isn't a content array or holds no text.
fn content_array_text(content: Option<&Value>) -> Option<String> {
    let arr = content?.as_array()?;
    let mut buf = String::new();
    for block in arr {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(text);
        }
    }
    (!buf.is_empty()).then_some(buf)
}

/// True if a `TurnItem`'s type tag matches `target` after normalising away
/// case and underscores (`FileChange`, `file_change`, `FILE_CHANGE` all
/// match `"file_change"`). Item-level tags are matched loosely because,
/// unlike the well-documented `event_msg` types, their exact serde casing
/// is less certain and has room to drift.
fn item_type_is(item: &Value, target: &str) -> bool {
    let Some(tag) = item
        .get("type")
        .or_else(|| item.get("item_type"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    normalize_tag(tag) == normalize_tag(target)
}

/// Lowercase and strip underscores/hyphens for loose tag comparison.
fn normalize_tag(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    //! The JSONL fixtures below are **synthetic**, authored from the
    //! researched Codex rollout schema (multi-agent plan Appendix A,
    //! researched 2026-07-09 against rust-v0.144.1) — no real `codex`
    //! installation was available on the build machine to capture from.
    //! They pin the record/field shapes the parser depends on so an
    //! upstream schema break fails loudly here rather than silently in the
    //! field.
    use super::*;

    fn codex() -> CodexAgent {
        CodexAgent
    }

    // ---- is_transcript ----

    const ROOT: &str = "/home/me/.codex/sessions";

    fn rollout_path(rest: &str) -> PathBuf {
        Path::new(ROOT).join(rest)
    }

    #[test]
    fn is_transcript_accepts_dated_rollout() {
        assert!(codex().is_transcript(
            &rollout_path(
                "2026/07/09/rollout-2026-07-09T14-23-05-00000000-1111-2222-3333-444444444444.jsonl"
            ),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_zst_cold_rollout() {
        // The documented skip (§2.5): a zstd-compressed cold rollout ends
        // `.jsonl.zst`, so the `.jsonl` suffix check drops it.
        assert!(!codex().is_transcript(
            &rollout_path("2026/07/09/rollout-2026-07-09T14-23-05-00000000-1111-2222-3333-444444444444.jsonl.zst"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_wrong_depth() {
        // Too shallow (3 components) and too deep (5 components).
        assert!(!codex().is_transcript(&rollout_path("2026/07/rollout-x.jsonl"), Path::new(ROOT),));
        assert!(!codex().is_transcript(
            &rollout_path("2026/07/09/extra/rollout-x.jsonl"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_non_numeric_date_dir() {
        assert!(!codex().is_transcript(
            &rollout_path("2026/July/09/rollout-x.jsonl"),
            Path::new(ROOT),
        ));
    }

    #[test]
    fn is_transcript_rejects_wrong_prefix_or_extension() {
        assert!(
            !codex().is_transcript(&rollout_path("2026/07/09/session-x.jsonl"), Path::new(ROOT),)
        );
        assert!(
            !codex().is_transcript(&rollout_path("2026/07/09/rollout-x.txt"), Path::new(ROOT),)
        );
    }

    #[test]
    fn is_transcript_rejects_paths_outside_root() {
        assert!(!codex().is_transcript(
            Path::new("/elsewhere/2026/07/09/rollout-x.jsonl"),
            Path::new(ROOT),
        ));
    }

    // ---- session_id_from_path ----

    #[test]
    fn session_id_parses_trailing_uuid_past_the_dashed_timestamp() {
        // The real shape: the local timestamp `2026-07-09T14-23-05` is full
        // of dashes, so the uuid must come from the END, not a `-` split.
        let id = codex().session_id_from_path(&rollout_path(
            "2026/07/09/rollout-2026-07-09T14-23-05-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl",
        ));
        assert_eq!(
            id,
            Some(SessionId(
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()
            ))
        );
    }

    #[test]
    fn session_id_rejects_non_uuid_tail() {
        // Trailing 36 chars are not uuid-shaped (a timestamp fragment).
        assert_eq!(
            codex().session_id_from_path(Path::new("/r/rollout-not-a-uuid-here.jsonl")),
            None
        );
    }

    #[test]
    fn session_id_rejects_missing_prefix() {
        assert_eq!(
            codex().session_id_from_path(Path::new(
                "/r/notrollout-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"
            )),
            None
        );
    }

    // ---- spawn / resume ----

    #[test]
    fn spawn_is_discover_after_spawn_with_bare_binary_argv() {
        // Codex can't pin an id, so the launch tail is the bare binary —
        // no `--session-id`, never `--ephemeral`. The provisional tmux
        // name + adoption live in the Attachment Driver / main loop.
        let plan = codex().spawn(Path::new("/w"), &SessionId("x".to_string()));
        match plan {
            SpawnPlan::DiscoverAfterSpawn { argv } => assert_eq!(argv, vec!["codex".to_string()]),
            SpawnPlan::PinnedId { .. } => panic!("codex must not pin an id"),
        }
    }

    #[test]
    fn resume_command_is_codex_resume_id() {
        assert_eq!(
            codex().resume_command(&SessionId("abc".to_string())),
            "codex resume abc"
        );
    }

    // ---- parse_meta ----

    const SESSION_META: &str = r#"{"timestamp":"2026-07-09T14:23:05Z","type":"session_meta","payload":{"id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","session_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","cwd":"/work/proj","originator":"cli","cli_version":"0.144.1","source":"tui","history_mode":"legacy","git":{"branch":"main"}}}"#;

    #[test]
    fn parse_meta_legacy_extracts_cwd_and_first_user_message() {
        let content = format!(
            "{SESSION_META}\n{}\n",
            r#"{"timestamp":"t","type":"event_msg","payload":{"type":"user_message","payload":{"message":"refactor the parser"}}}"#,
        );
        let meta = codex().parse_meta(&content);
        assert_eq!(meta.cwd, Some(PathBuf::from("/work/proj")));
        assert_eq!(meta.title, None); // codex writes no title record
        assert_eq!(
            meta.first_user_message.as_deref(),
            Some("refactor the parser")
        );
    }

    #[test]
    fn parse_meta_tolerates_unnested_user_message_field() {
        // Churn tolerance: `message` directly on the EventMsg rather than
        // under a second `payload`.
        let content = format!(
            "{SESSION_META}\n{}\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"hi there"}}"#,
        );
        assert_eq!(
            codex().parse_meta(&content).first_user_message.as_deref(),
            Some("hi there")
        );
    }

    #[test]
    fn parse_meta_paginated_reads_item_completed_user_message() {
        let content = format!(
            "{SESSION_META}\n{}\n",
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"user_message","text":"paginated prompt"}}}"#,
        );
        assert_eq!(
            codex().parse_meta(&content).first_user_message.as_deref(),
            Some("paginated prompt")
        );
    }

    #[test]
    fn parse_meta_falls_back_to_user_role_response_item() {
        // No user_message / item_completed present — a user-role
        // response_item is the last resort, and only that.
        let content = format!(
            "{SESSION_META}\n{}\n",
            r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"response item prompt"}]}}"#,
        );
        assert_eq!(
            codex().parse_meta(&content).first_user_message.as_deref(),
            Some("response item prompt")
        );
    }

    #[test]
    fn parse_meta_prefers_event_over_response_item_regardless_of_order() {
        // response_item appears first in the file, but the user_message
        // event must still win.
        let content = format!(
            "{SESSION_META}\n{}\n{}\n",
            r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"raw item"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","payload":{"message":"clean event text"}}}"#,
        );
        assert_eq!(
            codex().parse_meta(&content).first_user_message.as_deref(),
            Some("clean event text")
        );
    }

    #[test]
    fn parse_meta_truncates_long_first_message() {
        let long = "a".repeat(200);
        let content = format!(
            "{SESSION_META}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"payload\":{{\"message\":\"{long}\"}}}}}}\n",
        );
        let title = codex().parse_meta(&content).first_user_message.unwrap();
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), FIRST_USER_MSG_MAX_CHARS + 1);
    }

    #[test]
    fn parse_meta_empty_and_malformed_are_none() {
        assert_eq!(codex().parse_meta(""), TranscriptMeta::default());
        assert_eq!(
            codex().parse_meta("not json\n{also not\n"),
            TranscriptMeta::default()
        );
    }

    #[test]
    fn parse_meta_tolerates_unknown_record_types() {
        // world_state / inter_agent_communication / compacted / a
        // hypothetical future type must not break cwd or title extraction.
        let content = format!(
            "{}\n{SESSION_META}\n{}\n{}\n",
            r#"{"type":"world_state","payload":{"foo":1}}"#,
            r#"{"type":"inter_agent_communication","payload":{"bar":2}}"#,
            r#"{"type":"totally_new_future_type","payload":{"type":"also_new"}}"#,
        );
        let meta = codex().parse_meta(&content);
        assert_eq!(meta.cwd, Some(PathBuf::from("/work/proj")));
    }

    // ---- derive: attention ----

    fn derive(content: &str) -> AgentDerivation {
        codex().derive(content, Path::new("/work/proj"))
    }

    fn ev(event_type: &str) -> String {
        format!(r#"{{"type":"event_msg","payload":{{"type":"{event_type}"}}}}"#)
    }

    #[test]
    fn derive_empty_is_unknown() {
        assert_eq!(derive("").attention, Attention::Unknown);
        assert!(!derive("").from_tool_use);
    }

    #[test]
    fn derive_open_turn_started_is_working() {
        let content = format!("{SESSION_META}\n{}\n", ev("turn_started"));
        let d = derive(&content);
        assert_eq!(d.attention, Attention::Working);
        // WP8: an open turn now reports `from_tool_use = true` — it's the
        // semantic twin of claude's `tool_use` wait (mid-work, awaiting a
        // tool result or an approval the rollout can't show). This is what
        // lets the catalog protect a live `PermissionRequest` hook pin from
        // being clobbered by an open-turn Working poll (see
        // `derive_from_content` and the catalog clobber regression tests).
        assert!(d.from_tool_use);
    }

    #[test]
    fn derive_turn_complete_is_needs_input() {
        let content = format!("{}\n{}\n", ev("turn_started"), ev("turn_complete"));
        assert_eq!(derive(&content).attention, Attention::NeedsInput);
    }

    #[test]
    fn derive_turn_aborted_is_needs_input() {
        let content = format!("{}\n{}\n", ev("turn_started"), ev("turn_aborted"));
        assert_eq!(derive(&content).attention, Attention::NeedsInput);
    }

    #[test]
    fn derive_uses_last_turn_event() {
        // started → complete → started again: the session is Working again.
        let content = format!(
            "{}\n{}\n{}\n",
            ev("turn_started"),
            ev("turn_complete"),
            ev("turn_started"),
        );
        assert_eq!(derive(&content).attention, Attention::Working);
    }

    // ---- derive: real 0.142.5 `task_*` event names (regression) ----
    //
    // The shipping 0.142.5 writer emits `task_started` / `task_complete`
    // (not the `turn_*` the rust-v0.144.1 research named). These lines are
    // copied verbatim from a real `codex exec` rollout captured 2026-07-10;
    // before the fix, `derive_from_content` matched only `turn_*`, so a
    // completed real session derived `Attention::Unknown` and rendered as
    // `· unknown` in the sidebar instead of `needs input`. Keep both spellings
    // matched — Codex renames these across releases and the parser must
    // survive the churn in either direction.

    /// A real `task_started` event line from codex 0.142.5.
    const REAL_TASK_STARTED: &str = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"019f4cd2-58b4-7033-87f4-2ddebb646c18","started_at":1783700281,"model_context_window":258400,"collaboration_mode_kind":"default"}}"#;
    /// A real `task_complete` event line from codex 0.142.5.
    const REAL_TASK_COMPLETE: &str = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"019f4cd2-58b4-7033-87f4-2ddebb646c18","last_agent_message":null,"completed_at":1783700298,"duration_ms":17336}}"#;

    #[test]
    fn derive_real_task_started_is_working() {
        let d = derive(&format!("{REAL_TASK_STARTED}\n"));
        assert_eq!(d.attention, Attention::Working);
        // An open task, like an open turn, is the semantic twin of claude's
        // `tool_use` wait — it must protect a live hook pin.
        assert!(d.from_tool_use);
    }

    #[test]
    fn derive_real_task_complete_is_needs_input() {
        // The exact regression: task_started then task_complete, real shapes.
        let content = format!("{REAL_TASK_STARTED}\n{REAL_TASK_COMPLETE}\n");
        let d = derive(&content);
        assert_eq!(d.attention, Attention::NeedsInput);
        assert!(!d.from_tool_use);
    }

    #[test]
    fn derive_task_aborted_is_needs_input() {
        let content = format!("{}\n{}\n", ev("task_started"), ev("task_aborted"));
        assert_eq!(derive(&content).attention, Attention::NeedsInput);
    }

    #[test]
    fn derive_uses_last_event_across_mixed_task_and_turn_spellings() {
        // A rollout that straddles a rename (turn_* then task_*) still tracks
        // the last lifecycle event regardless of spelling.
        let content = format!(
            "{}\n{}\n{}\n",
            ev("turn_started"),
            ev("turn_complete"),
            ev("task_started"),
        );
        assert_eq!(derive(&content).attention, Attention::Working);
    }

    #[test]
    fn derive_ignores_non_turn_events_and_unknown_types() {
        // token_count / a future event type / a non-event_msg record must
        // not disturb the last-turn decision.
        let content = format!(
            "{}\n{}\n{}\n{}\n",
            ev("turn_started"),
            ev("token_count"),
            r#"{"type":"world_state","payload":{}}"#,
            ev("some_future_event"),
        );
        assert_eq!(derive(&content).attention, Attention::Working);
    }

    #[test]
    fn derive_no_turn_event_is_unknown() {
        // A file with only meta + a user message (no turn started yet).
        let content = format!(
            "{SESSION_META}\n{}\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","payload":{"message":"hi"}}}"#,
        );
        assert_eq!(derive(&content).attention, Attention::Unknown);
    }

    #[test]
    fn derive_skips_partial_first_line() {
        // A tail window that starts mid-JSON-line: the first (partial) line
        // fails to parse and is dropped, and the real events after it still
        // classify — same behaviour as the Claude parser.
        let content = format!(
            "sg_updated\":true}}}}\n{}\n{}\n",
            ev("turn_started"),
            ev("turn_complete"),
        );
        assert_eq!(derive(&content).attention, Attention::NeedsInput);
    }

    // ---- derive: edited files ----

    fn patch_apply_end(paths: &[&str]) -> String {
        let changes: Vec<String> = paths
            .iter()
            .map(|p| format!(r#""{p}":{{"update":{{"unified_diff":"@@"}}}}"#))
            .collect();
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"patch_apply_end","changes":{{{}}},"success":true}}}}"#,
            changes.join(","),
        )
    }

    fn file_change_item(paths: &[&str]) -> String {
        let changes: Vec<String> = paths
            .iter()
            .map(|p| format!(r#""{p}":{{"add":{{"content":"x"}}}}"#))
            .collect();
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"file_change","changes":{{{}}},"status":"completed"}}}}}}"#,
            changes.join(","),
        )
    }

    #[test]
    fn edited_files_from_legacy_patch_apply_end() {
        let content = format!(
            "{}\n{}\n",
            patch_apply_end(&["/work/proj/a.rs"]),
            patch_apply_end(&["/work/proj/b.rs"]),
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
    fn edited_files_from_paginated_file_change_item() {
        let content = format!(
            "{}\n{}\n",
            file_change_item(&["/work/proj/a.rs"]),
            file_change_item(&["/work/proj/b.rs"]),
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
    fn edited_files_resolves_relative_paths_against_cwd() {
        // Codex may record a workspace-relative path; it's resolved against
        // the session cwd so the picker gets an absolute path.
        let content = format!("{}\n", patch_apply_end(&["src/lib.rs"]));
        assert_eq!(
            derive(&content).edited_files,
            vec![PathBuf::from("/work/proj/src/lib.rs")]
        );
    }

    #[test]
    fn edited_files_dedup_moves_reedited_to_front() {
        let content = format!(
            "{}\n{}\n{}\n",
            patch_apply_end(&["/w/a.rs"]),
            patch_apply_end(&["/w/b.rs"]),
            patch_apply_end(&["/w/a.rs"]),
        );
        assert_eq!(
            codex().derive(&content, Path::new("/w")).edited_files,
            vec![PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")]
        );
    }

    #[test]
    fn edited_files_empty_when_no_patches() {
        let content = format!("{}\n{}\n", ev("turn_started"), ev("turn_complete"));
        assert!(derive(&content).edited_files.is_empty());
    }

    #[test]
    fn edited_files_caps_at_the_limit() {
        let lines: Vec<String> = (0..(EDITED_FILES_CAP + 20))
            .map(|i| patch_apply_end(&[&format!("/w/f{i}.rs")]))
            .collect();
        let files = codex()
            .derive(&lines.join("\n"), Path::new("/w"))
            .edited_files;
        assert_eq!(files.len(), EDITED_FILES_CAP);
        // Most-recent-first: the last-written file leads.
        assert_eq!(
            files[0],
            PathBuf::from(format!("/w/f{}.rs", EDITED_FILES_CAP + 19))
        );
    }

    #[test]
    fn derive_carries_attention_and_edits_from_one_walk() {
        let content = format!(
            "{SESSION_META}\n{}\n{}\n{}\n",
            ev("turn_started"),
            patch_apply_end(&["/work/proj/x.rs"]),
            ev("turn_complete"),
        );
        let d = derive(&content);
        assert_eq!(d.attention, Attention::NeedsInput);
        assert_eq!(d.edited_files, vec![PathBuf::from("/work/proj/x.rs")]);
    }
}
