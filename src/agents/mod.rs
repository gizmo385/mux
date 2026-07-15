//! Per-agent [`crate::agent::AgentCli`] implementations. Each agent is one
//! self-contained module — the property that lets a new agent CLI land as
//! one file plus fixtures rather than a codebase sweep. Claude Code is the
//! reference implementation; Codex and Pi are stubbed pending WP3/WP4.

pub mod claude;
pub mod codex;
pub mod pi;
