//! `agent-mux install-hooks` — writes the Claude Code Notification
//! hook entry into the user's `~/.claude/settings.json` so the hook
//! ingress pipeline gets wired up without a manual JSON edit.
//!
//! Two halves: a pure [`plan_install`] that takes the current settings
//! content as a string and returns the merged content + the action
//! taken (added / updated stale / no-op), and a CLI-facing
//! [`install_hooks_at`] that wraps the pure side with the I/O
//! (read, optional backup, atomic write, post-write verification).
//!
//! ## Why the pure-function split
//!
//! The JSON-merge logic has a handful of edge cases — empty file,
//! file with unrelated settings, file with other Notification hooks,
//! file with a stale agent-mux entry — and each needs its own test.
//! Driving them through real `~/.claude/settings.json` would be slow
//! and risk clobbering the developer's actual config. The pure
//! function takes a `&str`, returns a `String`, and the I/O wrapper
//! gets one test for the round-trip.
//!
//! ## Identifying "our" entry
//!
//! An existing Notification hook is recognised as agent-mux's iff its
//! `command` is a whitespace-separated string whose first token has
//! the basename `agent-mux` and whose second token is `hook`. That
//! catches every shape we ever write (`/abs/path/agent-mux hook`,
//! `agent-mux hook`, etc.) without false-matching unrelated commands.
//! Exact-string equality with the desired command means "already
//! installed at this path, no-op"; basename-match without exact equal
//! means "stale entry, update in place."

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// What [`plan_install`] decided to do for the given settings file.
/// Mirrored to the user via [`describe_action`] so they see what
/// changed (or didn't) on stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallAction {
    /// Already installed pointing at the same binary path. The
    /// returned content is unchanged from the input.
    NoOp,
    /// No agent-mux entry existed; a fresh one was appended.
    Added,
    /// A stale agent-mux entry was found (different binary path) and
    /// updated in place. Carries the previous command string for
    /// the user-facing diff line.
    Updated { previous_command: String },
}

/// Outcome of one [`plan_install`] call: the merged settings content
/// (`new_content`) and the action [`InstallAction`] the user should
/// be told about.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub new_content: String,
    pub action: InstallAction,
}

/// JSON-merge errors. Distinguished from I/O errors so the wrapper
/// can fail cleanly without conflating "settings file unreadable"
/// with "settings file isn't a JSON object."
#[derive(Debug)]
pub enum InstallError {
    /// File parsed as JSON but the root isn't an object (`null`, an
    /// array, a number, etc.). Claude Code requires an object root;
    /// we don't try to coerce.
    NotJsonObject,
    /// File content didn't parse as JSON. Carries the parser's
    /// message so the user can pinpoint the malformed line.
    Parse(serde_json::Error),
    /// Existing `hooks` key isn't an object, or `hooks.Notification`
    /// isn't an array — Claude Code's schema requires these shapes
    /// and rewriting them would silently break the user's other
    /// configured hooks. Refuse loudly instead.
    SchemaMismatch(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJsonObject => {
                write!(f, "settings.json root is not a JSON object")
            }
            Self::Parse(e) => write!(f, "settings.json parse error: {e}"),
            Self::SchemaMismatch(msg) => write!(f, "settings.json schema mismatch: {msg}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// Compute the merged settings.json content. Pure — takes the
/// existing content as a string (empty string = "no file yet"), the
/// path to the agent-mux binary, and returns the merged content + a
/// description of what changed.
///
/// Behaviour:
/// - Empty / missing input → start from `{}` and add a fresh hook.
/// - Existing JSON object with no `hooks` key → add `hooks.Notification` array.
/// - Existing `hooks.Notification` array without our entry → append.
/// - Existing entry with our exact `command` string → no-op.
/// - Existing entry whose command basename is `agent-mux` and
///   subcommand is `hook` but path differs → update in place.
///
/// # Errors
///
/// - [`InstallError::Parse`] if the input isn't valid JSON.
/// - [`InstallError::NotJsonObject`] if the root isn't a JSON object.
/// - [`InstallError::SchemaMismatch`] if `hooks` or `hooks.Notification`
///   has an unexpected shape (e.g. `hooks` is an array, or
///   `hooks.Notification` is a string).
pub fn plan_install(current: &str, binary_path: &Path) -> Result<InstallPlan, InstallError> {
    let desired_command = format!("{} hook", binary_path.display());
    let mut root: Value = if current.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(current).map_err(InstallError::Parse)?
    };
    let root_obj = root.as_object_mut().ok_or(InstallError::NotJsonObject)?;

    let hooks_obj = ensure_object(root_obj, "hooks")?;
    let notification_arr = ensure_array(hooks_obj, "Notification")?;

    // Claude wires the hook under the single `Notification` event with a
    // `.*` matcher; codex ([`plan_install_codex`]) reuses the same merge
    // helper across two matcher-less event arrays.
    let action = merge_command_into_array(notification_arr, &desired_command, Some(".*"));

    let new_content = serde_json::to_string_pretty(&root).map_err(InstallError::Parse)? + "\n";
    Ok(InstallPlan {
        new_content,
        action,
    })
}

/// Codex hooks-file variant of [`plan_install`]. Writes `type:"command"`
/// handlers for the two lifecycle events agent-mux cares about —
/// `PermissionRequest` (needs-approval → blocking marker) and `Stop`
/// (turn complete → non-blocking marker) — each invoking
/// `<binary> hook --agent codex`.
///
/// ## hooks.json schema assumption (BEST-EFFORT)
///
/// Codex configures lifecycle hooks in `~/.codex/hooks.json`
/// (Appendix A §5 of `docs/plans/2026-07-09-multi-agent-cli.md`,
/// researched 2026-07-09 — no `codex` on the build box to verify against).
/// The exact top-level key layout is **not independently confirmed**; we
/// mirror Claude Code's proven shape (Codex's hooks mechanism was modelled
/// on it): a top-level `"hooks"` object keyed by event name, each value an
/// array of `{ "hooks": [ { "type": "command", "command": … } ] }`
/// entries. If upstream turns out to use a different layout, the fix is
/// localised to this one function plus [`merge_command_into_array`]'s
/// appended-entry shape — everything else (recognition, idempotency,
/// merge, dry-run, the I/O wrapper) is schema-agnostic.
///
/// # Errors
///
/// Same as [`plan_install`]: [`InstallError::Parse`] on malformed JSON,
/// [`InstallError::NotJsonObject`] on a non-object root,
/// [`InstallError::SchemaMismatch`] if `hooks` or an event value has an
/// unexpected shape.
pub fn plan_install_codex(current: &str, binary_path: &Path) -> Result<InstallPlan, InstallError> {
    let desired_command = format!("{} hook --agent codex", binary_path.display());
    let mut root: Value = if current.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(current).map_err(InstallError::Parse)?
    };
    let root_obj = root.as_object_mut().ok_or(InstallError::NotJsonObject)?;
    let hooks_obj = ensure_object(root_obj, "hooks")?;

    // Aggregate across both event arrays: any stale update wins (carries
    // its previous command for the notice), else any fresh append, else a
    // clean no-op when both handlers were already present.
    let mut action = InstallAction::NoOp;
    for event in CODEX_HOOK_EVENTS {
        let arr = ensure_array(hooks_obj, event)?;
        let this = merge_command_into_array(arr, &desired_command, None);
        action = combine_actions(action, this);
    }

    let new_content = serde_json::to_string_pretty(&root).map_err(InstallError::Parse)? + "\n";
    Ok(InstallPlan {
        new_content,
        action,
    })
}

/// The codex lifecycle events agent-mux installs command handlers for.
/// `PermissionRequest` is the needs-approval signal (→ blocking marker);
/// `Stop` is turn-complete (→ non-blocking marker).
const CODEX_HOOK_EVENTS: &[&str] = &["PermissionRequest", "Stop"];

/// Merge `desired_command` into one hook-event array (claude's
/// `Notification` array, or one of codex's event arrays), preserving every
/// unrelated entry. Returns the action taken for *this* array:
/// - an existing agent-mux entry with the exact command → [`InstallAction::NoOp`];
/// - an existing agent-mux entry with a different (stale) path → updated
///   in place, [`InstallAction::Updated`];
/// - no agent-mux entry → a fresh one appended ([`InstallAction::Added`]),
///   with a `matcher` field only when `matcher` is `Some`.
fn merge_command_into_array(
    arr: &mut Vec<Value>,
    desired_command: &str,
    matcher: Option<&str>,
) -> InstallAction {
    for matcher_entry in arr.iter_mut() {
        let Some(entry_obj) = matcher_entry.as_object_mut() else {
            continue;
        };
        let Some(inner_hooks) = entry_obj.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        for inner in inner_hooks.iter_mut() {
            let Some(inner_obj) = inner.as_object_mut() else {
                continue;
            };
            let Some(command_str) = inner_obj.get("command").and_then(Value::as_str) else {
                continue;
            };
            if !is_agent_mux_hook_command(command_str) {
                continue;
            }
            if command_str == desired_command {
                return InstallAction::NoOp;
            }
            let previous_command = command_str.to_string();
            inner_obj.insert("command".into(), Value::String(desired_command.to_string()));
            return InstallAction::Updated { previous_command };
        }
    }

    let entry = match matcher {
        Some(m) => json!({
            "matcher": m,
            "hooks": [{"type": "command", "command": desired_command}]
        }),
        None => json!({
            "hooks": [{"type": "command", "command": desired_command}]
        }),
    };
    arr.push(entry);
    InstallAction::Added
}

/// Combine two per-array [`InstallAction`]s into the overall action for a
/// multi-array install (codex). Precedence: a stale update > a fresh
/// append > no-op — the first `Updated` keeps its `previous_command`.
fn combine_actions(acc: InstallAction, next: InstallAction) -> InstallAction {
    match (acc, next) {
        (prev @ InstallAction::Updated { .. }, _) => prev,
        (_, next @ InstallAction::Updated { .. }) => next,
        (InstallAction::Added, _) | (_, InstallAction::Added) => InstallAction::Added,
        (InstallAction::NoOp, InstallAction::NoOp) => InstallAction::NoOp,
    }
}

/// Hook-command element-shape predicate. The command field is a
/// free-form shell string; we identify our own entries by splitting on
/// whitespace and checking that the first token's basename is
/// `agent-mux` and the second is `hook`. That recognises every shape we
/// write — claude's `<abs>/agent-mux hook` *and* codex's
/// `<abs>/agent-mux hook --agent codex` (the trailing `--agent codex`
/// doesn't change the first two tokens) — while staying conservative
/// enough not to false-match a different command that merely mentions
/// agent-mux in flags.
#[must_use]
fn is_agent_mux_hook_command(command: &str) -> bool {
    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else {
        return false;
    };
    let Some(subcommand) = parts.next() else {
        return false;
    };
    if subcommand != "hook" {
        return false;
    }
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "agent-mux")
}

/// Ensure `obj[key]` is an object, creating an empty one if absent.
/// Returns a mutable reference to the (existing or fresh) object.
fn ensure_object<'a>(
    obj: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, InstallError> {
    if !obj.contains_key(key) {
        obj.insert(key.into(), Value::Object(Map::new()));
    }
    obj.get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| InstallError::SchemaMismatch(format!("`{key}` is not a JSON object")))
}

/// Ensure `obj[key]` is an array, creating an empty one if absent.
/// Returns a mutable reference to the (existing or fresh) array.
fn ensure_array<'a>(
    obj: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Vec<Value>, InstallError> {
    if !obj.contains_key(key) {
        obj.insert(key.into(), Value::Array(Vec::new()));
    }
    obj.get_mut(key)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| InstallError::SchemaMismatch(format!("`{key}` is not a JSON array")))
}

/// Default settings.json path under the user's home. Used by the CLI
/// subcommand; the test path goes through [`install_hooks_at`] with
/// an injected path.
#[must_use]
pub fn default_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Default codex hooks-file path (`~/.codex/hooks.json`) — the global
/// user-scope file, mirroring the claude installer's user-scope choice
/// (Appendix A §5). `$CODEX_HOME` relocation and the project-level
/// `<repo>/.codex/hooks.json` are out of scope for the installer (as the
/// project-scope claude install is).
#[must_use]
pub fn default_codex_hooks_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("hooks.json"))
}

/// CLI-facing wrapper: read the settings file, plan the install,
/// optionally back up and write, then verify the post-write content
/// still parses. `out` receives the user-facing summary lines.
///
/// `dry_run` short-circuits the write — the planned content prints to
/// `out` and nothing on disk changes.
///
/// # Errors
///
/// I/O errors from read/write, [`InstallError`] flavours from
/// [`plan_install`] (surfaced as `io::Error::other` so the caller's
/// `io::Result` covers everything).
pub fn install_hooks_at<W: Write>(
    settings_path: &Path,
    binary_path: &Path,
    dry_run: bool,
    out: &mut W,
) -> io::Result<()> {
    let current = read_current(settings_path)?;
    let plan = plan_install(&current, binary_path).map_err(io::Error::other)?;
    apply_install_plan(
        settings_path,
        binary_path,
        &current,
        &plan,
        "Notification hook",
        dry_run,
        out,
    )
}

/// Codex variant of [`install_hooks_at`]: merge the two lifecycle-hook
/// command handlers into `~/.codex/hooks.json`. Shares the whole I/O
/// wrapper (backup, atomic write, verify, dry-run) with the claude path —
/// only the plan function and the user-facing hook label differ.
///
/// # Errors
///
/// Same as [`install_hooks_at`].
pub fn install_codex_hooks_at<W: Write>(
    hooks_path: &Path,
    binary_path: &Path,
    dry_run: bool,
    out: &mut W,
) -> io::Result<()> {
    let current = read_current(hooks_path)?;
    let plan = plan_install_codex(&current, binary_path).map_err(io::Error::other)?;
    apply_install_plan(
        hooks_path,
        binary_path,
        &current,
        &plan,
        "lifecycle hooks",
        dry_run,
        out,
    )
}

/// Read the current hooks/settings file, treating a missing file as empty
/// (the fresh-install case) and propagating any other I/O error.
fn read_current(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Shared I/O half of the installer, parameterised over the plan and the
/// user-facing hook label (`what`). Prints the summary, short-circuits a
/// no-op, honours `dry_run`, and otherwise backs up + atomically writes +
/// verifies. Used by both [`install_hooks_at`] (claude) and
/// [`install_codex_hooks_at`].
fn apply_install_plan<W: Write>(
    settings_path: &Path,
    binary_path: &Path,
    current: &str,
    plan: &InstallPlan,
    what: &str,
    dry_run: bool,
    out: &mut W,
) -> io::Result<()> {
    writeln!(out, "Settings file: {}", settings_path.display())?;
    writeln!(out, "agent-mux binary: {}", binary_path.display())?;
    writeln!(out, "Action: {}", describe_action(&plan.action, what))?;
    if matches!(plan.action, InstallAction::NoOp) {
        return Ok(());
    }
    if dry_run {
        writeln!(
            out,
            "\n--- dry run: planned {} ---",
            file_label(settings_path)
        )?;
        out.write_all(plan.new_content.as_bytes())?;
        return Ok(());
    }
    // Make sure the parent dir exists. Fresh-install case: ~/.claude/
    // may not be there yet if the user hasn't run claude on this box.
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // One-time backup: if the file existed and no .bak is around yet,
    // copy it before we overwrite. Skip on the dry-run / no-op paths
    // so we don't write a backup that's identical to the live file.
    if !current.is_empty() {
        let backup_path = backup_path_for(settings_path);
        if !backup_path.exists() {
            fs::write(&backup_path, current)?;
            writeln!(out, "Backup written: {}", backup_path.display())?;
        }
    }
    // Atomic replacement via tmp + rename so a crash mid-write can't
    // leave the user with a half-truncated settings file.
    let tmp_path = settings_path.with_extension("json.tmp");
    fs::write(&tmp_path, &plan.new_content)?;
    fs::rename(&tmp_path, settings_path)?;
    // Verify the round-trip: re-read and parse so a write that
    // succeeded but produced unparseable JSON surfaces immediately
    // rather than failing the next time Claude Code starts.
    let written = fs::read_to_string(settings_path)?;
    serde_json::from_str::<Value>(&written)
        .map_err(|e| io::Error::other(format!("post-write verification failed: {e}")))?;
    writeln!(
        out,
        "\nSettings updated. Restart agent-mux if it's already running."
    )?;
    Ok(())
}

/// The file's basename for user-facing messages (`settings.json` /
/// `hooks.json`), falling back to the full path when it has no filename.
fn file_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Where to put the one-time backup of the existing settings.json.
/// Sibling to the file with a `.bak` suffix added; if the user
/// already has a `.bak` from a previous install, we leave it alone
/// (the first backup is the most valuable one).
#[must_use]
fn backup_path_for(settings_path: &Path) -> PathBuf {
    let mut backup = settings_path.as_os_str().to_owned();
    backup.push(".bak");
    PathBuf::from(backup)
}

/// One-line user-facing summary of the action a plan function chose.
/// `what` names the hook kind (`"Notification hook"` for claude,
/// `"lifecycle hooks"` for codex). Routed through this helper (rather than
/// inlined in the installer) so the tests can spot-check the text without
/// going through the full CLI wrapper.
#[must_use]
pub fn describe_action(action: &InstallAction, what: &str) -> String {
    match action {
        InstallAction::NoOp => {
            format!("no change \u{2014} agent-mux {what} already configured at this path")
        }
        InstallAction::Added => format!("added agent-mux {what} entry"),
        InstallAction::Updated { previous_command } => {
            format!("updated stale agent-mux {what} entry (was: {previous_command})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn binary() -> PathBuf {
        PathBuf::from("/usr/local/bin/agent-mux")
    }

    #[test]
    fn plan_install_creates_hooks_block_when_settings_is_empty() {
        let plan = plan_install("", &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::Added);
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        let cmd = value["hooks"]["Notification"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "/usr/local/bin/agent-mux hook");
    }

    #[test]
    fn plan_install_preserves_unrelated_top_level_keys() {
        let input = r#"{
  "theme": "dark",
  "permissions": {"allow_all": false}
}"#;
        let plan = plan_install(input, &binary()).unwrap();
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["permissions"]["allow_all"], false);
        assert_eq!(plan.action, InstallAction::Added);
    }

    #[test]
    fn plan_install_appends_when_other_notification_hooks_exist() {
        let input = r#"{
  "hooks": {
    "Notification": [
      {"matcher": "permission_prompt", "hooks": [{"type": "command", "command": "/other/tool"}]}
    ]
  }
}"#;
        let plan = plan_install(input, &binary()).unwrap();
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        let arr = value["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "other entry must be preserved alongside ours");
        // Original entry untouched
        assert_eq!(arr[0]["hooks"][0]["command"], "/other/tool");
        // Ours appended
        assert_eq!(
            arr[1]["hooks"][0]["command"],
            "/usr/local/bin/agent-mux hook"
        );
    }

    #[test]
    fn plan_install_is_noop_when_our_hook_already_present_with_same_path() {
        let input = r#"{
  "hooks": {
    "Notification": [
      {"matcher": ".*", "hooks": [{"type": "command", "command": "/usr/local/bin/agent-mux hook"}]}
    ]
  }
}"#;
        let plan = plan_install(input, &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::NoOp);
    }

    #[test]
    fn plan_install_updates_stale_agent_mux_entry_in_place() {
        // User moved/reinstalled the binary; old path needs replacing.
        let input = r#"{
  "hooks": {
    "Notification": [
      {"matcher": ".*", "hooks": [{"type": "command", "command": "/old/path/agent-mux hook"}]}
    ]
  }
}"#;
        let plan = plan_install(input, &binary()).unwrap();
        match &plan.action {
            InstallAction::Updated { previous_command } => {
                assert_eq!(previous_command, "/old/path/agent-mux hook");
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        let arr = value["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "stale entry replaced in place, not appended");
        assert_eq!(
            arr[0]["hooks"][0]["command"],
            "/usr/local/bin/agent-mux hook"
        );
    }

    #[test]
    fn plan_install_does_not_touch_non_agent_mux_commands_that_mention_agent_mux() {
        // Edge case: a flag-rich command that says "agent-mux" in
        // its argv but isn't `<...>/agent-mux hook`. Must not be
        // mistaken for our entry.
        let input = r#"{
  "hooks": {
    "Notification": [
      {"matcher": ".*", "hooks": [{"type": "command", "command": "/bin/echo agent-mux"}]}
    ]
  }
}"#;
        let plan = plan_install(input, &binary()).unwrap();
        // We should append our entry, not replace the echo entry.
        assert_eq!(plan.action, InstallAction::Added);
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        let arr = value["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["hooks"][0]["command"], "/bin/echo agent-mux");
    }

    #[test]
    fn plan_install_rejects_non_object_root() {
        let err = plan_install("[]", &binary()).unwrap_err();
        assert!(matches!(err, InstallError::NotJsonObject), "got {err:?}");
    }

    #[test]
    fn plan_install_rejects_hooks_with_wrong_shape() {
        let input = r#"{"hooks": "not an object"}"#;
        let err = plan_install(input, &binary()).unwrap_err();
        assert!(
            matches!(err, InstallError::SchemaMismatch(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn plan_install_rejects_malformed_json() {
        let err = plan_install("{not json", &binary()).unwrap_err();
        assert!(matches!(err, InstallError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn is_agent_mux_hook_command_matches_path_and_subcommand() {
        assert!(is_agent_mux_hook_command("/abs/agent-mux hook"));
        assert!(is_agent_mux_hook_command("agent-mux hook"));
        assert!(is_agent_mux_hook_command("/Users/x/bin/agent-mux hook"));
    }

    #[test]
    fn is_agent_mux_hook_command_rejects_non_agent_mux_programs() {
        assert!(!is_agent_mux_hook_command("/bin/echo hook"));
        assert!(!is_agent_mux_hook_command("not-agent-mux hook"));
    }

    #[test]
    fn is_agent_mux_hook_command_rejects_wrong_subcommand() {
        assert!(!is_agent_mux_hook_command("/abs/agent-mux config"));
        assert!(!is_agent_mux_hook_command("agent-mux help"));
    }

    #[test]
    fn install_hooks_at_creates_settings_file_and_backup_when_missing() {
        let tmp = TempDir::new().unwrap();
        let settings = tmp.path().join(".claude").join("settings.json");
        let mut out = Vec::new();
        install_hooks_at(&settings, &binary(), false, &mut out).unwrap();
        assert!(settings.exists(), "settings.json must be created");
        // No backup written because there was no prior file.
        assert!(!backup_path_for(&settings).exists());
        let content = fs::read_to_string(&settings).unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            value["hooks"]["Notification"][0]["hooks"][0]["command"],
            "/usr/local/bin/agent-mux hook"
        );
    }

    #[test]
    fn install_hooks_at_writes_backup_only_once() {
        let tmp = TempDir::new().unwrap();
        let settings = tmp.path().join("settings.json");
        fs::write(&settings, r#"{"theme": "dark"}"#).unwrap();
        let mut out = Vec::new();
        install_hooks_at(&settings, &binary(), false, &mut out).unwrap();
        let backup = backup_path_for(&settings);
        assert!(backup.exists());
        let original_backup = fs::read_to_string(&backup).unwrap();
        // Second invocation against the now-mutated file: no new backup.
        let other_binary = PathBuf::from("/different/path/agent-mux");
        install_hooks_at(&settings, &other_binary, false, &mut Vec::new()).unwrap();
        // Backup content unchanged (still the pristine original).
        assert_eq!(fs::read_to_string(&backup).unwrap(), original_backup);
    }

    #[test]
    fn install_hooks_at_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let settings = tmp.path().join("settings.json");
        fs::write(&settings, r#"{"theme": "dark"}"#).unwrap();
        let original = fs::read_to_string(&settings).unwrap();
        let mut out = Vec::new();
        install_hooks_at(&settings, &binary(), true, &mut out).unwrap();
        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            original,
            "dry_run must not modify the file"
        );
        let out_str = String::from_utf8(out).unwrap();
        assert!(
            out_str.contains("dry run"),
            "dry-run output should announce itself"
        );
        assert!(out_str.contains("agent-mux hook"));
    }

    #[test]
    fn install_hooks_at_noop_path_prints_status_and_skips_backup() {
        let tmp = TempDir::new().unwrap();
        let settings = tmp.path().join("settings.json");
        let prefilled = r#"{
  "hooks": {
    "Notification": [
      {"matcher": ".*", "hooks": [{"type": "command", "command": "/usr/local/bin/agent-mux hook"}]}
    ]
  }
}"#;
        fs::write(&settings, prefilled).unwrap();
        let mut out = Vec::new();
        install_hooks_at(&settings, &binary(), false, &mut out).unwrap();
        let out_str = String::from_utf8(out).unwrap();
        assert!(out_str.contains("already configured"));
        assert!(
            !backup_path_for(&settings).exists(),
            "no-op must not write a backup"
        );
    }

    #[test]
    fn describe_action_includes_previous_command_for_updated_variant() {
        let s = describe_action(
            &InstallAction::Updated {
                previous_command: "/old/agent-mux hook".to_string(),
            },
            "Notification hook",
        );
        assert!(s.contains("/old/agent-mux hook"), "got: {s}");
    }

    // ---- codex installer (WP8) ----

    #[test]
    fn plan_install_codex_creates_both_event_handlers_when_empty() {
        let plan = plan_install_codex("", &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::Added);
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        for event in ["PermissionRequest", "Stop"] {
            let cmd = value["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("missing command for {event}"));
            assert_eq!(cmd, "/usr/local/bin/agent-mux hook --agent codex");
        }
    }

    #[test]
    fn plan_install_codex_is_noop_when_both_handlers_present() {
        let input = r#"{
  "hooks": {
    "PermissionRequest": [{"hooks": [{"type": "command", "command": "/usr/local/bin/agent-mux hook --agent codex"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "/usr/local/bin/agent-mux hook --agent codex"}]}]
  }
}"#;
        let plan = plan_install_codex(input, &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::NoOp);
    }

    #[test]
    fn plan_install_codex_updates_stale_path_in_place() {
        let input = r#"{
  "hooks": {
    "PermissionRequest": [{"hooks": [{"type": "command", "command": "/old/agent-mux hook --agent codex"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "/old/agent-mux hook --agent codex"}]}]
  }
}"#;
        let plan = plan_install_codex(input, &binary()).unwrap();
        match &plan.action {
            InstallAction::Updated { previous_command } => {
                assert_eq!(previous_command, "/old/agent-mux hook --agent codex");
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        for event in ["PermissionRequest", "Stop"] {
            assert_eq!(
                value["hooks"][event][0]["hooks"][0]["command"],
                "/usr/local/bin/agent-mux hook --agent codex",
                "{event} stale entry replaced in place"
            );
        }
    }

    #[test]
    fn plan_install_codex_preserves_unrelated_hook_events() {
        // A user's own SessionStart handler must survive untouched
        // alongside the two we add.
        let input = r#"{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "/other/tool"}]}]
  }
}"#;
        let plan = plan_install_codex(input, &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::Added);
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        assert_eq!(
            value["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "/other/tool"
        );
        assert!(value["hooks"]["PermissionRequest"].is_array());
        assert!(value["hooks"]["Stop"].is_array());
    }

    #[test]
    fn plan_install_codex_appends_alongside_a_foreign_permission_request_handler() {
        let input = r#"{
  "hooks": {
    "PermissionRequest": [{"hooks": [{"type": "command", "command": "/other/tool"}]}]
  }
}"#;
        let plan = plan_install_codex(input, &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::Added);
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        let arr = value["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "foreign handler preserved, ours appended");
        assert_eq!(arr[0]["hooks"][0]["command"], "/other/tool");
        assert_eq!(
            arr[1]["hooks"][0]["command"],
            "/usr/local/bin/agent-mux hook --agent codex"
        );
    }

    #[test]
    fn is_agent_mux_hook_command_matches_codex_variant() {
        assert!(is_agent_mux_hook_command(
            "/abs/agent-mux hook --agent codex"
        ));
        assert!(is_agent_mux_hook_command("agent-mux hook --agent codex"));
    }

    #[test]
    fn install_codex_hooks_at_creates_file_and_handlers_when_missing() {
        let tmp = TempDir::new().unwrap();
        let hooks = tmp.path().join(".codex").join("hooks.json");
        let mut out = Vec::new();
        install_codex_hooks_at(&hooks, &binary(), false, &mut out).unwrap();
        assert!(hooks.exists(), "hooks.json must be created");
        let value: Value = serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
        assert_eq!(
            value["hooks"]["PermissionRequest"][0]["hooks"][0]["command"],
            "/usr/local/bin/agent-mux hook --agent codex"
        );
        assert_eq!(
            value["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/usr/local/bin/agent-mux hook --agent codex"
        );
    }

    #[test]
    fn install_codex_hooks_at_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let hooks = tmp.path().join("hooks.json");
        install_codex_hooks_at(&hooks, &binary(), false, &mut Vec::new()).unwrap();
        let after_first = fs::read_to_string(&hooks).unwrap();
        let mut out = Vec::new();
        install_codex_hooks_at(&hooks, &binary(), false, &mut out).unwrap();
        assert_eq!(
            fs::read_to_string(&hooks).unwrap(),
            after_first,
            "second run is a no-op"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("already configured")
        );
    }

    #[test]
    fn install_codex_hooks_at_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let hooks = tmp.path().join("hooks.json");
        fs::write(&hooks, "{}").unwrap();
        let original = fs::read_to_string(&hooks).unwrap();
        let mut out = Vec::new();
        install_codex_hooks_at(&hooks, &binary(), true, &mut out).unwrap();
        assert_eq!(
            fs::read_to_string(&hooks).unwrap(),
            original,
            "dry_run must not modify the file"
        );
        let out_str = String::from_utf8(out).unwrap();
        assert!(
            out_str.contains("dry run"),
            "dry-run announces itself: {out_str}"
        );
        assert!(out_str.contains("hook --agent codex"));
    }
}
