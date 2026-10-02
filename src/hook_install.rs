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

use crate::host::Host;

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
    /// Nothing to add or update, but a legacy agent-mux handler the
    /// current installer no longer writes (codex's `Stop`, superseded by
    /// the rollout's `task_complete`) was removed.
    RemovedLegacy,
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

/// Codex hooks-file variant of [`plan_install`]. Writes a `type:"command"`
/// handler for the one lifecycle event the rollout can't show —
/// `PermissionRequest` (needs-approval → blocking marker) — invoking
/// `<binary> hook --agent codex`, and removes the legacy agent-mux `Stop`
/// handler earlier installers wrote: turn completion is already in the
/// rollout (`task_complete`, with `last_agent_message`), so a `Stop` hook
/// only added a second handler for the user to trust in Codex.
///
/// ## hooks.json schema (validated 2026-10-02 against codex 0.142.5)
///
/// A top-level `"hooks"` object keyed by event name, each value an array of
/// `{ "matcher"?, "hooks": [ { "type": "command", "command": … } ] }`
/// groups — the same shape as Claude Code's settings. Codex parses the file
/// silently when it matches and warns (`failed to parse hooks config`) when
/// it doesn't.
///
/// ## Trust
///
/// Codex runs a non-managed hook only after the user trusts it in Codex's
/// own hooks review (the interactive TUI prompts "Hooks need review" at
/// startup). Trust is keyed by the handler's *position* and hashed over its
/// command, so any install that adds or changes the handler needs a fresh
/// review — see [`codex_hook_trust`]. agent-mux never writes trust state:
/// that is the user's security decision inside Codex.
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

    // Aggregate across the event arrays: any stale update wins (carries
    // its previous command for the notice), else any fresh append, else a
    // clean no-op when every handler was already present.
    let mut action = InstallAction::NoOp;
    for event in CODEX_HOOK_EVENTS {
        let arr = ensure_array(hooks_obj, event)?;
        let this = merge_command_into_array(arr, &desired_command, None);
        action = combine_actions(action, this);
    }
    for event in CODEX_LEGACY_HOOK_EVENTS {
        if remove_agent_mux_handlers(hooks_obj, event)? {
            action = combine_actions(action, InstallAction::RemovedLegacy);
        }
    }

    let new_content = serde_json::to_string_pretty(&root).map_err(InstallError::Parse)? + "\n";
    Ok(InstallPlan {
        new_content,
        action,
    })
}

/// The codex lifecycle events agent-mux installs command handlers for.
/// `PermissionRequest` is the needs-approval signal (→ blocking marker) —
/// the one state codex never persists to its rollout.
const CODEX_HOOK_EVENTS: &[&str] = &["PermissionRequest"];

/// Codex events earlier installers wrote an agent-mux handler for and the
/// current one removes. `Stop` duplicated the rollout's `task_complete`.
/// (`hook --agent codex` still ingests a `Stop` event, so a not-yet-migrated
/// install keeps working.)
const CODEX_LEGACY_HOOK_EVENTS: &[&str] = &["Stop"];

/// Remove every agent-mux command handler from `hooks_obj[event]`,
/// dropping matcher groups left empty and the event key itself when no
/// group remains. Foreign handlers are untouched. Returns whether anything
/// was removed.
fn remove_agent_mux_handlers(
    hooks_obj: &mut Map<String, Value>,
    event: &str,
) -> Result<bool, InstallError> {
    let Some(groups) = hooks_obj.get_mut(event) else {
        return Ok(false);
    };
    let groups = groups
        .as_array_mut()
        .ok_or_else(|| InstallError::SchemaMismatch(format!("`{event}` is not a JSON array")))?;
    let mut removed = false;
    for group in groups.iter_mut() {
        let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = inner.len();
        inner.retain(|h| {
            !h.get("command")
                .and_then(Value::as_str)
                .is_some_and(is_agent_mux_hook_command)
        });
        removed |= inner.len() != before;
    }
    if !removed {
        return Ok(false);
    }
    groups.retain(|g| {
        g.get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|h| !h.is_empty())
    });
    if groups.is_empty() {
        hooks_obj.remove(event);
    }
    Ok(true)
}

/// Whether Codex will actually run the agent-mux `PermissionRequest`
/// handler, as far as agent-mux can tell from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexHookTrust {
    /// No agent-mux `PermissionRequest` handler in the hooks file.
    NotInstalled,
    /// Installed, but Codex has no trust record for it — Codex skips it
    /// until the user reviews hooks in the Codex TUI.
    Untrusted,
    /// The user disabled it in Codex's hooks review.
    Disabled,
    /// A trust record exists. Codex also compares a hash of the command, so
    /// a record made before the handler changed reads as "modified" to
    /// Codex (it re-prompts at startup); agent-mux doesn't replicate that
    /// hash, so this means "trusted at some point", not "currently valid".
    Trusted,
}

/// Classify the agent-mux codex handler's trust from the hooks file
/// (`hooks_json`, at `hooks_path`) and Codex's `config.toml` content.
///
/// Mirrors codex 0.142.5's state lookup: trust lives in the user config as
/// `[hooks.state."<source>:<event>:<group>:<handler>"]` with `enabled` and
/// `trusted_hash`, where `<source>` is the hooks file's path and the
/// indices are the handler's position. Read-only; best-effort — unparseable
/// input degrades to `NotInstalled` / `Untrusted` rather than erroring.
#[must_use]
pub fn codex_hook_trust(hooks_path: &Path, hooks_json: &str, config_toml: &str) -> CodexHookTrust {
    let Some((group, handler)) = find_agent_mux_handler(hooks_json, "PermissionRequest") else {
        return CodexHookTrust::NotInstalled;
    };
    let key = format!(
        "{}:permission_request:{group}:{handler}",
        hooks_path.display()
    );
    let config: toml::Table = config_toml.parse().unwrap_or_default();
    let state = config
        .get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(|s| s.get(&key));
    let Some(state) = state else {
        return CodexHookTrust::Untrusted;
    };
    if state.get("enabled").and_then(toml::Value::as_bool) == Some(false) {
        return CodexHookTrust::Disabled;
    }
    if state
        .get("trusted_hash")
        .and_then(toml::Value::as_str)
        .is_some_and(|h| !h.is_empty())
    {
        CodexHookTrust::Trusted
    } else {
        CodexHookTrust::Untrusted
    }
}

/// Read `hooks_path` and its sibling `config.toml` (both under
/// `$CODEX_HOME`) and classify trust via [`codex_hook_trust`]. A missing
/// file reads as empty.
///
/// # Errors
///
/// I/O errors other than not-found.
pub fn codex_hook_trust_at(hooks_path: &Path) -> io::Result<CodexHookTrust> {
    let hooks_json = read_current(hooks_path)?;
    let config_path = hooks_path.with_file_name("config.toml");
    let config_toml = read_current(&config_path)?;
    Ok(codex_hook_trust(hooks_path, &hooks_json, &config_toml))
}

/// Position `(group_index, handler_index)` of the first agent-mux command
/// handler under `hooks.<event>` in a hooks-file JSON document.
fn find_agent_mux_handler(hooks_json: &str, event: &str) -> Option<(usize, usize)> {
    let root: Value = serde_json::from_str(hooks_json).ok()?;
    let groups = root.get("hooks")?.get(event)?.as_array()?;
    groups.iter().enumerate().find_map(|(g, group)| {
        group
            .get("hooks")?
            .as_array()?
            .iter()
            .position(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_agent_mux_hook_command)
            })
            .map(|h| (g, h))
    })
}

/// User-facing explanation of a [`CodexHookTrust`] state, printed by
/// `install-hooks --agent codex`. `None` when nothing needs saying.
#[must_use]
pub fn describe_codex_trust(trust: &CodexHookTrust) -> Option<&'static str> {
    match trust {
        CodexHookTrust::NotInstalled => None,
        CodexHookTrust::Untrusted => Some(
            "Codex will NOT run this hook until you trust it. Start `codex` once on this \
             machine and choose \"Trust all and continue\" (or \"Review hooks\") at the \
             \"Hooks need review\" prompt. Until then a Codex session waiting on an \
             approval reads as working in agent-mux.",
        ),
        CodexHookTrust::Disabled => Some(
            "The agent-mux hook is disabled in Codex's hooks review; re-enable it there \
             or Codex approvals won't surface in agent-mux.",
        ),
        CodexHookTrust::Trusted => Some(
            "Codex has a trust record for this hook. If you just changed it, Codex will \
             ask you to review it again the next time it starts.",
        ),
    }
}

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
        (InstallAction::RemovedLegacy, _) | (_, InstallAction::RemovedLegacy) => {
            InstallAction::RemovedLegacy
        }
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
    )?;
    // Codex gates every non-managed hook on the user's trust; report where
    // this handler stands so a silent "installed but never runs" can't
    // happen. On a dry run, classify the *planned* content.
    let hooks_json = if dry_run {
        plan.new_content.clone()
    } else {
        read_current(hooks_path)?
    };
    let config_toml = read_current(&hooks_path.with_file_name("config.toml"))?;
    report_codex_trust(out, hooks_path, &hooks_json, &config_toml, &plan.action)
}

/// Print where the agent-mux codex handler stands with Codex's hook trust
/// after an install (or dry run). Shared by the local and `--host`
/// installers so both say exactly the same thing. A trusted, unchanged
/// handler needs no commentary.
fn report_codex_trust<W: Write>(
    out: &mut W,
    hooks_path: &Path,
    hooks_json: &str,
    config_toml: &str,
    action: &InstallAction,
) -> io::Result<()> {
    let trust = codex_hook_trust(hooks_path, hooks_json, config_toml);
    let unchanged_and_trusted = trust == CodexHookTrust::Trusted && *action == InstallAction::NoOp;
    if let Some(msg) = describe_codex_trust(&trust).filter(|_| !unchanged_and_trusted) {
        writeln!(out, "\n{msg}")?;
    }
    Ok(())
}

// ---- remote (`--host`) install ----------------------------------------
//
// The remote installer reuses the pure planners above; only the I/O moves
// onto the host's transport. Every remote step is one small `sh -c` script
// run through `Host::run` (ssh mechanics stay inside `Host`), with paths
// passed as positional arguments so nothing is spliced into the script
// text. The scripts are named constants so tests can recognise them.

/// Prints, one per line: `$HOME`, the codex home (`$CODEX_HOME`, else
/// `$HOME/.codex`), and the remote `agent-mux` path (empty if not on PATH).
/// Resolved remotely because the hook paths — and Codex's trust key, which
/// embeds the hooks file's absolute path — are the *remote* machine's.
pub const REMOTE_PROBE_SCRIPT: &str =
    r#"printf '%s\n' "$HOME" "${CODEX_HOME:-$HOME/.codex}"; command -v agent-mux || printf '\n'"#;
/// `cat "$1"` when it is a regular file, nothing otherwise (a missing
/// hooks/settings/config file is the fresh-install case, not an error).
pub const REMOTE_READ_OPTIONAL_SCRIPT: &str = r#"if [ -f "$1" ]; then cat "$1"; fi"#;
/// `mkdir -p "$1"`.
pub const REMOTE_MKDIR_SCRIPT: &str = r#"mkdir -p "$1""#;
/// One-time backup: copy `$2` to `$1` unless `$1` already exists; prints
/// `written` when it copied.
pub const REMOTE_BACKUP_SCRIPT: &str = r#"if [ ! -e "$1" ]; then cp "$2" "$1" && echo written; fi"#;
/// Atomic replace: `mv -f "$1" "$2"`.
pub const REMOTE_RENAME_SCRIPT: &str = r#"mv -f "$1" "$2""#;

/// Which agent's hook the installer targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookTarget {
    /// Claude Code's `Notification` hook in `~/.claude/settings.json`.
    Claude,
    /// Codex's `PermissionRequest` hook in `$CODEX_HOME/hooks.json`.
    Codex,
}

/// The remote paths the installer needs, from one [`REMOTE_PROBE_SCRIPT`]
/// round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHookEnv {
    pub home: PathBuf,
    pub codex_home: PathBuf,
    /// The remote `agent-mux` binary, if it's on the remote `PATH`.
    pub agent_mux: Option<PathBuf>,
}

impl RemoteHookEnv {
    /// The file the installer edits for `target` on this machine.
    #[must_use]
    pub fn settings_path(&self, target: HookTarget) -> PathBuf {
        match target {
            HookTarget::Claude => self.home.join(".claude").join("settings.json"),
            HookTarget::Codex => self.codex_hooks_path(),
        }
    }

    /// `$CODEX_HOME/hooks.json` on this machine.
    #[must_use]
    pub fn codex_hooks_path(&self) -> PathBuf {
        self.codex_home.join("hooks.json")
    }
}

/// Parse [`REMOTE_PROBE_SCRIPT`]'s stdout.
///
/// # Errors
/// [`io::ErrorKind::InvalidData`] when `$HOME` or the codex home is
/// missing or not absolute.
pub fn parse_remote_probe(stdout: &str) -> io::Result<RemoteHookEnv> {
    fn absolute(s: Option<&str>, what: &str) -> io::Result<PathBuf> {
        match s {
            Some(v) if v.starts_with('/') => Ok(PathBuf::from(v)),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("remote probe: {what} is not an absolute path: {other:?}"),
            )),
        }
    }
    let mut lines = stdout.lines().map(str::trim);
    let home = absolute(lines.next(), "$HOME")?;
    let codex_home = absolute(lines.next(), "codex home")?;
    let agent_mux = lines.next().filter(|s| !s.is_empty()).map(PathBuf::from);
    Ok(RemoteHookEnv {
        home,
        codex_home,
        agent_mux,
    })
}

/// Run one of the remote scripts above with positional `args`, failing on
/// a non-zero exit (with the remote stderr in the message).
fn run_remote_script(host: &dyn Host, script: &str, args: &[&str]) -> io::Result<String> {
    let mut sh_argv: Vec<&str> = vec!["-c", script, "sh"];
    sh_argv.extend_from_slice(args);
    let output = host.run(None, "sh", &sh_argv)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "remote command on {} failed: {}",
            host.id(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Resolve the remote home / codex home / agent-mux path.
///
/// # Errors
/// Transport failures, a failing probe, or unparseable output.
pub fn probe_remote_hook_env(host: &dyn Host) -> io::Result<RemoteHookEnv> {
    parse_remote_probe(&run_remote_script(host, REMOTE_PROBE_SCRIPT, &[])?)
}

/// Read a remote file, treating "missing" as empty.
fn read_remote_optional(host: &dyn Host, path: &Path) -> io::Result<String> {
    run_remote_script(
        host,
        REMOTE_READ_OPTIONAL_SCRIPT,
        &[&path.to_string_lossy()],
    )
}

/// `agent-mux install-hooks [--agent codex] --host <name>`: install the
/// agent's hook on a remote machine over `host`'s transport. Same planner,
/// same summary lines and same trust report as the local installer; the
/// handler points at the **remote** `agent-mux` (it runs there), and every
/// path is the remote machine's. The write is atomic (`tmp` + `mv`) with
/// the same one-time `.bak`, and is re-read and parsed afterwards.
///
/// # Errors
/// - the remote has no `agent-mux` on its `PATH` (install it there first);
/// - transport / remote-command failures;
/// - planner errors (malformed existing file), as `io::Error::other`.
pub fn install_hooks_on_host<W: Write>(
    host: &dyn Host,
    target: HookTarget,
    dry_run: bool,
    out: &mut W,
) -> io::Result<()> {
    let env = probe_remote_hook_env(host)?;
    let binary = env.agent_mux.clone().ok_or_else(|| {
        io::Error::other(format!(
            "agent-mux is not on the PATH of host {} — install it there first \
             (the hook runs on the machine the agent runs on)",
            host.id()
        ))
    })?;
    let path = env.settings_path(target);
    let current = read_remote_optional(host, &path)?;
    let (plan, what) = match target {
        HookTarget::Claude => (
            plan_install(&current, &binary).map_err(io::Error::other)?,
            "Notification hook",
        ),
        HookTarget::Codex => (
            plan_install_codex(&current, &binary).map_err(io::Error::other)?,
            "lifecycle hooks",
        ),
    };
    writeln!(out, "Host: {}", host.id())?;
    writeln!(out, "Settings file: {}", path.display())?;
    writeln!(out, "agent-mux binary: {}", binary.display())?;
    writeln!(out, "Action: {}", describe_action(&plan.action, what))?;
    let changed = !matches!(plan.action, InstallAction::NoOp);
    if changed && dry_run {
        writeln!(out, "\n--- dry run: planned {} ---", file_label(&path))?;
        out.write_all(plan.new_content.as_bytes())?;
    } else if changed {
        write_remote_atomically(host, &path, &current, &plan.new_content, out)?;
        writeln!(
            out,
            "\nSettings updated. Restart agent-mux if it's already running."
        )?;
    }
    if target == HookTarget::Codex {
        let hooks_json = if dry_run || !changed {
            plan.new_content.clone()
        } else {
            read_remote_optional(host, &path)?
        };
        let config_toml = read_remote_optional(host, &env.codex_home.join("config.toml"))?;
        report_codex_trust(out, &path, &hooks_json, &config_toml, &plan.action)?;
    }
    Ok(())
}

/// mkdir the parent, take the one-time backup, write `tmp`, `mv` into
/// place, then re-read and parse — the remote twin of the local
/// `apply_install_plan` write path.
fn write_remote_atomically<W: Write>(
    host: &dyn Host,
    path: &Path,
    current: &str,
    new_content: &str,
    out: &mut W,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        run_remote_script(host, REMOTE_MKDIR_SCRIPT, &[&parent.to_string_lossy()])?;
    }
    if !current.is_empty() {
        let backup = backup_path_for(path);
        let copied = run_remote_script(
            host,
            REMOTE_BACKUP_SCRIPT,
            &[&backup.to_string_lossy(), &path.to_string_lossy()],
        )?;
        if copied.contains("written") {
            writeln!(out, "Backup written: {}", backup.display())?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    host.write_file(&tmp, new_content)?;
    run_remote_script(
        host,
        REMOTE_RENAME_SCRIPT,
        &[&tmp.to_string_lossy(), &path.to_string_lossy()],
    )?;
    let written = read_remote_optional(host, path)?;
    serde_json::from_str::<Value>(&written)
        .map_err(|e| io::Error::other(format!("post-write verification failed: {e}")))?;
    Ok(())
}

/// Best-effort remote trust classification of the agent-mux codex hook,
/// for the per-host discovery thread's footer hint. `None` on any probe or
/// read failure — a hint is never worth an error.
#[must_use]
pub fn remote_codex_hook_trust(host: &dyn Host) -> Option<CodexHookTrust> {
    let env = probe_remote_hook_env(host).ok()?;
    let hooks_path = env.codex_hooks_path();
    let hooks_json = read_remote_optional(host, &hooks_path).ok()?;
    let config_toml = read_remote_optional(host, &env.codex_home.join("config.toml")).ok()?;
    Some(codex_hook_trust(&hooks_path, &hooks_json, &config_toml))
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
        InstallAction::RemovedLegacy => {
            format!("removed a legacy agent-mux {what} entry no longer needed")
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

    // ---- codex installer (WP8; PermissionRequest-only since 2026-10-02) ----

    const CODEX_CMD: &str = "/usr/local/bin/agent-mux hook --agent codex";

    #[test]
    fn plan_install_codex_creates_only_the_permission_request_handler_when_empty() {
        let plan = plan_install_codex("", &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::Added);
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        assert_eq!(
            value["hooks"]["PermissionRequest"][0]["hooks"][0]["command"],
            CODEX_CMD
        );
        assert!(
            value["hooks"].get("Stop").is_none(),
            "turn-complete comes from the rollout; no Stop handler"
        );
    }

    #[test]
    fn plan_install_codex_is_noop_when_handler_present() {
        let input = r#"{
  "hooks": {
    "PermissionRequest": [{"hooks": [{"type": "command", "command": "/usr/local/bin/agent-mux hook --agent codex"}]}]
  }
}"#;
        let plan = plan_install_codex(input, &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::NoOp);
    }

    #[test]
    fn plan_install_codex_removes_legacy_stop_handler() {
        // An earlier install wrote both handlers; re-running drops ours from
        // Stop (and the now-empty key) while keeping PermissionRequest.
        let input = r#"{
  "hooks": {
    "PermissionRequest": [{"hooks": [{"type": "command", "command": "/usr/local/bin/agent-mux hook --agent codex"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "/usr/local/bin/agent-mux hook --agent codex"}]}]
  }
}"#;
        let plan = plan_install_codex(input, &binary()).unwrap();
        assert_eq!(plan.action, InstallAction::RemovedLegacy);
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        assert!(value["hooks"].get("Stop").is_none());
        assert_eq!(
            value["hooks"]["PermissionRequest"][0]["hooks"][0]["command"],
            CODEX_CMD
        );
    }

    #[test]
    fn plan_install_codex_legacy_stop_removal_keeps_foreign_stop_handlers() {
        let input = r#"{
  "hooks": {
    "Stop": [
      {"hooks": [{"type": "command", "command": "/other/tool"}, {"type": "command", "command": "/old/agent-mux hook --agent codex"}]},
      {"hooks": [{"type": "command", "command": "/old/agent-mux hook --agent codex"}]}
    ]
  }
}"#;
        let plan = plan_install_codex(input, &binary()).unwrap();
        let value: Value = serde_json::from_str(&plan.new_content).unwrap();
        let stop = value["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "the emptied group is dropped");
        assert_eq!(stop[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "/other/tool");
    }

    #[test]
    fn plan_install_codex_updates_stale_path_in_place() {
        let input = r#"{
  "hooks": {
    "PermissionRequest": [{"hooks": [{"type": "command", "command": "/old/agent-mux hook --agent codex"}]}]
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
        assert_eq!(
            value["hooks"]["PermissionRequest"][0]["hooks"][0]["command"],
            CODEX_CMD
        );
    }

    #[test]
    fn plan_install_codex_preserves_unrelated_hook_events() {
        // A user's own SessionStart handler must survive untouched.
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
        assert_eq!(arr[1]["hooks"][0]["command"], CODEX_CMD);
    }

    // ---- codex hook trust ----

    fn hooks_with_ours_at_group(group: usize) -> String {
        let mut groups: Vec<Value> = (0..group)
            .map(|_| json!({"hooks": [{"type": "command", "command": "/other/tool"}]}))
            .collect();
        groups.push(json!({"hooks": [{"type": "command", "command": CODEX_CMD}]}));
        json!({"hooks": {"PermissionRequest": groups}}).to_string()
    }

    #[test]
    fn codex_hook_trust_not_installed_without_our_handler() {
        let p = Path::new("/h/.codex/hooks.json");
        assert_eq!(codex_hook_trust(p, "", ""), CodexHookTrust::NotInstalled);
        assert_eq!(
            codex_hook_trust(p, r#"{"hooks":{"Stop":[]}}"#, ""),
            CodexHookTrust::NotInstalled
        );
    }

    #[test]
    fn codex_hook_trust_untrusted_without_a_state_record() {
        let p = Path::new("/h/.codex/hooks.json");
        assert_eq!(
            codex_hook_trust(p, &hooks_with_ours_at_group(0), "model = \"x\"\n"),
            CodexHookTrust::Untrusted
        );
    }

    #[test]
    fn codex_hook_trust_reads_the_positional_state_key() {
        // Key shape mirrors codex 0.142.5 `hook_key`:
        // "<hooks file>:permission_request:<group>:<handler>".
        let p = Path::new("/h/.codex/hooks.json");
        let config = r#"
[hooks.state."/h/.codex/hooks.json:permission_request:1:0"]
trusted_hash = "sha256:abc"
"#;
        assert_eq!(
            codex_hook_trust(p, &hooks_with_ours_at_group(1), config),
            CodexHookTrust::Trusted
        );
        // A record for a different position doesn't count.
        assert_eq!(
            codex_hook_trust(p, &hooks_with_ours_at_group(0), config),
            CodexHookTrust::Untrusted
        );
    }

    #[test]
    fn codex_hook_trust_disabled_wins_over_trusted_hash() {
        let p = Path::new("/h/.codex/hooks.json");
        let config = r#"
[hooks.state."/h/.codex/hooks.json:permission_request:0:0"]
enabled = false
trusted_hash = "sha256:abc"
"#;
        assert_eq!(
            codex_hook_trust(p, &hooks_with_ours_at_group(0), config),
            CodexHookTrust::Disabled
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
        assert!(value["hooks"].get("Stop").is_none());
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("NOT run this hook until you trust it"),
            "fresh install explains Codex's trust gate: {out}"
        );
    }

    #[test]
    fn install_codex_hooks_at_is_quiet_about_trust_when_unchanged_and_trusted() {
        let tmp = TempDir::new().unwrap();
        let hooks = tmp.path().join("hooks.json");
        install_codex_hooks_at(&hooks, &binary(), false, &mut Vec::new()).unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            format!(
                "[hooks.state.\"{}:permission_request:0:0\"]\ntrusted_hash = \"sha256:x\"\n",
                hooks.display()
            ),
        )
        .unwrap();
        let mut out = Vec::new();
        install_codex_hooks_at(&hooks, &binary(), false, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("trust"), "nothing to say: {out}");
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

    // ---- remote (`--host`) install ----

    /// A stand-in remote machine: `run` executes the real `sh` scripts
    /// with `HOME` / `PATH` (and optionally `CODEX_HOME`) pointed into a
    /// temp dir, so the tests exercise the exact remote shell the installer
    /// ships, minus ssh. `write_file` is a plain local write.
    struct ShellHost {
        id: crate::session::HostId,
        home: PathBuf,
        path_dir: PathBuf,
        codex_home: Option<PathBuf>,
    }

    impl ShellHost {
        /// A host whose PATH has a fake `agent-mux` when `with_binary`.
        fn new(tmp: &TempDir, with_binary: bool) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let home = tmp.path().join("home");
            let path_dir = tmp.path().join("bin");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&path_dir).unwrap();
            if with_binary {
                let bin = path_dir.join("agent-mux");
                fs::write(&bin, "#!/bin/sh\n").unwrap();
                fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self {
                id: crate::session::HostId("devbox".into()),
                home,
                path_dir,
                codex_home: None,
            }
        }

        fn remote_binary(&self) -> String {
            self.path_dir.join("agent-mux").display().to_string()
        }
    }

    impl Host for ShellHost {
        fn id(&self) -> &crate::session::HostId {
            &self.id
        }
        fn list_transcripts(
            &self,
            _: &Path,
            _: &crate::agent::ListingSpec,
        ) -> io::Result<Vec<crate::host::TranscriptStat>> {
            unimplemented!("not used by the installer")
        }
        fn read_to_string(&self, _: &Path) -> io::Result<String> {
            unimplemented!("installer reads through run()")
        }
        fn read_tail(&self, _: &Path, _: u64) -> io::Result<String> {
            unimplemented!()
        }
        fn is_dir(&self, _: &Path) -> bool {
            unimplemented!()
        }
        fn read_many(&self, _: &[&Path]) -> io::Result<Vec<io::Result<String>>> {
            unimplemented!()
        }
        fn is_dir_many(&self, _: &[&Path]) -> io::Result<Vec<bool>> {
            unimplemented!()
        }
        fn run(
            &self,
            _cwd: Option<&Path>,
            program: &str,
            args: &[&str],
        ) -> io::Result<std::process::Output> {
            let mut cmd = std::process::Command::new(program);
            cmd.args(args)
                .env("HOME", &self.home)
                .env("PATH", format!("{}:/usr/bin:/bin", self.path_dir.display()))
                .env_remove("CODEX_HOME");
            if let Some(c) = &self.codex_home {
                cmd.env("CODEX_HOME", c);
            }
            cmd.output()
        }
        fn write_file(&self, path: &Path, content: &str) -> io::Result<()> {
            fs::write(path, content)
        }
        fn list_files(&self, _: &Path) -> io::Result<Vec<PathBuf>> {
            unimplemented!()
        }
        fn remove(&self, _: &Path) -> io::Result<()> {
            unimplemented!()
        }
        fn ssh_argv(&self, _: bool, _: &[&str]) -> Option<Vec<String>> {
            None
        }
    }

    #[test]
    fn parse_remote_probe_reads_home_codex_home_and_binary() {
        let env = parse_remote_probe("/home/u\n/home/u/.codex\n/usr/bin/agent-mux\n").unwrap();
        assert_eq!(env.home, PathBuf::from("/home/u"));
        assert_eq!(
            env.codex_hooks_path(),
            PathBuf::from("/home/u/.codex/hooks.json")
        );
        assert_eq!(env.agent_mux, Some(PathBuf::from("/usr/bin/agent-mux")));
        assert_eq!(
            parse_remote_probe("/home/u\n/home/u/.codex\n\n")
                .unwrap()
                .agent_mux,
            None,
            "an empty third line means agent-mux isn't on the remote PATH"
        );
        assert!(parse_remote_probe("relative\n/x\n").is_err());
        assert!(parse_remote_probe("").is_err());
    }

    #[test]
    fn remote_codex_install_writes_remote_hooks_file_pointing_at_remote_binary() {
        let tmp = TempDir::new().unwrap();
        let host = ShellHost::new(&tmp, true);
        let mut out = Vec::new();
        install_hooks_on_host(&host, HookTarget::Codex, false, &mut out).unwrap();
        let hooks = host.home.join(".codex/hooks.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
        assert_eq!(
            value["hooks"]["PermissionRequest"][0]["hooks"][0]["command"],
            format!("{} hook --agent codex", host.remote_binary())
        );
        assert!(
            !host.home.join(".codex/hooks.json.tmp").exists(),
            "tmp renamed into place"
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Host: devbox"), "{out}");
        assert!(out.contains("added agent-mux"), "{out}");
        assert!(
            out.contains("NOT run this hook until you trust it"),
            "same trust report as the local installer: {out}"
        );
    }

    #[test]
    fn remote_codex_install_honours_remote_codex_home() {
        let tmp = TempDir::new().unwrap();
        let mut host = ShellHost::new(&tmp, true);
        let codex_home = tmp.path().join("relocated-codex");
        host.codex_home = Some(codex_home.clone());
        install_hooks_on_host(&host, HookTarget::Codex, false, &mut Vec::new()).unwrap();
        assert!(codex_home.join("hooks.json").exists());
        assert!(!host.home.join(".codex/hooks.json").exists());
    }

    #[test]
    fn remote_install_errors_when_agent_mux_is_not_on_the_remote_path() {
        let tmp = TempDir::new().unwrap();
        let host = ShellHost::new(&tmp, false);
        let err =
            install_hooks_on_host(&host, HookTarget::Codex, false, &mut Vec::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not on the PATH of host devbox"),
            "clear install-it-there-first error: {msg}"
        );
        assert!(!host.home.join(".codex/hooks.json").exists());
    }

    #[test]
    fn remote_install_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let host = ShellHost::new(&tmp, true);
        let mut out = Vec::new();
        install_hooks_on_host(&host, HookTarget::Codex, true, &mut out).unwrap();
        assert!(!host.home.join(".codex").exists(), "nothing created");
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("dry run"), "{out}");
        assert!(out.contains("hook --agent codex"), "{out}");
    }

    #[test]
    fn remote_install_backs_up_once_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let host = ShellHost::new(&tmp, true);
        let hooks = host.home.join(".codex/hooks.json");
        fs::create_dir_all(hooks.parent().unwrap()).unwrap();
        // A legacy install pointing at an old path → updated in place.
        fs::write(
            &hooks,
            r#"{"hooks":{"PermissionRequest":[{"hooks":[{"type":"command","command":"/old/agent-mux hook --agent codex"}]}]}}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        install_hooks_on_host(&host, HookTarget::Codex, false, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Backup written"), "{out}");
        assert!(host.home.join(".codex/hooks.json.bak").exists());
        let after_first = fs::read_to_string(&hooks).unwrap();
        let mut out = Vec::new();
        install_hooks_on_host(&host, HookTarget::Codex, false, &mut out).unwrap();
        assert_eq!(fs::read_to_string(&hooks).unwrap(), after_first);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("already configured")
        );
    }

    #[test]
    fn remote_claude_install_targets_remote_settings_json() {
        let tmp = TempDir::new().unwrap();
        let host = ShellHost::new(&tmp, true);
        install_hooks_on_host(&host, HookTarget::Claude, false, &mut Vec::new()).unwrap();
        let settings = host.home.join(".claude/settings.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(settings).unwrap()).unwrap();
        assert_eq!(
            value["hooks"]["Notification"][0]["hooks"][0]["command"],
            format!("{} hook", host.remote_binary())
        );
    }

    #[test]
    fn remote_codex_hook_trust_classifies_from_remote_files() {
        let tmp = TempDir::new().unwrap();
        let host = ShellHost::new(&tmp, true);
        // Nothing installed yet.
        assert_eq!(
            remote_codex_hook_trust(&host),
            Some(CodexHookTrust::NotInstalled)
        );
        install_hooks_on_host(&host, HookTarget::Codex, false, &mut Vec::new()).unwrap();
        assert_eq!(
            remote_codex_hook_trust(&host),
            Some(CodexHookTrust::Untrusted)
        );
        // Trust recorded under the *remote* absolute hooks path.
        let hooks = host.home.join(".codex/hooks.json");
        fs::write(
            host.home.join(".codex/config.toml"),
            format!(
                "[hooks.state.\"{}:permission_request:0:0\"]\ntrusted_hash = \"sha256:x\"\n",
                hooks.display()
            ),
        )
        .unwrap();
        assert_eq!(
            remote_codex_hook_trust(&host),
            Some(CodexHookTrust::Trusted)
        );
    }
}
