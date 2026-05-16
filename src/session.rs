use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Attention {
    NeedsInput,
    Working,
    Idle,
    #[default]
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    pub host: Host,
    pub project_dir: PathBuf,
    pub transcript_path: PathBuf,
    pub last_activity: SystemTime,
    pub attention: Attention,
}
