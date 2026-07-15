use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::agent::{AgentCli, AgentKind, agent};
use crate::host::Host;
use crate::session::{Attention, HostId, Session};
use crate::worktree;

/// Transcripts older than this are dropped at the `list_transcripts`
/// boundary, before any per-transcript SSH `cat` / `test -d` round-trip.
/// Measured 2026-05-18 on a long-lived remote host: 233 transcripts × 4
/// sequential ssh round-trips per session ≈ 7+ minutes wall-clock through
/// a Coder proxy, almost all of it spent on weeks-cold conversations the
/// user is never going to attach to. 30 days is the initial cut; the
/// exact value will become a config knob in M5 (see TODO under
/// `#m5 #config`). Local discovery applies the same filter for parity —
/// the user's mental model of "old session" shouldn't shift based on
/// whether the box happens to be local or remote.
pub const DISCOVERY_MAX_AGE: Duration = Duration::from_hours(720);

/// Discover sessions by listing `root` (the agent's transcript root)
/// through the given `Host`. The same code path serves the local case and
/// the future SSH case — the only thing that varies is the `Host` impl.
///
/// Transcripts whose mtime is older than [`DISCOVERY_MAX_AGE`] are
/// dropped before any per-session work. See [`discover_with_cutoff`] for
/// the test-friendly variant that takes an explicit cutoff.
///
/// # Errors
/// Returns `io::Error` if `host.list_transcripts` or the per-transcript
/// reads fail. A missing `root` directory is treated as "no sessions"
/// (see [`crate::host::Host::list_transcripts`]) and yields an empty `Vec`.
pub fn discover(host: &dyn Host, root: &Path, kind: AgentKind) -> io::Result<Vec<Session>> {
    let cutoff = SystemTime::now()
        .checked_sub(DISCOVERY_MAX_AGE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    discover_with_cutoff(host, root, kind, cutoff)
}

/// Per-transcript intermediate built during phase 1 of bulk discovery.
/// Stays local to this module — the public surface still trades only in
/// fully-formed [`Session`]s.
struct Partial<'a> {
    stat: &'a crate::host::TranscriptStat,
    content: String,
    project_dir: PathBuf,
    attention: Attention,
}

/// Variant of [`discover`] that takes an explicit `cutoff`: transcripts
/// with mtime strictly older than `cutoff` are filtered. Lets tests pin
/// the boundary without mocking the clock.
///
/// # Errors
/// See [`discover`].
pub fn discover_with_cutoff(
    host: &dyn Host,
    root: &Path,
    kind: AgentKind,
    cutoff: SystemTime,
) -> io::Result<Vec<Session>> {
    // One (host, agent, root) triple: list via this agent's `ListingSpec`
    // and parse via its `parse_meta` / `derive`. Callers iterate
    // (host × enabled agents); each root is a distinct `list_transcripts`,
    // preserving the "one find per root, batched reads" remote discipline.
    let cli = agent(kind);
    let stats: Vec<_> = host
        .list_transcripts(root, &cli.listing())?
        .into_iter()
        .filter(|s| s.mtime >= cutoff)
        .collect();
    if stats.is_empty() {
        return Ok(Vec::new());
    }

    // Phase 1 — bulk-fetch every transcript in one round-trip. The
    // content is reused three times: to extract cwd/title metadata,
    // to derive initial attention, and (indirectly via the parsed
    // cwd) to drive the bulk is_dir / task.toml batches below. Over
    // a high-latency proxy this is the difference between O(N) ssh
    // round-trips and O(1).
    let transcript_paths: Vec<&Path> = stats.iter().map(|s| s.path.as_path()).collect();
    let transcript_contents = host.read_many(&transcript_paths)?;

    // Parse meta + derive attention for each transcript we could read.
    // Transcripts that failed the per-path read drop here — they won't
    // appear in the dashboard this tick, and the next poll will retry.
    let mut partials: Vec<Partial<'_>> = Vec::with_capacity(stats.len());
    for (stat, content_result) in stats.iter().zip(transcript_contents) {
        let Ok(content) = content_result else {
            continue;
        };
        let meta = cli.parse_meta(&content);
        let project_dir = meta
            .cwd
            .clone()
            .unwrap_or_else(|| cli.fallback_dir(&stat.path));
        let attention = cli.derive(&content, &project_dir).attention;
        partials.push(Partial {
            stat,
            content,
            project_dir,
            attention,
        });
    }

    // Phase 2 — bulk is_dir on the unique project_dirs. Multiple
    // sessions sharing a worktree (common — same repo, several
    // conversations) collapse to one stat call. Dedup is cheap
    // because partials.len() is small.
    let mut unique_dirs: Vec<PathBuf> = partials.iter().map(|p| p.project_dir.clone()).collect();
    unique_dirs.sort();
    unique_dirs.dedup();
    let unique_dir_refs: Vec<&Path> = unique_dirs.iter().map(PathBuf::as_path).collect();
    let exists = host.is_dir_many(&unique_dir_refs)?;
    let exists_by_dir: HashMap<&Path, bool> = unique_dir_refs
        .iter()
        .copied()
        .zip(exists.iter().copied())
        .collect();

    // Phase 3 — bulk-read task.toml and the worktree `.git` pointer
    // file in one round-trip each, only for project_dirs that actually
    // exist. NotFound results land per-path and turn into "no override
    // title" / "no parent repo" rather than failing the batch. Reading
    // `.git` lets the dashboard group worktree-backed sessions under
    // their parent repo instead of treating each worktree as its own
    // project (see `worktree::parse_parent_repo`); for a regular
    // checkout `.git` is a directory and `read_many` returns
    // `NotFound` (its remote `[ -f $p ]` test rejects directories),
    // which surfaces as `parent_repo = None` — exactly what we want.
    let task_dirs: Vec<&PathBuf> = unique_dirs
        .iter()
        .filter(|d| *exists_by_dir.get(d.as_path()).unwrap_or(&false))
        .collect();
    let task_toml_paths: Vec<PathBuf> = task_dirs
        .iter()
        .map(|d| worktree::task_metadata_path(d))
        .collect();
    let task_toml_path_refs: Vec<&Path> = task_toml_paths.iter().map(PathBuf::as_path).collect();
    let task_toml_results = host.read_many(&task_toml_path_refs)?;
    let task_toml_by_dir: HashMap<&Path, String> = task_dirs
        .iter()
        .map(|d| d.as_path())
        .zip(task_toml_results)
        .filter_map(|(d, r)| r.ok().map(|content| (d, content)))
        .collect();

    let git_pointer_paths: Vec<PathBuf> = task_dirs
        .iter()
        .map(|d| worktree::git_pointer_path(d))
        .collect();
    let git_pointer_path_refs: Vec<&Path> =
        git_pointer_paths.iter().map(PathBuf::as_path).collect();
    let git_pointer_results = host.read_many(&git_pointer_path_refs)?;
    let git_pointer_by_dir: HashMap<&Path, String> = task_dirs
        .iter()
        .map(|d| d.as_path())
        .zip(git_pointer_results)
        .filter_map(|(d, r)| r.ok().map(|content| (d, content)))
        .collect();

    // Phase 4 — assemble sessions from the now-resolved data. No I/O
    // beyond this point; this loop is pure CPU over in-memory inputs.
    let mut sessions = Vec::with_capacity(partials.len());
    for partial in &partials {
        let project_dir_exists = *exists_by_dir
            .get(partial.project_dir.as_path())
            .unwrap_or(&false);
        let task_toml = task_toml_by_dir
            .get(partial.project_dir.as_path())
            .map(String::as_str);
        let git_pointer = git_pointer_by_dir
            .get(partial.project_dir.as_path())
            .map(String::as_str);
        if let Some(s) = assemble_session(
            cli,
            host.id(),
            &partial.stat.path,
            partial.stat.mtime,
            &partial.content,
            project_dir_exists,
            task_toml,
            git_pointer,
            partial.attention,
        ) {
            sessions.push(s);
        }
    }
    Ok(sessions)
}

/// Pure session-assembly: given a fully-fetched payload for one
/// transcript, produce a `Session` or `None` if it's not usable
/// (missing file stem, no surviving `project_dir`). No I/O — both
/// [`build_session`] (single-shot reads for the live-discovery path)
/// and [`discover_with_cutoff`] (bulk reads at startup) compose
/// around this function.
#[allow(clippy::too_many_arguments)] // pure-data assembly; bundling into a
// struct would just push the field-by-field threading one level up to the
// two call sites (bulk discovery + single-shot build_session) without
// reducing the actual coupling.
fn assemble_session(
    cli: &dyn AgentCli,
    host_id: &HostId,
    transcript_path: &Path,
    mtime: SystemTime,
    transcript_content: &str,
    project_dir_exists: bool,
    task_toml_content: Option<&str>,
    git_pointer_content: Option<&str>,
    attention: Attention,
) -> Option<Session> {
    let id = cli.session_id_from_path(transcript_path)?;
    let meta = cli.parse_meta(transcript_content);
    let project_dir = meta
        .cwd
        .clone()
        .unwrap_or_else(|| cli.fallback_dir(transcript_path));
    if !project_dir_exists {
        return None;
    }
    let task_meta = task_toml_content.and_then(|raw| worktree::parse_task_metadata(raw).ok());
    let task_title = task_meta.as_ref().map(|m| m.task.clone());
    // `started_at` (the "running <total>" cell) comes from the
    // agent-mux task metadata's creation time. External sessions have
    // no task.toml, so they get `None` and the cell is omitted —
    // deriving a start from the first transcript entry's timestamp is a
    // filed follow-up (it needs a date parser).
    let started_at = task_meta
        .as_ref()
        .map(|m| SystemTime::UNIX_EPOCH + Duration::from_secs(m.created_at));
    // Stillborn-transcript filter. A transcript with no `task.toml`
    // task name, no agent-title entry, and no real first user message
    // is the "/clear-and-walked-away" case: Claude Code creates the
    // file immediately on `/clear` (a `<local-command-caveat>` /
    // `<command-name>/clear</command-name>` envelope pair plus a
    // `file-history-snapshot` and a `system` entry), but if the user
    // never sends an actual prompt the transcript never accumulates
    // anything to identify it by. Surfacing it as a row titled only
    // by its session-id hash is pure clutter — the user can't even
    // tell which conversation it represents. Holding the row back
    // here keeps the dashboard clean; if the user does eventually
    // type, the next watcher event re-fires `NewTranscript` and the
    // session surfaces naturally with a meaningful title. Mirrors
    // the discovery-boundary discipline the subagent path-shape
    // filter follows: filter at ingestion, don't clean up after.
    if task_title.is_none() && meta.title.is_none() && meta.first_user_message.is_none() {
        return None;
    }
    let title = task_title.or(meta.title).or(meta.first_user_message);
    let parent_repo = git_pointer_content.and_then(worktree::parse_parent_repo);
    // Full edit history from the whole transcript buffer in hand — the
    // watcher's tail-derived updates union onto this as the conversation
    // continues (see `merge_edited_files`). Same buffer walk the attention
    // derivation does; discovery keeps the two calls separate (attention
    // in phase 1, edits here) exactly as before the trait.
    let edited_files = cli.derive(transcript_content, &project_dir).edited_files;
    Some(Session {
        id,
        host: host_id.clone(),
        agent: cli.kind(),
        project_dir,
        transcript_path: transcript_path.to_path_buf(),
        last_activity: mtime,
        attention,
        title,
        parent_repo,
        has_live_pane: None,
        hook_pinned: None,
        blocking_prompt: false,
        // The session entered its current state around when the
        // transcript last changed (exact for stopped states); live
        // transitions re-stamp this in the catalog.
        attention_entered_at: Some(mtime),
        started_at,
        edited_files,
        // Left empty at discovery — the git-status source is background +
        // transition-gated (see `App::refresh_git_changed_files`), not part
        // of the startup transcript read, so the startup set doesn't trigger
        // a git-status stampede. Populates on the session's first turn-end.
        git_changed_files: Vec::new(),
    })
}

/// Build a `Session` from a single transcript path and its mtime. Reused
/// by the transcript watcher's discovery flow when a new `.jsonl` appears
/// mid-run, so both startup discovery and live discovery produce
/// identically-shaped sessions.
///
/// Returns `Ok(None)` for transcripts that aren't usable as live
/// sessions: missing file stem (no derivable id), a `project_dir`
/// that isn't an existing directory on disk (the worktree was
/// deleted, or the transcript predates having `cwd` metadata and we
/// fell back to the `<unknown>` literal), or a transcript with no
/// content-identifying signal yet — no task.toml task name, no
/// an agent title, and no real user message (the post-`/clear`
/// stillborn case; the watcher's next event re-fires `NewTranscript`
/// once the user actually types). In any of those cases, the user
/// can't attach to or meaningfully recognise the session, so showing
/// it in the dashboard would only generate noise.
///
/// # Errors
/// Returns `io::Error` if the transcript cannot be read through the host.
pub fn build_session(
    host: &dyn Host,
    transcript_path: &Path,
    kind: AgentKind,
    mtime: SystemTime,
) -> io::Result<Option<Session>> {
    // The agent comes from the (host × agent) root the `NewTranscript`
    // event was routed through, so the right parser builds the session.
    let cli = agent(kind);
    if cli.session_id_from_path(transcript_path).is_none() {
        return Ok(None);
    }
    let content = host.read_to_string(transcript_path)?;
    let meta = cli.parse_meta(&content);
    let project_dir = meta
        .cwd
        .clone()
        .unwrap_or_else(|| cli.fallback_dir(transcript_path));
    let exists = host.is_dir(&project_dir);
    let (task_toml, git_pointer) = if exists {
        (
            host.read_to_string(&worktree::task_metadata_path(&project_dir))
                .ok(),
            host.read_to_string(&worktree::git_pointer_path(&project_dir))
                .ok(),
        )
    } else {
        (None, None)
    };
    Ok(assemble_session(
        cli,
        host.id(),
        transcript_path,
        mtime,
        &content,
        exists,
        task_toml.as_deref(),
        git_pointer.as_deref(),
        Attention::Unknown,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::LocalHost;
    use std::fs::{self, create_dir_all};

    /// Build the standard "real cwd + project entry" scaffolding under a
    /// fresh tempdir. Returns `(tempdir, projects_root, real_cwd)` so the
    /// tempdir's lifetime extends to the end of the test.
    fn setup_with_real_cwd() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let cwd = tmp.path().join("real-cwd");
        create_dir_all(&projects).unwrap();
        create_dir_all(&cwd).unwrap();
        (tmp, projects, cwd)
    }

    /// Test-local shorthand: every test in this module discovers claude
    /// sessions against the local filesystem, so wrap the explicit-host
    /// + explicit-agent call.
    fn discover_local(root: &Path) -> io::Result<Vec<Session>> {
        discover(&LocalHost::new(), root, AgentKind::Claude)
    }

    /// Force a file's mtime to a specific instant. Used by the recency-
    /// filter tests to age a transcript past the cutoff without waiting
    /// 30 days. Wraps `File::set_times` so the call site stays one-line.
    fn set_mtime(path: &Path, mtime: SystemTime) {
        let times = std::fs::FileTimes::new().set_modified(mtime);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_times");
        f.set_times(times).expect("set_times");
    }

    #[test]
    fn discovers_session_with_cwd_from_jsonl() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc-123.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "abc-123");
        assert_eq!(sessions[0].project_dir, cwd);
    }

    #[test]
    fn discovers_session_with_parent_repo_when_cwd_is_a_worktree() {
        // End-to-end: a transcript whose `cwd` is a git worktree must
        // surface `Session.parent_repo = Some(<parent>)`, derived from
        // the worktree's `.git` pointer file. This is what lets the
        // dashboard group worktree-backed sessions under the parent
        // repo header instead of fragmenting per-worktree.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        // Lay down a `.git` pointer file in the cwd so the discovery
        // bulk-read picks it up. The exact `worktrees/<id>` suffix
        // doesn't matter for parsing — only the segment up to
        // `/.git/worktrees/` matters.
        let parent_repo = cwd.parent().unwrap().join("the-parent");
        create_dir_all(&parent_repo).unwrap();
        fs::write(
            cwd.join(".git"),
            format!(
                "gitdir: {}/.git/worktrees/cwd-task\n",
                parent_repo.display()
            ),
        )
        .unwrap();

        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("wt.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project_dir, cwd);
        assert_eq!(
            sessions[0].parent_repo.as_deref(),
            Some(parent_repo.as_path())
        );
    }

    #[test]
    fn discovers_session_with_no_parent_repo_for_plain_checkout() {
        // Regular-checkout session: `.git` is a directory, not a
        // pointer file. The bulk-read should return `NotFound`
        // (LocalHost::read_many → fs::read_to_string of a directory
        // errors with IsADirectory on Unix; either way it's an Err)
        // and parent_repo should land as `None` — grouping falls
        // through to project_dir.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        // Plain .git directory, no pointer file.
        create_dir_all(cwd.join(".git")).unwrap();

        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("plain.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].parent_repo, None);
    }

    #[test]
    fn stale_cwd_session_is_filtered_out() {
        // The user's scenario: a transcript whose recorded cwd points at a
        // worktree that has since been deleted. The session is no longer
        // resumable, so it should not appear in the dashboard.
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let entry = projects.join("-deleted");
        create_dir_all(&entry).unwrap();
        let gone = tmp.path().join("deleted-worktree");
        // Note: we never `create_dir_all(&gone)`.
        fs::write(
            entry.join("abc.jsonl"),
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", gone.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert!(sessions.is_empty(), "got: {sessions:?}");
    }

    #[test]
    fn session_with_no_cwd_metadata_and_no_real_fallback_is_filtered() {
        // When the transcript has no cwd, build_session falls back to the
        // decoded project-dir-name (`-home-test-proj` → `/home/test/proj`).
        // The fallback path is unlikely to exist on a CI worker, so the
        // session is filtered. This is by design — such transcripts are
        // pre-cwd-metadata legacy entries that we can't attach to anyway.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-this-path-does-not-exist-anywhere-xyz");
        create_dir_all(&proj).unwrap();
        fs::write(proj.join("xyz.jsonl"), "{\"type\":\"system\"}\n").unwrap();

        let sessions = discover_local(tmp.path()).unwrap();
        assert!(sessions.is_empty(), "got: {sessions:?}");
    }

    #[test]
    fn returns_empty_when_root_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let sessions = discover_local(&missing).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn codex_discovery_lists_via_codex_listing_spec() {
        // WP2 routing: discovering with `AgentKind::Codex` must list the
        // codex tree (depth-4 `rollout-*.jsonl`) through the codex agent,
        // not the claude depth-2 shape. The stub parser yields no cwd, so
        // no Session survives assembly — but the *listing* must run through
        // the right spec, which we prove directly against the host.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".codex").join("sessions");
        let day = root.join("2026").join("07").join("09");
        create_dir_all(&day).unwrap();
        let rollout =
            day.join("rollout-2026-07-09T10-00-00-00000000-1111-2222-3333-444444444444.jsonl");
        fs::write(&rollout, "{\"type\":\"session_meta\"}\n").unwrap();
        // A claude-shaped depth-2 file that the codex spec must NOT pick up.
        let shallow = root.join("bucket");
        create_dir_all(&shallow).unwrap();
        fs::write(shallow.join("x.jsonl"), "{}\n").unwrap();

        let host = LocalHost::new();
        let listed = host
            .list_transcripts(&root, &agent(AgentKind::Codex).listing())
            .unwrap();
        let paths: Vec<_> = listed.iter().map(|s| s.path.clone()).collect();
        assert_eq!(
            paths,
            vec![rollout],
            "codex spec lists only depth-4 rollouts"
        );

        // discover() through the codex agent runs cleanly (stub parser →
        // no assembled session yet; the read path lands in WP3).
        let sessions = discover(&host, &root, AgentKind::Codex).unwrap();
        assert!(
            sessions.is_empty(),
            "stub codex parser assembles nothing: {sessions:?}"
        );
    }

    #[test]
    fn discovers_edited_files_from_transcript() {
        // End-to-end: an Edit/Write tool_use in the transcript surfaces
        // on Session.edited_files, most-recent-first, so the picker has a
        // seeded list from the first frame.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{cwd}\",\"message\":\"go\"}}\n\
                 {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Edit\",\"input\":{{\"file_path\":\"{cwd}/a.rs\"}}}}]}}}}\n\
                 {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Write\",\"input\":{{\"file_path\":\"{cwd}/b.rs\"}}}}]}}}}\n",
                cwd = cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(
            sessions[0].edited_files,
            vec![cwd.join("b.rs"), cwd.join("a.rs")]
        );
    }

    #[test]
    fn extracts_ai_title_from_transcript() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\"}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"Wire up the parser\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Wire up the parser"));
    }

    #[test]
    fn ai_title_uses_latest_entry_when_multiple() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\"}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"early guess\"}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"refined title\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("refined title"));
    }

    #[test]
    fn task_toml_title_overrides_ai_title() {
        let tmp = tempfile::tempdir().unwrap();
        // Transcript lives in projects/<encoded>; project_dir points at a
        // separate directory containing .agent-mux/task.toml.
        let proj_dir = tmp.path().join("worktree");
        let agent_mux_dir = proj_dir.join(".agent-mux");
        create_dir_all(&agent_mux_dir).unwrap();
        fs::write(
            agent_mux_dir.join("task.toml"),
            "task = \"explicit task name\"\n\
             base_branch = \"main\"\n\
             created_at = 0\n",
        )
        .unwrap();

        let projects = tmp.path().join("projects");
        let entry = projects.join("-worktree");
        create_dir_all(&entry).unwrap();
        let cwd_line = format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", proj_dir.display());
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{cwd_line}\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"auto title\"}}\n"
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("explicit task name"));
    }

    #[test]
    fn stillborn_transcript_with_no_signal_is_filtered() {
        // A transcript with a cwd entry but no real user message, no
        // agent title, and no task.toml is the post-`/clear`-and-walked-
        // away case. The session has nothing the user could recognise
        // it by; surfacing it produces a row titled only by its
        // session-id hash. Filter at the discovery boundary so the
        // catalog never sees the row.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert!(sessions.is_empty(), "got: {sessions:?}");
    }

    #[test]
    fn realistic_post_clear_transcript_is_filtered() {
        // The exact shape Claude Code writes to a fresh transcript when
        // the user runs `/clear` and never types again: a
        // file-history-snapshot, the local-command-caveat envelope, the
        // command-name envelope, and a system entry. The two user
        // entries are both slash-command envelopes (already filtered
        // for the title fallback by `is_slash_command_envelope`), so
        // `first_user_message` is None and the stillborn filter trips.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"file-history-snapshot\"}}\n\
                 {{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"<local-command-caveat>caveat</local-command-caveat>\"}}\n\
                 {{\"type\":\"user\",\"message\":\"<command-name>/clear</command-name>\"}}\n\
                 {{\"type\":\"system\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert!(sessions.is_empty(), "got: {sessions:?}");
    }

    #[test]
    fn task_toml_keeps_session_visible_with_no_user_message() {
        // The agent-mux-spawned worktree case: a fresh session created
        // via `n` has a `.agent-mux/task.toml` carrying the user's
        // declared task name, but the transcript may have no user
        // message yet (the user just spawned it and hasn't typed). The
        // task.toml is itself a signal of user intent, so the session
        // must surface — without it the user would spawn something
        // from inside agent-mux and see no row appear.
        let tmp = tempfile::tempdir().unwrap();
        let proj_dir = tmp.path().join("worktree");
        let agent_mux_dir = proj_dir.join(".agent-mux");
        create_dir_all(&agent_mux_dir).unwrap();
        fs::write(
            agent_mux_dir.join("task.toml"),
            "task = \"refactor the parser\"\n\
             base_branch = \"main\"\n\
             created_at = 0\n",
        )
        .unwrap();

        let projects = tmp.path().join("projects");
        let entry = projects.join("-worktree");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", proj_dir.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("refactor the parser"));
    }

    #[test]
    fn build_session_also_applies_stillborn_filter() {
        // The live-discovery path (NewTranscript → build_session) must
        // apply the same filter as bulk startup discovery — otherwise a
        // `/clear` mid-run would still leak a row that startup
        // discovery would have filtered. The watcher's retry loop
        // (silent drop on `Ok(None)`, re-emit on next Modify) then
        // naturally surfaces the row once the user actually types.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let path = entry.join("fresh.jsonl");
        fs::write(
            &path,
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();

        let session = build_session(
            &LocalHost::new(),
            &path,
            AgentKind::Claude,
            SystemTime::now(),
        )
        .unwrap();
        assert!(session.is_none(), "got: {session:?}");
    }

    #[test]
    fn ignores_non_jsonl_files() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(entry.join("memory"), "not a session").unwrap();
        fs::write(
            entry.join("real.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "real");
    }

    #[test]
    fn first_user_message_is_used_when_no_ai_title() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"refactor the parser\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("refactor the parser"));
    }

    #[test]
    fn ai_title_takes_precedence_over_first_user_message() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"Wire the parser\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Wire the parser"));
    }

    #[test]
    fn first_user_message_extracts_from_content_string_shape() {
        // The schema Claude Code writes in practice: message is an object
        // with role + content, content is a plain string.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"do the thing\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("do the thing"));
    }

    #[test]
    fn first_user_message_extracts_from_content_block_array() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"hello\"}},{{\"type\":\"text\",\"text\":\"world\"}}]}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("hello world"));
    }

    #[test]
    fn first_user_message_is_truncated_with_ellipsis() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let long: String = "a".repeat(200);
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"{long}\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        let title = sessions[0].title.as_deref().unwrap();
        assert!(title.ends_with('…'), "got: {title}");
        // 60-char cap (see the reference agent's first-user-message cap) + the ellipsis.
        assert_eq!(title.chars().count(), 61);
    }

    #[test]
    fn first_user_message_collapses_whitespace() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"line one\\n\\nline two\\t\\ttabbed\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("line one line two tabbed")
        );
    }

    #[test]
    fn tool_result_user_entries_do_not_count_as_first_user_message() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"toolUseResult\":{{\"stdout\":\"ok\"}}}}\n\
                 {{\"type\":\"user\",\"message\":\"the real first prompt\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("the real first prompt"));
    }

    #[test]
    fn local_command_caveat_envelope_is_skipped() {
        // The user's scenario: open Claude Code, type `/clear`, and the
        // first JSONL user entry has content like
        // "<local-command-caveat>The messages below were generated...
        // </local-command-caveat>" — Claude Code's CLI wrapper text,
        // not human prose. We should fall through to the next real
        // message (or to cwd) rather than display the envelope.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"<local-command-caveat>The messages below were generated by the user while running local commands. DO NOT respond.</local-command-caveat>\"}}\n\
                 {{\"type\":\"user\",\"message\":\"the real first prompt\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("the real first prompt"));
    }

    #[test]
    fn command_name_envelope_is_skipped() {
        // The other slash-command shape: a user message whose content
        // starts with `<command-name>/foo</command-name>` (often
        // followed by `<command-message>` / `<command-args>` tags).
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"<command-name>/clear</command-name>\"}}\n\
                 {{\"type\":\"user\",\"message\":\"real prompt\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("real prompt"));
    }

    #[test]
    fn slash_command_envelope_with_leading_whitespace_is_still_skipped() {
        // Defensive: leading whitespace (newlines, indentation) should
        // not cause the envelope predicate to miss.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"  \\n<local-command-caveat>x</local-command-caveat>\"}}\n\
                 {{\"type\":\"user\",\"message\":\"real prompt\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("real prompt"));
    }

    #[test]
    fn message_that_merely_mentions_envelope_tag_is_not_skipped() {
        // The predicate is anchored to the start of the trimmed text,
        // so a human message that quotes the tag (e.g. asking about
        // it) is not mistaken for an envelope.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"what does <local-command-caveat> mean?\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("what does <local-command-caveat> mean?")
        );
    }

    #[test]
    fn empty_user_message_is_skipped() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"   \"}}\n\
                 {{\"type\":\"user\",\"message\":\"second message\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("second message"));
    }

    #[test]
    fn cold_transcript_is_filtered_before_per_session_work() {
        // The performance win this filter exists for: a transcript older
        // than the cutoff is dropped at the `list_transcripts` boundary,
        // so the expensive `cat`/`is_dir`/title-read round-trips never
        // happen. We can't directly observe "no SSH calls were made"
        // through the LocalHost path, but we can prove the filtered
        // transcript doesn't appear in the result even though it would
        // have been a valid session by content.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let path = entry.join("cold.jsonl");
        fs::write(
            &path,
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();
        let now = SystemTime::now();
        // Age the transcript 60 days; cutoff at 30 days means it's
        // out — strictly older than the cutoff.
        set_mtime(&path, now - Duration::from_hours(1440));

        let cutoff = now - Duration::from_hours(720);
        let sessions =
            discover_with_cutoff(&LocalHost::new(), &projects, AgentKind::Claude, cutoff)
                .expect("discover");
        assert!(
            sessions.is_empty(),
            "cold transcript should be filtered: {sessions:?}"
        );
    }

    #[test]
    fn warm_transcript_survives_the_cutoff() {
        // Sibling test to the cold-filter one: a transcript still inside
        // the window must reach `build_session` and appear in the result.
        // Pins both halves of the boundary so a future change that flips
        // the comparison direction blows up here.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let path = entry.join("warm.jsonl");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();
        let now = SystemTime::now();
        // Age the transcript 5 days; cutoff at 30 days lets it through.
        set_mtime(&path, now - Duration::from_hours(120));

        let cutoff = now - Duration::from_hours(720);
        let sessions =
            discover_with_cutoff(&LocalHost::new(), &projects, AgentKind::Claude, cutoff)
                .expect("discover");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "warm");
    }

    #[test]
    fn discover_default_uses_max_age_constant() {
        // Without injecting a cutoff, `discover` should apply the public
        // `DISCOVERY_MAX_AGE` constant. Pin it by constructing a
        // transcript aged just past the constant and confirming the
        // bare `discover` call filters it.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let path = entry.join("aged.jsonl");
        fs::write(
            &path,
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();
        // 1 hour past the constant — well inside floating-point /
        // scheduler slack on the SystemTime::now() comparison.
        set_mtime(
            &path,
            SystemTime::now() - DISCOVERY_MAX_AGE - Duration::from_hours(1),
        );

        let sessions = discover_local(&projects).expect("discover");
        assert!(
            sessions.is_empty(),
            "discover() should honour DISCOVERY_MAX_AGE: {sessions:?}"
        );
    }
}
