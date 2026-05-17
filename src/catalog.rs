use crate::session::{Attention, Session, SessionId};

#[derive(Debug, Default)]
pub struct SessionCatalog {
    sessions: Vec<Session>,
}

impl SessionCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace_all(&mut self, mut sessions: Vec<Session>) {
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        self.sessions = sessions;
    }

    /// Append a newly-discovered session at the tail. Returns `true` if it
    /// was inserted, `false` if a session with the same id already exists.
    /// Appending (rather than sorting in) preserves the index a UI list
    /// state holds — the dashboard's selected row stays selected when a
    /// fresh session shows up mid-run.
    pub fn add(&mut self, session: Session) -> bool {
        if self.sessions.iter().any(|s| s.id == session.id) {
            return false;
        }
        self.sessions.push(session);
        true
    }

    /// Update the attention state of a session by id. Returns `true` if the
    /// session was found and updated.
    pub fn update_attention(&mut self, id: &SessionId, attention: Attention) -> bool {
        for session in &mut self.sessions {
            if session.id == *id {
                session.attention = attention;
                return true;
            }
        }
        false
    }

    #[must_use]
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::HostId;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn session(id: &str) -> Session {
        Session {
            id: SessionId(id.to_string()),
            host: HostId::local(),
            project_dir: PathBuf::from("/proj"),
            transcript_path: PathBuf::from(format!("/transcripts/{id}.jsonl")),
            last_activity: SystemTime::UNIX_EPOCH,
            attention: Attention::Unknown,
            title: None,
        }
    }

    #[test]
    fn add_appends_to_empty_catalog() {
        let mut c = SessionCatalog::new();
        assert!(c.add(session("a")));
        assert_eq!(c.len(), 1);
        assert_eq!(c.sessions()[0].id.0, "a");
    }

    #[test]
    fn add_appends_at_tail_preserving_existing_order() {
        let mut c = SessionCatalog::new();
        c.replace_all(vec![session("first"), session("second")]);
        assert!(c.add(session("third")));
        let ids: Vec<&str> = c.sessions().iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"]);
    }

    #[test]
    fn add_rejects_duplicate_id() {
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        assert!(!c.add(session("a")));
        assert_eq!(c.len(), 1);
    }
}
