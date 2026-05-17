use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use notify::{EventKind, RecursiveMode, Watcher};

use crate::session::{Attention, SessionId};

/// How much of the transcript's tail to read when deriving attention.
/// Transcripts are append-only JSONL; reading the last few KB is enough
/// to find the most recent meaningful entry without parsing the whole file.
const TAIL_BYTES: u64 = 32 * 1024;

#[derive(Debug)]
pub struct AttentionUpdate {
    pub id: SessionId,
    pub attention: Attention,
}

/// Events emitted by the watcher. `Attention` flows from filesystem events
/// against transcripts already registered with the watcher. `NewTranscript`
/// flows when a previously-unknown `.jsonl` appears under the discovery
/// root, so the dashboard can pull it into the catalog without a restart.
#[derive(Debug)]
pub enum WatcherEvent {
    Attention(AttentionUpdate),
    NewTranscript(PathBuf),
}

pub struct TranscriptWatcher {
    /// Kept alive for the lifetime of the dashboard; dropping it tears
    /// the notify backend down. We also call `.watch` on it from
    /// `add_target` in the no-recursive-root fallback path.
    watcher: notify::RecommendedWatcher,
    targets: Arc<Mutex<HashMap<PathBuf, SessionId>>>,
    event_tx: Sender<WatcherEvent>,
    /// True when a recursive watch on the projects root is active. When
    /// false, `add_target` falls back to per-file watches so the watcher
    /// still works in the degenerate no-discovery-root case.
    has_recursive_root: bool,
}

impl TranscriptWatcher {
    /// Start watching for transcript events.
    ///
    /// When `discovery_root` is `Some` and watchable, a single recursive
    /// watch covers every transcript under it — both attention updates
    /// for known files and discovery of new ones. When it is `None`, or
    /// the recursive watch fails (e.g. the dir is on a filesystem that
    /// can't be recursively watched), the watcher falls back to per-file
    /// watches for the `initial` set and `NewTranscript` events will not
    /// fire.
    ///
    /// Emits an initial `Attention` update for each session in `initial`
    /// synchronously before the watcher thread starts, so the UI never
    /// has to display `Unknown` for a session that has on-disk content.
    ///
    /// # Errors
    /// Returns `notify::Error` if the platform watcher cannot be created
    /// or, in the per-file fallback, if any of the initial paths cannot
    /// be watched.
    pub fn start(
        initial: Vec<(SessionId, PathBuf)>,
        discovery_root: Option<&Path>,
    ) -> notify::Result<(Self, Receiver<WatcherEvent>)> {
        let (event_tx, event_rx) = mpsc::channel::<WatcherEvent>();
        let (notify_tx, notify_rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(notify_tx)?;

        let has_recursive_root = match discovery_root {
            Some(root) => {
                // Ensure the dir exists so the watch attaches on first-run
                // (claude code would create it on its own eventually, but
                // we'd miss the discovery window).
                let _ = fs::create_dir_all(root);
                watcher.watch(root, RecursiveMode::Recursive).is_ok()
            }
            None => false,
        };

        if !has_recursive_root {
            for (_, path) in &initial {
                watcher.watch(path, RecursiveMode::NonRecursive)?;
            }
        }

        // Prime initial state so the UI shows real attention from frame one.
        for (id, path) in &initial {
            let _ = event_tx.send(WatcherEvent::Attention(AttentionUpdate {
                id: id.clone(),
                attention: derive_attention(path),
            }));
        }

        let targets: Arc<Mutex<HashMap<PathBuf, SessionId>>> = Arc::new(Mutex::new(
            initial.into_iter().map(|(id, p)| (p, id)).collect(),
        ));

        let targets_for_thread = Arc::clone(&targets);
        let event_tx_for_thread = event_tx.clone();
        thread::spawn(move || {
            for res in notify_rx {
                let Ok(event) = res else { continue };
                let is_create = matches!(event.kind, EventKind::Create(_));
                let is_modify = matches!(event.kind, EventKind::Modify(_));
                if !is_create && !is_modify {
                    continue;
                }
                for path in event.paths {
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let known_id = targets_for_thread
                        .lock()
                        .ok()
                        .and_then(|m| m.get(&path).cloned());
                    let outgoing = match known_id {
                        Some(id) => WatcherEvent::Attention(AttentionUpdate {
                            id,
                            attention: derive_attention(&path),
                        }),
                        None => WatcherEvent::NewTranscript(path),
                    };
                    if event_tx_for_thread.send(outgoing).is_err() {
                        return;
                    }
                }
            }
        });

        Ok((
            Self {
                watcher,
                targets,
                event_tx,
                has_recursive_root,
            },
            event_rx,
        ))
    }

    /// Register a newly-discovered transcript so future filesystem events
    /// against it become `Attention` updates rather than further
    /// `NewTranscript` emissions. Also emits an immediate initial
    /// `Attention` update so the dashboard reflects the session's current
    /// state without waiting for the next file write.
    ///
    /// # Errors
    /// In the no-recursive-root fallback path, returns `notify::Error` if
    /// the per-file watch cannot be installed. With a recursive root in
    /// place, this never fails.
    pub fn add_target(&mut self, id: SessionId, path: PathBuf) -> notify::Result<()> {
        if !self.has_recursive_root {
            self.watcher.watch(&path, RecursiveMode::NonRecursive)?;
        }
        let attention = derive_attention(&path);
        if let Ok(mut targets) = self.targets.lock() {
            targets.insert(path, id.clone());
        }
        let _ = self
            .event_tx
            .send(WatcherEvent::Attention(AttentionUpdate { id, attention }));
        Ok(())
    }
}

/// Derive an attention state from the most recent meaningful JSONL entry in
/// `transcript_path`. Reads only the last `TAIL_BYTES` of the file; the
/// (possibly truncated) first line is discarded by virtue of failing to
/// parse, and the remaining lines are walked to find the latest
/// conversational entry.
#[must_use]
pub fn derive_attention(transcript_path: &Path) -> Attention {
    let Ok(mut file) = File::open(transcript_path) else {
        return Attention::Unknown;
    };
    let Ok(metadata) = file.metadata() else {
        return Attention::Unknown;
    };
    let start = metadata.len().saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Attention::Unknown;
    }

    let mut last: Option<EntryKind> = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(kind) = classify(&value) {
            last = Some(kind);
        }
    }

    match last {
        Some(EntryKind::Assistant) => Attention::NeedsInput,
        Some(EntryKind::UserMessage | EntryKind::ToolResult) => Attention::Working,
        None => Attention::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Assistant,
    UserMessage,
    ToolResult,
}

fn classify(value: &serde_json::Value) -> Option<EntryKind> {
    let entry_type = value.get("type")?.as_str()?;
    match entry_type {
        "assistant" => Some(EntryKind::Assistant),
        "user" => {
            if value.get("toolUseResult").is_some() {
                Some(EntryKind::ToolResult)
            } else {
                Some(EntryKind::UserMessage)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let content = lines.join("\n") + "\n";
        fs::write(f.path(), content).unwrap();
        f
    }

    #[test]
    fn empty_file_is_unknown() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(derive_attention(f.path()), Attention::Unknown);
    }

    #[test]
    fn only_housekeeping_entries_is_unknown() {
        let f = write_jsonl(&[
            r#"{"type":"permission-mode","permissionMode":"default"}"#,
            r#"{"type":"file-history-snapshot"}"#,
            r#"{"type":"ai-title","title":"x"}"#,
        ]);
        assert_eq!(derive_attention(f.path()), Attention::Unknown);
    }

    #[test]
    fn last_assistant_means_needs_input() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":"hi"}"#,
            r#"{"type":"assistant","message":"hello"}"#,
        ]);
        assert_eq!(derive_attention(f.path()), Attention::NeedsInput);
    }

    #[test]
    fn last_user_message_means_working() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":"hello"}"#,
            r#"{"type":"user","message":"do thing"}"#,
        ]);
        assert_eq!(derive_attention(f.path()), Attention::Working);
    }

    #[test]
    fn last_tool_result_means_working() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":"running tool"}"#,
            r#"{"type":"user","toolUseResult":{"stdout":"ok"}}"#,
        ]);
        assert_eq!(derive_attention(f.path()), Attention::Working);
    }

    #[test]
    fn housekeeping_after_assistant_does_not_change_state() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":"hi"}"#,
            r#"{"type":"assistant","message":"hello"}"#,
            r#"{"type":"file-history-snapshot"}"#,
            r#"{"type":"ai-title","title":"x"}"#,
        ]);
        assert_eq!(derive_attention(f.path()), Attention::NeedsInput);
    }

    #[test]
    fn nonexistent_file_is_unknown() {
        let path = std::path::PathBuf::from("/nonexistent/path/foo.jsonl");
        assert_eq!(derive_attention(&path), Attention::Unknown);
    }
}
