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
