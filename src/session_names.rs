//! User-editable session-name overrides.
//!
//! Persistent per-`(host, session_id)` rename store. The dashboard's
//! `r` keybind opens an inline edit over the selected row; the
//! resulting name overrides the transcript's agent title (and the
//! branch/commit fallback) when present, so a session whose
//! auto-derived title is unreadable for triage at a glance can be
//! renamed to something the user remembers.
//!
//! Persistence is a small JSON file under
//! `~/.cache/agent-mux/session_names.json`. The store survives
//! restarts; an empty/missing file degrades to "no overrides" rather
//! than failing startup. Writes are atomic (`tmp + rename`), matching
//! the per-host session cache's discipline so a crashed write never
//! leaves a half-truncated file.
//!
//! Sync model: the user's override sticks until they explicitly clear
//! it. An AI-derived title arriving later does *not* clobber the
//! override — once the user named something, they meant it. Set the
//! value to the empty string (or call `clear`) to remove the override
//! and let the auto-derivation take back over.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::{HostId, SessionId};

/// Default on-disk location for the override store. `Some(path)` when
/// the user's cache dir resolves; `None` only if `dirs::cache_dir`
/// returns nothing (unusual — every supported platform has one).
#[must_use]
pub fn default_store_path() -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("agent-mux")
            .join("session_names.json"),
    )
}

/// In-memory map of `(host_label, session_id) → override`. Loaded
/// from disk at startup, mutated by the rename keybind, flushed back
/// after every change so a crash doesn't lose the user's intent.
#[derive(Debug, Clone, Default)]
pub struct SessionNameStore {
    entries: BTreeMap<(String, String), String>,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OnDisk {
    /// Flat list of overrides, sorted by `(host, session_id)` so the
    /// on-disk JSON has a stable diff across writes.
    entries: Vec<DiskEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskEntry {
    host: String,
    session_id: String,
    name: String,
}

impl SessionNameStore {
    /// Empty store with no on-disk path. Used in tests and as the
    /// fallback when [`default_store_path`] is `None`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct from `path`, loading existing overrides if the file
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

    /// Lookup the override for `(host, session_id)`. Returns `None`
    /// when no override is set (the renderer falls back to AI title
    /// → first user message → cwd basename, as before).
    #[must_use]
    pub fn get(&self, host: &HostId, id: &SessionId) -> Option<&str> {
        self.entries
            .get(&(host.as_str().to_string(), id.0.clone()))
            .map(String::as_str)
    }

    /// Set the override for `(host, session_id)` to `name`. An empty
    /// string clears the override — equivalent to [`Self::clear`].
    /// Writes to disk immediately; an IO error is swallowed (the
    /// in-memory map still updates so the UI reflects the change).
    pub fn set(&mut self, host: &HostId, id: &SessionId, name: String) {
        let key = (host.as_str().to_string(), id.0.clone());
        if name.is_empty() {
            self.entries.remove(&key);
        } else {
            self.entries.insert(key, name);
        }
        let _ = self.persist();
    }

    /// Remove the override for `(host, session_id)`. Equivalent to
    /// `set(_, "")`; provided for callsites where the intent is
    /// clearer.
    pub fn clear(&mut self, host: &HostId, id: &SessionId) {
        let key = (host.as_str().to_string(), id.0.clone());
        if self.entries.remove(&key).is_some() {
            let _ = self.persist();
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Flush the current map to disk. Atomic via `tmp + rename`;
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
                .map(|((h, s), n)| DiskEntry {
                    host: h.clone(),
                    session_id: s.clone(),
                    name: n.clone(),
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

fn read_from_disk(path: &Path) -> Option<BTreeMap<(String, String), String>> {
    let bytes = fs::read(path).ok()?;
    let parsed: OnDisk = serde_json::from_slice(&bytes).ok()?;
    Some(
        parsed
            .entries
            .into_iter()
            .map(|e| ((e.host, e.session_id), e.name))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store_at(tmp: &TempDir) -> SessionNameStore {
        SessionNameStore::load_or_empty(tmp.path().join("session_names.json"))
    }

    #[test]
    fn empty_store_has_no_overrides() {
        let s = SessionNameStore::new();
        assert!(s.is_empty());
        assert!(s.get(&HostId::local(), &SessionId("x".into())).is_none());
    }

    #[test]
    fn set_then_get_returns_the_override() {
        let tmp = TempDir::new().unwrap();
        let mut s = store_at(&tmp);
        let host = HostId::local();
        let id = SessionId("abc".into());
        s.set(&host, &id, "refactor".into());
        assert_eq!(s.get(&host, &id), Some("refactor"));
    }

    #[test]
    fn set_with_empty_string_clears_the_override() {
        let tmp = TempDir::new().unwrap();
        let mut s = store_at(&tmp);
        let host = HostId::local();
        let id = SessionId("abc".into());
        s.set(&host, &id, "refactor".into());
        s.set(&host, &id, String::new());
        assert!(s.get(&host, &id).is_none());
    }

    #[test]
    fn clear_removes_the_override() {
        let tmp = TempDir::new().unwrap();
        let mut s = store_at(&tmp);
        let host = HostId::local();
        let id = SessionId("abc".into());
        s.set(&host, &id, "x".into());
        s.clear(&host, &id);
        assert!(s.get(&host, &id).is_none());
    }

    #[test]
    fn persist_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session_names.json");
        {
            let mut s = SessionNameStore::load_or_empty(path.clone());
            s.set(&HostId::local(), &SessionId("a".into()), "Alice".into());
            s.set(
                &HostId("alpenglow".into()),
                &SessionId("b".into()),
                "Bob".into(),
            );
        }
        let reloaded = SessionNameStore::load_or_empty(path);
        assert_eq!(
            reloaded.get(&HostId::local(), &SessionId("a".into())),
            Some("Alice"),
        );
        assert_eq!(
            reloaded.get(&HostId("alpenglow".into()), &SessionId("b".into())),
            Some("Bob"),
        );
    }

    #[test]
    fn malformed_file_degrades_to_empty_store() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session_names.json");
        fs::write(&path, "not json at all").unwrap();
        let s = SessionNameStore::load_or_empty(path);
        assert!(s.is_empty());
    }

    #[test]
    fn host_and_session_id_disambiguate_keys() {
        // Two sessions with the same `id.0` on different hosts must
        // hold independent overrides.
        let tmp = TempDir::new().unwrap();
        let mut s = store_at(&tmp);
        let id = SessionId("shared".into());
        s.set(&HostId::local(), &id, "local-name".into());
        s.set(&HostId("alpenglow".into()), &id, "remote-name".into());
        assert_eq!(s.get(&HostId::local(), &id), Some("local-name"));
        assert_eq!(s.get(&HostId("alpenglow".into()), &id), Some("remote-name"),);
    }
}
