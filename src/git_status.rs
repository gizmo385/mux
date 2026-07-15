//! Working-tree changed-files derivation via `git status`.
//!
//! The `{file}`-tool picker's primary source is transcript-derived
//! (`Session.edited_files` — files the agent touched via its edit tools).
//! That misses files changed *programmatically*: a script the agent ran
//! through `Bash`, codegen, a formatter, `sed`/`mv`. `git status` catches
//! exactly those — especially newly *generated* (untracked) files.
//!
//! Unlike the transcript source, this is *genuine I/O* — a subprocess
//! locally, an SSH round-trip remotely — so it must never run on the
//! session-switch / picker-open hot path (ARCHITECTURE.md: "session
//! switching never blocks on I/O"). Callers run [`changed_files`] on a
//! background thread and deliver the result into the catalog via a
//! [`crate::watcher::WatcherEvent::GitStatus`] event, exactly like the
//! attention watcher; the picker only ever reads the cached snapshot.

use std::path::{Path, PathBuf};

use crate::host::Host;
use crate::session::EDITED_FILES_CAP;

/// One shell command, one round-trip: print the repo top-level, then the
/// NUL-separated porcelain status. Porcelain paths are repo-root-relative
/// (verified: even run from a subdir, git reports `src/main.rs`, not
/// `main.rs`), so the top-level is what absolutises them for dedup against
/// the already-absolute `edited_files`. `--no-renames` reports a rename as
/// a delete of the old path + an add of the new, so the surviving file
/// still shows (as untracked/added) and the vanished one is filtered out
/// with the other deletions. `-z` emits paths verbatim (no C-quoting), so
/// spaces and other oddities round-trip. The `&&` means a non-repo cwd
/// (where `rev-parse` fails) yields a non-zero exit and an empty result.
const STATUS_SCRIPT: &str =
    "git rev-parse --show-toplevel && git status --porcelain=v1 -z --no-renames";

/// Files `git status` reports as changed in `cwd`'s working tree, as
/// absolute paths, capped at [`EDITED_FILES_CAP`]. Modified + untracked
/// (untracked already honours `.gitignore`, so build artefacts don't
/// flood); deletions excluded — the picker opens files, and a deleted
/// file is gone. A non-git `cwd`, a missing `git`, or any command failure
/// yields an empty list (the picker silently falls back to the
/// transcript-derived set).
///
/// Blocking I/O — call from a background thread, never the main loop.
#[must_use]
pub fn changed_files(host: &dyn Host, cwd: &Path) -> Vec<PathBuf> {
    let output = match host.run(Some(cwd), "sh", &["-c", STATUS_SCRIPT]) {
        Ok(o) if o.status.success() => o.stdout,
        // Non-zero exit (not a repo, git missing) or an I/O error: no
        // changed-files signal, fall back to transcript-only. Not an
        // error worth surfacing — a session whose cwd isn't a git repo
        // is a normal case (the `N` no-worktree flow in a plain dir).
        _ => return Vec::new(),
    };
    parse_status(&output)
}

/// Parse the [`STATUS_SCRIPT`] stdout — first line the repo top-level,
/// then NUL-separated `XY <path>` porcelain records — into absolute paths,
/// excluding deletions and capping at [`EDITED_FILES_CAP`].
fn parse_status(stdout: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(stdout);
    // Split off the top-level line (from `rev-parse`); the remainder is
    // the NUL-separated porcelain body.
    let mut parts = text.splitn(2, '\n');
    let toplevel = parts.next().unwrap_or("").trim();
    if toplevel.is_empty() {
        return Vec::new();
    }
    let base = PathBuf::from(toplevel);
    let body = parts.next().unwrap_or("");

    let mut out: Vec<PathBuf> = Vec::new();
    for record in body.split('\0') {
        // Each record is `XY <path>`: two status chars, a space, then the
        // repo-relative path. Anything shorter is the trailing empty split
        // or a malformed line.
        if record.len() < 4 {
            continue;
        }
        let bytes = record.as_bytes();
        let (x, y) = (bytes[0], bytes[1]);
        // Exclude deletions in either the index (`X`) or worktree (`Y`)
        // column — the file is gone, so there's nothing to open.
        if x == b'D' || y == b'D' {
            continue;
        }
        // Path starts after the two status chars and the separating space.
        // The first three bytes are ASCII, so byte index 3 is a valid char
        // boundary.
        let rel = &record[3..];
        if rel.is_empty() {
            continue;
        }
        out.push(base.join(rel));
        if out.len() >= EDITED_FILES_CAP {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(toplevel: &str, records: &[&str]) -> Vec<u8> {
        // Mirror STATUS_SCRIPT stdout: toplevel + '\n' + NUL-joined
        // records (git -z terminates each record with a NUL, so there is a
        // trailing NUL after the last one — modelled by the trailing "").
        let mut s = String::from(toplevel);
        s.push('\n');
        for r in records {
            s.push_str(r);
            s.push('\0');
        }
        s.into_bytes()
    }

    #[test]
    fn absolutises_repo_relative_paths_against_toplevel() {
        let out = parse_status(&body("/home/u/repo", &[" M src/main.rs", "?? gen/out.rs"]));
        assert_eq!(
            out,
            vec![
                PathBuf::from("/home/u/repo/src/main.rs"),
                PathBuf::from("/home/u/repo/gen/out.rs"),
            ]
        );
    }

    #[test]
    fn keeps_untracked_and_staged_and_modified() {
        // `??` untracked (the programmatic-generation case), `A ` staged
        // add, `M ` staged modify, ` M` unstaged modify, `MM` both.
        let out = parse_status(&body(
            "/r",
            &[
                "?? new.txt",
                "A  added.rs",
                "M  staged.rs",
                " M work.rs",
                "MM both.rs",
            ],
        ));
        let names: Vec<_> = out
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "/r/new.txt",
                "/r/added.rs",
                "/r/staged.rs",
                "/r/work.rs",
                "/r/both.rs"
            ]
        );
    }

    #[test]
    fn excludes_deletions_in_either_column() {
        let out = parse_status(&body(
            "/r",
            &[" D gone.rs", "D  staged-del.rs", " M kept.rs"],
        ));
        assert_eq!(out, vec![PathBuf::from("/r/kept.rs")]);
    }

    #[test]
    fn empty_toplevel_or_body_yields_nothing() {
        assert!(parse_status(b"").is_empty());
        assert!(parse_status(b"\n").is_empty());
        // Top-level but a clean tree (no records).
        assert!(parse_status(&body("/r", &[])).is_empty());
    }

    #[test]
    fn caps_at_edited_files_cap() {
        let records: Vec<String> = (0..EDITED_FILES_CAP + 50)
            .map(|i| format!(" M f{i}.rs"))
            .collect();
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let out = parse_status(&body("/r", &refs));
        assert_eq!(out.len(), EDITED_FILES_CAP);
    }

    #[test]
    fn path_with_spaces_round_trips_under_z() {
        // -z does not quote, so a spaced path arrives verbatim.
        let out = parse_status(&body("/r", &[" M dir with spaces/a b.rs"]));
        assert_eq!(out, vec![PathBuf::from("/r/dir with spaces/a b.rs")]);
    }

    /// End-to-end against a real repo through `LocalHost::run` — exercises
    /// the actual `sh -c` command, git's real `--porcelain -z` output, and
    /// the parser together (the unit tests above only feed synthetic
    /// bytes). Asserts on file *names*, not full paths, because
    /// `rev-parse --show-toplevel` returns the canonicalised repo root
    /// (e.g. `/private/var/…` on macOS) while the tempdir handle is the
    /// symlinked form (`/var/…`) — the documented `/tmp`↔`/private/tmp`
    /// dedup caveat, harmless here since both point at the same file.
    #[test]
    fn changed_files_reports_modified_and_untracked_excludes_deleted() {
        use crate::host::LocalHost;
        use std::process::Command;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(root)
                .args(args)
                .status()
                .expect("run git")
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(root.join("tracked.rs"), "one").unwrap();
        std::fs::write(root.join("todelete.rs"), "gone").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);

        // A conversation's aftermath: an edit-tool change, a programmatic
        // (untracked) file the edit log would miss, and a deletion.
        std::fs::write(root.join("tracked.rs"), "two").unwrap();
        std::fs::write(root.join("generated.rs"), "made by a script").unwrap();
        std::fs::remove_file(root.join("todelete.rs")).unwrap();

        let changed = changed_files(&LocalHost::new(), root);
        let names: Vec<String> = changed
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();

        assert!(
            changed.iter().all(|p| p.is_absolute()),
            "not absolute: {changed:?}"
        );
        assert!(
            names.contains(&"tracked.rs".to_string()),
            "modified missing: {names:?}"
        );
        assert!(
            names.contains(&"generated.rs".to_string()),
            "untracked (programmatic) missing: {names:?}"
        );
        assert!(
            !names.contains(&"todelete.rs".to_string()),
            "deleted file should be excluded: {names:?}"
        );
    }

    #[test]
    fn changed_files_on_non_git_dir_is_empty() {
        use crate::host::LocalHost;
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(changed_files(&LocalHost::new(), tmp.path()).is_empty());
    }
}
