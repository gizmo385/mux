use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
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

pub struct TranscriptWatcher {
    _watcher: notify::RecommendedWatcher,
}

impl TranscriptWatcher {
    /// Start watching `sessions` for filesystem events and derive attention
    /// state in a background thread. Emits an initial `AttentionUpdate` for
    /// each session synchronously before the watcher thread starts, so the
    /// UI never has to display `Unknown` for a session that has on-disk
    /// content.
    ///
    /// # Errors
    /// Returns `notify::Error` if the platform watcher cannot be created or
    /// any of the transcript paths cannot be watched.
    pub fn start(
        sessions: Vec<(SessionId, PathBuf)>,
    ) -> notify::Result<(Self, Receiver<AttentionUpdate>)> {
        let (update_tx, update_rx) = mpsc::channel::<AttentionUpdate>();
        let (notify_tx, notify_rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(notify_tx)?;
        for (_, path) in &sessions {
            watcher.watch(path, RecursiveMode::NonRecursive)?;
        }

        // Prime initial state so the UI shows real attention from frame one.
        for (id, path) in &sessions {
            let _ = update_tx.send(AttentionUpdate {
                id: id.clone(),
                attention: derive_attention(path),
            });
        }

        let id_by_path: HashMap<PathBuf, SessionId> =
            sessions.into_iter().map(|(id, p)| (p, id)).collect();

        thread::spawn(move || {
            for res in notify_rx {
                let Ok(event) = res else { continue };
                if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    continue;
                }
                for path in &event.paths {
                    if let Some(id) = id_by_path.get(path) {
                        let update = AttentionUpdate {
                            id: id.clone(),
                            attention: derive_attention(path),
                        };
                        if update_tx.send(update).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        Ok((Self { _watcher: watcher }, update_rx))
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
