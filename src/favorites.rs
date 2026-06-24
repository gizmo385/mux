//! User-pinned session favorites.
//!
//! Persistent per-`(host, session_id)` map of sessions the user has
//! flagged for promotion to a pinned section at the top of the
//! sidebar. Decouples a handful of frequently-attended sessions from
//! the activity-driven reshuffle in `build_display_rows` (projects
//! re-sort by `max(last_activity) desc` on every transcript write, so
//! a session with constant activity never settles into a stable spot
//! the user can muscle-memory).
//!
//! Each entry carries cached display metadata ([`FavoriteMeta`]: the
//! last-seen title and project dir). That metadata is what lets the
//! sidebar render a favorite as a dimmed "unconfirmed" placeholder when
//! its live session isn't in the catalog yet — e.g. at startup before a
//! remote host's `ControlMaster` connects, or while a host is offline.
//! Without it, a favorited remote session blinks out the moment
//! `reconcile_host` drops the disk-cache seed and the live discovery
//! hasn't (or can't) re-prove it. The metadata is refreshed from the
//! live session whenever one is observed, so the placeholder shows a
//! real name rather than a bare id.
//!
//! Persistence is a small JSON file under
//! `~/.cache/agent-mux/favorites.json`. The store survives restarts;
//! an empty/missing file degrades to "no favorites." Writes are
//! atomic (`tmp + rename`), matching the [`SessionNameStore`] and
//! per-host session cache discipline so a crashed write never leaves
//! a half-truncated file. The on-disk schema is forward-compatible:
//! the metadata fields are `#[serde(default)]`, so a favorites file
//! written by an older build (keys only) loads cleanly and re-binds
//! its metadata the first time each session is seen live.
//!
//! [`SessionNameStore`]: crate::session_names::SessionNameStore
//!
//! ## Stale entries
//!
//! When a favorited session is deleted (or its host stops being
//! configured), the entry stays on disk. If the same `(host, id)` ever
//! reappears via discovery, the favorite re-binds without any explicit
//! re-toggle. A favorite whose session is permanently gone renders as a
//! placeholder the user can dismiss with the favorite keybind (it
//! resolves the `(host, id)` from the placeholder row, same as a live
//! row) — so a stuck favorite is one keystroke from gone rather than a
//! disk-edit.
//!
//! ## Cross-machine sync
//!
//! Deliberately not portable: local session ids are uuids that don't
//! exist anywhere else, and remote session ids are scoped to their
//! host. A "favorite" makes sense only against the catalog this
//! machine sees, which is exactly what the per-machine cache stores.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::{HostId, SessionId};

/// Cached display metadata for a favorited session. Lets the sidebar
/// render a favorite even when its live session isn't in the catalog
/// (a dimmed "unconfirmed" placeholder), and lets search match a
/// placeholder by title/project. Refreshed from the live session
/// whenever one is observed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FavoriteMeta {
    /// Display title last seen for this favorite. `None` until a live
    /// session or the toggle site supplies one (then the placeholder
    /// falls back to a short id suffix, same as a title-less row).
    pub title: Option<String>,
    /// Working directory last seen. Cached so search can match a
    /// placeholder by project even when the live session is absent.
    pub project_dir: Option<PathBuf>,
}

/// Default on-disk location for the favorites store. `Some(path)`
/// when the user's cache dir resolves; `None` only if
/// `dirs::cache_dir` returns nothing (unusual — every supported
/// platform has one).
#[must_use]
pub fn default_store_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("agent-mux").join("favorites.json"))
}

/// In-memory map of `(host_label, session_id)` → cached metadata for
/// the sessions the user has favorited. Loaded from disk at startup,
/// mutated by the `f` keybind, flushed back after every change so a
/// crash doesn't lose the user's intent.
///
/// Backed by [`BTreeMap`] so the on-disk JSON has a stable diff
/// across writes (`HashMap` iteration is non-deterministic and would
/// thrash the file every save).
#[derive(Debug, Clone, Default)]
pub struct FavoritesStore {
    entries: BTreeMap<(String, String), FavoriteMeta>,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OnDisk {
    entries: Vec<DiskEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskEntry {
    host: String,
    session_id: String,
    /// `#[serde(default)]` so a favorites file written by an older
    /// keys-only build still loads; `skip_serializing_if` keeps the
    /// JSON tidy for entries we haven't seen live yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_dir: Option<PathBuf>,
}

impl FavoritesStore {
    /// Empty store with no on-disk path. Used in tests and as the
    /// fallback when [`default_store_path`] is `None`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct from `path`, loading existing favorites if the file
    /// exists. A malformed file degrades silently to an empty store
    /// — the user shouldn't lose access to their dashboard because
    /// the cache file got corrupted.
    #[must_use]
    pub fn load_or_empty(path: PathBuf) -> Self {
        let entries = read_from_disk(&path).unwrap_or_default();
        Self {
            entries,
            path: Some(path),
        }
    }

    /// True iff `(host, session_id)` is currently favorited.
    #[must_use]
    pub fn contains(&self, host: &HostId, id: &SessionId) -> bool {
        self.entries
            .contains_key(&(host.as_str().to_string(), id.0.clone()))
    }

    /// Flip the favorite state for `(host, session_id)`. When adding,
    /// `meta` seeds the cached metadata (so a brand-new favorite can
    /// render as a placeholder immediately); on removal `meta` is
    /// ignored. Returns the new state (`true` = now favorited) so the
    /// caller can render a status line without re-querying. Persists
    /// immediately; IO failure is swallowed (the in-memory map still
    /// updates so the UI reflects the change — same dropped-write
    /// trade-off as [`SessionNameStore::set`]).
    ///
    /// [`SessionNameStore::set`]: crate::session_names::SessionNameStore::set
    pub fn toggle(&mut self, host: &HostId, id: &SessionId, meta: FavoriteMeta) -> bool {
        let key = (host.as_str().to_string(), id.0.clone());
        // `remove` returns the prior value: `Some` means it was
        // favorited and is now cleared, `None` means we just added it.
        let now_favorited = if self.entries.remove(&key).is_some() {
            false
        } else {
            self.entries.insert(key, meta);
            true
        };
        let _ = self.persist();
        now_favorited
    }

    /// Refresh the cached metadata for an already-favorited
    /// `(host, id)` from a freshly-observed live session. No-op (and
    /// no disk write) when the pair isn't favorited or the metadata is
    /// unchanged — cheap to call on every catalog drain. Returns
    /// whether anything changed.
    pub fn record_meta(&mut self, host: &HostId, id: &SessionId, meta: FavoriteMeta) -> bool {
        let key = (host.as_str().to_string(), id.0.clone());
        let Some(existing) = self.entries.get_mut(&key) else {
            return false;
        };
        if *existing == meta {
            return false;
        }
        *existing = meta;
        let _ = self.persist();
        true
    }

    /// Iterate every favorited `(host, id)` with its cached metadata.
    /// The sidebar uses this to build placeholder rows for favorites
    /// whose live session isn't currently in the catalog.
    pub fn entries(&self) -> impl Iterator<Item = (HostId, SessionId, &FavoriteMeta)> + '_ {
        self.entries
            .iter()
            .map(|((h, s), meta)| (HostId(h.clone()), SessionId(s.clone()), meta))
    }

    /// Number of favorited entries currently held. Used by the
    /// sidebar render path to decide whether to emit the pinned
    /// section header at all (an empty favorites set should produce
    /// no `── favorites ──` row, not an empty group).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Flush the current set to disk. Atomic via `tmp + rename`;
    /// returns the IO error so tests can assert on it without
    /// requiring the store to expose its `path`.
    ///
    /// # Errors
    /// Returns any IO error from creating the parent dir, writing
    /// the temp file, or renaming into place.
    pub fn persist(&self) -> io::Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let on_disk = OnDisk {
            entries: self
                .entries
                .iter()
                .map(|((h, s), meta)| DiskEntry {
                    host: h.clone(),
                    session_id: s.clone(),
                    title: meta.title.clone(),
                    project_dir: meta.project_dir.clone(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&on_disk)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn read_from_disk(path: &Path) -> Option<BTreeMap<(String, String), FavoriteMeta>> {
    let bytes = fs::read(path).ok()?;
    let parsed: OnDisk = serde_json::from_slice(&bytes).ok()?;
    Some(
        parsed
            .entries
            .into_iter()
            .map(|e| {
                (
                    (e.host, e.session_id),
                    FavoriteMeta {
                        title: e.title,
                        project_dir: e.project_dir,
                    },
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store_at(tmp: &TempDir) -> FavoritesStore {
        FavoritesStore::load_or_empty(tmp.path().join("favorites.json"))
    }

    fn meta(title: &str) -> FavoriteMeta {
        FavoriteMeta {
            title: Some(title.to_string()),
            project_dir: Some(PathBuf::from("/work/proj")),
        }
    }

    #[test]
    fn empty_store_has_no_favorites() {
        let s = FavoritesStore::new();
        assert!(s.is_empty());
        assert!(!s.contains(&HostId::local(), &SessionId("x".into())));
    }

    #[test]
    fn toggle_adds_a_favorite_when_absent_and_reports_true() {
        let tmp = TempDir::new().unwrap();
        let mut s = store_at(&tmp);
        let host = HostId::local();
        let id = SessionId("abc".into());
        assert!(
            s.toggle(&host, &id, meta("hello")),
            "first toggle should add"
        );
        assert!(s.contains(&host, &id));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn toggle_removes_a_favorite_when_present_and_reports_false() {
        let tmp = TempDir::new().unwrap();
        let mut s = store_at(&tmp);
        let host = HostId::local();
        let id = SessionId("abc".into());
        s.toggle(&host, &id, meta("hello"));
        assert!(
            !s.toggle(&host, &id, FavoriteMeta::default()),
            "second toggle should remove"
        );
        assert!(!s.contains(&host, &id));
        assert!(s.is_empty());
    }

    #[test]
    fn persist_round_trips_metadata_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("favorites.json");
        {
            let mut s = FavoritesStore::load_or_empty(path.clone());
            s.toggle(&HostId::local(), &SessionId("a".into()), meta("Refactor"));
            s.toggle(
                &HostId("alpenglow".into()),
                &SessionId("b".into()),
                meta("Deploy"),
            );
        }
        let reloaded = FavoritesStore::load_or_empty(path);
        assert!(reloaded.contains(&HostId::local(), &SessionId("a".into())));
        assert!(reloaded.contains(&HostId("alpenglow".into()), &SessionId("b".into())));
        assert_eq!(reloaded.len(), 2);
        // Cached metadata survives the round-trip so a placeholder can
        // render a real title before the live session arrives.
        let (_, _, m) = reloaded
            .entries()
            .find(|(h, id, _)| h == &HostId::local() && id == &SessionId("a".into()))
            .expect("entry present");
        assert_eq!(m.title.as_deref(), Some("Refactor"));
        assert_eq!(m.project_dir.as_deref(), Some(Path::new("/work/proj")));
    }

    #[test]
    fn keys_only_file_from_older_build_still_loads() {
        // Forward-compat: a favorites.json written before metadata
        // fields existed has entries with only host + session_id. It
        // must load (metadata defaults to None) rather than being
        // treated as corrupt and silently dropping the user's pins.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("favorites.json");
        fs::write(&path, r#"{"entries":[{"host":"local","session_id":"a"}]}"#).unwrap();
        let s = FavoritesStore::load_or_empty(path);
        assert!(s.contains(&HostId::local(), &SessionId("a".into())));
        let (_, _, m) = s.entries().next().expect("entry present");
        assert_eq!(m.title, None);
        assert_eq!(m.project_dir, None);
    }

    #[test]
    fn record_meta_updates_only_existing_favorites_and_only_on_change() {
        let tmp = TempDir::new().unwrap();
        let mut s = store_at(&tmp);
        let host = HostId::local();
        let id = SessionId("a".into());

        // No-op for a non-favorited pair.
        assert!(!s.record_meta(&host, &id, meta("X")));

        s.toggle(&host, &id, meta("old"));
        // Same metadata → no change, no write.
        assert!(!s.record_meta(&host, &id, meta("old")));
        // Different title → updates.
        assert!(s.record_meta(&host, &id, meta("new")));
        let (_, _, m) = s.entries().next().unwrap();
        assert_eq!(m.title.as_deref(), Some("new"));
    }

    #[test]
    fn malformed_file_degrades_to_empty_store() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("favorites.json");
        fs::write(&path, "not json at all").unwrap();
        let s = FavoritesStore::load_or_empty(path);
        assert!(s.is_empty());
    }

    #[test]
    fn host_and_session_id_disambiguate_keys() {
        // Two sessions with the same `id.0` on different hosts must
        // toggle independently.
        let tmp = TempDir::new().unwrap();
        let mut s = store_at(&tmp);
        let id = SessionId("shared".into());
        s.toggle(&HostId::local(), &id, FavoriteMeta::default());
        assert!(s.contains(&HostId::local(), &id));
        assert!(!s.contains(&HostId("alpenglow".into()), &id));
    }

    #[test]
    fn store_with_no_path_still_toggles_in_memory() {
        // Construction via `new()` (no path) is the fallback when
        // `default_store_path` returns `None`. The UI must still work
        // — toggles update the in-memory map, persist is a no-op.
        let mut s = FavoritesStore::new();
        let host = HostId::local();
        let id = SessionId("x".into());
        assert!(s.toggle(&host, &id, FavoriteMeta::default()));
        assert!(s.contains(&host, &id));
        assert!(s.persist().is_ok(), "persist with no path is a no-op");
    }

    #[test]
    fn persist_atomic_via_tmp_and_rename() {
        // The `.tmp + rename` discipline matters because a partial
        // write that the loader sees as the canonical file would
        // silently drop favorites on the next start. Sanity-check
        // that no stray `.tmp` survives a successful persist.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("favorites.json");
        let mut s = FavoritesStore::load_or_empty(path.clone());
        s.toggle(
            &HostId::local(),
            &SessionId("a".into()),
            FavoriteMeta::default(),
        );
        assert!(path.exists());
        let tmp_path = path.with_extension("json.tmp");
        assert!(!tmp_path.exists(), "tmp file should have been renamed");
    }

    #[test]
    fn on_disk_entries_are_sorted_for_stable_diffs() {
        // The `BTreeMap` backing means the on-disk JSON's entry order
        // is deterministic across writes — so a session-management
        // checked-in dotfile (or anyone diffing two snapshots) sees
        // signal-only diffs. Insert out of order, then read the file
        // back and verify host ordering is alphabetical.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("favorites.json");
        let mut s = FavoritesStore::load_or_empty(path.clone());
        s.toggle(
            &HostId("zeta".into()),
            &SessionId("a".into()),
            FavoriteMeta::default(),
        );
        s.toggle(
            &HostId("alpha".into()),
            &SessionId("a".into()),
            FavoriteMeta::default(),
        );
        let bytes = fs::read(&path).unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        let alpha_pos = body.find("alpha").expect("alpha host present");
        let zeta_pos = body.find("zeta").expect("zeta host present");
        assert!(alpha_pos < zeta_pos, "entries should sort by host");
    }
}
