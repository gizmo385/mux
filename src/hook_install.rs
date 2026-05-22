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

    // Search the existing entries for ours.
    let mut action = InstallAction::Added;
    let mut handled = false;
    for matcher_entry in notification_arr.iter_mut() {
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
                action = InstallAction::NoOp;
            } else {
                action = InstallAction::Updated {
                    previous_command: command_str.to_string(),
                };
                inner_obj.insert("command".into(), Value::String(desired_command.clone()));
            }
            handled = true;
            break;
        }
        if handled {
            break;
        }
    }

    if !handled {
        notification_arr.push(json!({
            "matcher": ".*",
            "hooks": [
                {"type": "command", "command": desired_command}
            ]
        }));
    }

    let new_content = serde_json::to_string_pretty(&root).map_err(InstallError::Parse)? + "\n";
    Ok(InstallPlan {
        new_content,
        action,
    })
}

/// `hooks.Notification` element-shape predicate. The command field is
/// a free-form shell string; we identify our own entries by splitting
/// on whitespace and checking that the first token's basename is
/// `agent-mux` and the second is `hook`. That's deliberately
/// conservative — it won't false-match a different hook command that
/// happens to mention agent-mux in flags.
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
    let current = match fs::read_to_string(settings_path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let plan = plan_install(&current, binary_path).map_err(io::Error::other)?;
    writeln!(out, "Settings file: {}", settings_path.display())?;
    writeln!(out, "agent-mux binary: {}", binary_path.display())?;
    writeln!(out, "Action: {}", describe_action(&plan.action))?;
    if matches!(plan.action, InstallAction::NoOp) {
        return Ok(());
    }
    if dry_run {
        writeln!(out, "\n--- dry run: planned settings.json ---")?;
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
            fs::write(&backup_path, &current)?;
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

/// One-line user-facing summary of the action [`plan_install`]
/// chose. Routed through this helper (rather than inlined in
/// [`install_hooks_at`]) so the tests can spot-check the text
/// without going through the full CLI wrapper.
#[must_use]
pub fn describe_action(action: &InstallAction) -> String {
    match action {
        InstallAction::NoOp => {
            "no change \u{2014} agent-mux Notification hook already configured at this path"
                .to_string()
        }
        InstallAction::Added => "added agent-mux Notification hook entry".to_string(),
        InstallAction::Updated { previous_command } => {
            format!("updated stale agent-mux Notification hook entry (was: {previous_command})")
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
        let s = describe_action(&InstallAction::Updated {
            previous_command: "/old/agent-mux hook".to_string(),
        });
        assert!(s.contains("/old/agent-mux hook"), "got: {s}");
    }
}
