use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Stopped,
    Failed,
    Crashed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionProvider {
    Managed,
    TmuxAttach,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitContext {
    pub repo_root: String,
    pub worktree_root: String,
    pub branch: String,
    pub head_sha: String,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub schema_version: u32,
    pub session_id: String,
    pub session_slug: String,
    pub display_name: String,
    pub status: SessionStatus,
    pub provider: SessionProvider,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub pid: Option<i32>,
    pub exit_code: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub summary_last_line: Option<String>,
    pub stdout_path: String,
    pub stderr_path: String,
    pub combined_path: String,
    pub git_context: Option<GitContext>,
    pub provider_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogReadResult {
    pub chunk: String,
    pub next_offset: u64,
    pub eof: bool,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSearchMatch {
    pub byte_offset: u64,
    pub line_number: usize,
    pub line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSearchResult {
    pub matches: Vec<LogSearchMatch>,
    pub scanned_until_offset: u64,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogStats {
    pub bytes: u64,
    pub lines: usize,
    pub error_like_lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct StartSessionRequest {
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub command: String,
    pub env: Vec<(String, String)>,
    pub foreground: bool,
}

#[derive(Debug, Clone)]
pub struct AttachSessionRequest {
    pub name: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StopSessionRequest {
    pub session_id: String,
    pub grace_ms: u64,
}

#[derive(Debug, Clone)]
pub struct LogReadRequest {
    pub session_id: String,
    pub stream: String,
    pub offset: u64,
    pub max_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct LogSearchRequest {
    pub session_id: String,
    pub stream: String,
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub start_offset: u64,
    pub max_scan_bytes: usize,
    pub max_matches: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallerPlan {
    pub client: String,
    pub description: String,
    pub command_preview: Option<String>,
    pub config_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallerResult {
    pub client: String,
    pub dry_run: bool,
    pub plan: InstallerPlan,
    pub success: bool,
    pub details: String,
}
