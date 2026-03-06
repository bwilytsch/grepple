use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use chrono::Utc;
use regex::Regex;

use crate::{
    error::{GreppleError, Result},
    installer::{Client, InstallRequest, install},
    log_ops,
    model::{
        AttachSessionRequest, InstallerResult, LogErrorCountRequest, LogErrorCounts,
        LogReadRequest, LogReadResult, LogSearchRequest, LogSearchResult, LogStats, RankedSession,
        SessionMetadata, SessionPresetKind, SessionPresetResult, SessionResolveResult,
        SessionStatus, StartSessionRequest, StopSessionRequest, Warning,
    },
    runtime::{
        RuntimeOptions, attach_tmux_session, build_default_name, list_tmux_panes, refresh_status,
        start_managed_session, stop_session,
    },
    storage::SessionStore,
};

#[derive(Debug, Clone)]
pub struct GreppleConfig {
    pub state_dir: PathBuf,
    pub ttl_days: i64,
    pub max_state_bytes: Option<u64>,
    pub compact_tail_lines: usize,
    pub archive_full_logs: bool,
    pub max_read_bytes: usize,
    pub max_search_scan_bytes: usize,
    pub max_search_matches: usize,
    pub redact_output: bool,
    pub runtime: RuntimeOptions,
}

impl Default for GreppleConfig {
    fn default() -> Self {
        let state_dir = std::env::var("GREPPLE_STATE_DIR")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                dirs::state_dir()
                    .or_else(|| dirs::home_dir().map(|d| d.join(".local/state")))
                    .map(|d| d.join("grepple"))
            })
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(".grepple-state")
            });

        Self {
            state_dir,
            ttl_days: 7,
            max_state_bytes: std::env::var("GREPPLE_MAX_STATE_BYTES")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|v| if v == 0 { None } else { Some(v) })
                .or(Some(2 * 1024 * 1024 * 1024)),
            compact_tail_lines: 10_000,
            archive_full_logs: true,
            max_read_bytes: 256 * 1024,
            max_search_scan_bytes: 10 * 1024 * 1024,
            max_search_matches: 200,
            redact_output: std::env::var("GREPPLE_REDACT")
                .map(|v| v != "0")
                .unwrap_or(true),
            runtime: RuntimeOptions::default(),
        }
    }
}

pub struct Grepple {
    pub config: GreppleConfig,
    store: SessionStore,
}

impl Grepple {
    pub fn new(config: GreppleConfig) -> Result<Self> {
        Self::new_with_cleanup(config, true)
    }

    pub fn new_for_mcp(config: GreppleConfig) -> Result<Self> {
        Self::new_with_cleanup(config, true)
    }

    fn new_with_cleanup(config: GreppleConfig, run_startup_cleanup: bool) -> Result<Self> {
        let store = match SessionStore::new(config.state_dir.clone(), config.ttl_days) {
            Ok(store) => store,
            Err(err) => {
                if let crate::error::GreppleError::Io(io_err) = &err {
                    if io_err.kind() == std::io::ErrorKind::PermissionDenied {
                        let fallback = std::env::current_dir()
                            .unwrap_or_else(|_| PathBuf::from("."))
                            .join(".grepple-state");
                        SessionStore::new(fallback, config.ttl_days)?
                    } else {
                        return Err(err);
                    }
                } else {
                    return Err(err);
                }
            }
        };
        let app = Self { config, store };
        if run_startup_cleanup {
            let _ = app.store.cleanup_expired_sessions();
            if let Some(max_state_bytes) = app.config.max_state_bytes {
                let _ = app.store.enforce_storage_limit(max_state_bytes);
            }
        }
        Ok(app)
    }

    pub fn start_session(&self, req: StartSessionRequest) -> Result<SessionMetadata> {
        start_managed_session(&self.store, req)
    }

    pub fn attach_session(&self, req: AttachSessionRequest) -> Result<SessionMetadata> {
        attach_tmux_session(&self.store, req)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>> {
        let mut sessions = self.store.list_sessions()?;
        for session in &mut sessions {
            if matches!(
                session.status,
                SessionStatus::Running | SessionStatus::Starting
            ) && session.provider == crate::model::SessionProvider::Managed
            {
                if let Ok(updated) = refresh_status(&self.store, &session.session_id) {
                    *session = updated;
                }
            }
        }
        sessions.sort_by(|a, b| {
            self.default_session_sort_key(b)
                .cmp(&self.default_session_sort_key(a))
        });
        Ok(sessions)
    }

    pub fn list_ranked_sessions(
        &self,
        caller_cwd: Option<&Path>,
        intent: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<RankedSession>> {
        let sessions = self.list_sessions()?;
        Ok(self.rank_sessions(sessions, caller_cwd, intent, false, limit))
    }

    pub fn current_repo_sessions(
        &self,
        caller_cwd: Option<&Path>,
        intent: Option<&str>,
        limit: Option<usize>,
    ) -> Result<SessionResolveResult> {
        let caller_cwd = caller_cwd
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| GreppleError::Tool("unable to determine caller cwd".to_string()))?;
        let sessions = self.list_sessions()?;
        let candidates =
            self.rank_sessions(sessions, Some(caller_cwd.as_path()), intent, true, limit);
        let session = candidates.first().cloned();
        let mut warnings = Vec::new();
        if session.is_none() {
            warnings.push(Warning {
                code: "NO_CURRENT_REPO_SESSION".to_string(),
                message: "no grepple session matched the current repo/worktree".to_string(),
                metadata: BTreeMap::new(),
            });
        }
        Ok(SessionResolveResult {
            session,
            candidates,
            warnings,
        })
    }

    pub fn pick_best_session(
        &self,
        caller_cwd: Option<&Path>,
        intent: Option<&str>,
        limit: Option<usize>,
    ) -> Result<SessionResolveResult> {
        let sessions = self.list_sessions()?;
        let candidates = self.rank_sessions(sessions, caller_cwd, intent, false, limit);
        let session = candidates.first().cloned();
        let mut warnings = Vec::new();
        if let Some(selected) = &session {
            warnings.extend(self.context_warnings(&selected.session, caller_cwd));
        } else {
            warnings.push(Warning {
                code: "NO_SESSION_FOUND".to_string(),
                message: "no grepple sessions available".to_string(),
                metadata: BTreeMap::new(),
            });
        }
        Ok(SessionResolveResult {
            session,
            candidates,
            warnings,
        })
    }

    pub fn session_status(
        &self,
        session_id: &str,
        caller_cwd: Option<&Path>,
    ) -> Result<(SessionMetadata, Vec<Warning>)> {
        let meta = refresh_status(&self.store, session_id)?;
        let warnings = self.context_warnings(&meta, caller_cwd);
        Ok((meta, warnings))
    }

    pub fn stop_session(&self, req: StopSessionRequest) -> Result<SessionMetadata> {
        let mut meta = stop_session(&self.store, req, &self.config.runtime)?;
        self.store.compact_session_logs(
            &meta.session_id,
            self.config.compact_tail_lines,
            self.config.archive_full_logs,
        )?;
        meta.summary_last_line = self.store.update_summary_from_combined(&meta.session_id)?;
        meta.updated_at = Utc::now();
        self.store.write_meta(&meta)?;
        if let Some(max_state_bytes) = self.config.max_state_bytes {
            let _ = self.store.enforce_storage_limit(max_state_bytes);
        }
        Ok(meta)
    }

    pub fn log_read(
        &self,
        req: LogReadRequest,
        caller_cwd: Option<&Path>,
    ) -> Result<LogReadResult> {
        let meta = self.store.read_meta(&req.session_id)?;
        let path = self.stream_path(&meta, &req.stream)?;
        let mut result = log_ops::read_logs(
            path,
            &LogReadRequest {
                max_bytes: req.max_bytes.min(self.config.max_read_bytes).max(1),
                ..req
            },
        )?;
        if self.config.redact_output {
            result.chunk = self.redact_text(&result.chunk);
        }
        result
            .warnings
            .extend(self.context_warnings(&meta, caller_cwd));
        Ok(result)
    }

    pub fn log_search(
        &self,
        req: LogSearchRequest,
        caller_cwd: Option<&Path>,
    ) -> Result<LogSearchResult> {
        let meta = self.store.read_meta(&req.session_id)?;
        let path = self.stream_path(&meta, &req.stream)?;
        let mut result = log_ops::search_logs(
            path,
            &LogSearchRequest {
                max_scan_bytes: req
                    .max_scan_bytes
                    .min(self.config.max_search_scan_bytes)
                    .max(1),
                max_matches: req.max_matches.min(self.config.max_search_matches).max(1),
                ..req
            },
        )?;
        if self.config.redact_output {
            for m in &mut result.matches {
                m.line = self.redact_text(&m.line);
            }
        }
        result
            .warnings
            .extend(self.context_warnings(&meta, caller_cwd));
        Ok(result)
    }

    pub fn log_tail(&self, session_id: &str, stream: &str, lines: usize) -> Result<String> {
        let meta = self.store.read_meta(session_id)?;
        let path = self.stream_path(&meta, stream)?;
        let tail = log_ops::tail_lines(path, lines)?;
        if self.config.redact_output {
            Ok(self.redact_text(&tail))
        } else {
            Ok(tail)
        }
    }

    pub fn log_stats(&self, session_id: &str, stream: &str) -> Result<LogStats> {
        let meta = self.store.read_meta(session_id)?;
        let path = self.stream_path(&meta, stream)?;
        log_ops::stats(path)
    }

    pub fn log_error_counts(
        &self,
        req: LogErrorCountRequest,
        caller_cwd: Option<&Path>,
    ) -> Result<LogErrorCounts> {
        let meta = self.store.read_meta(&req.session_id)?;
        let path = self.stream_path(&meta, &req.stream)?;
        let mut result = log_ops::error_counts(
            path,
            &LogErrorCountRequest {
                max_scan_bytes: req
                    .max_scan_bytes
                    .min(self.config.max_search_scan_bytes)
                    .max(1),
                max_matches: req.max_matches.min(self.config.max_search_matches).max(1),
                ..req
            },
        )?;
        result.session_id = meta.session_id.clone();
        if self.config.redact_output {
            for m in &mut result.recent_matches {
                m.line = self.redact_text(&m.line);
            }
        }
        result
            .warnings
            .extend(self.context_warnings(&meta, caller_cwd));
        Ok(result)
    }

    pub fn session_preset(
        &self,
        preset: SessionPresetKind,
        session_id: &str,
        stream: &str,
        window_ms: Option<i64>,
        caller_cwd: Option<&Path>,
    ) -> Result<SessionPresetResult> {
        let session = self.store.read_meta(session_id)?;
        let mut warnings = self.context_warnings(&session, caller_cwd);
        let stream = stream.to_string();
        let tail;
        let mut error_counts = None;
        let summary = match preset {
            SessionPresetKind::RecentErrors => {
                let counts = self.log_error_counts(
                    LogErrorCountRequest {
                        session_id: session_id.to_string(),
                        stream: stream.clone(),
                        query: None,
                        regex: false,
                        case_sensitive: false,
                        window_ms: Some(window_ms.unwrap_or(15 * 60 * 1000)),
                        max_scan_bytes: self.config.max_search_scan_bytes,
                        max_matches: 20,
                    },
                    caller_cwd,
                )?;
                warnings.extend(counts.warnings.clone());
                let window_label = format_window_label(counts.window_ms);
                let recent = counts.recent_matches.len();
                error_counts = Some(counts);
                tail = Some(self.log_tail(session_id, &stream, 80)?);
                format!(
                    "{} error-like lines in {}; showing {} recent matches",
                    error_counts.as_ref().map(|v| v.matching_lines).unwrap_or(0),
                    window_label,
                    recent
                )
            }
            SessionPresetKind::StartupFailures => {
                let search = self.log_search(
                    LogSearchRequest {
                        session_id: session_id.to_string(),
                        stream: stream.clone(),
                        query: "error|panic|exception|traceback|failed|fatal".to_string(),
                        regex: true,
                        case_sensitive: false,
                        start_offset: 0,
                        max_scan_bytes: self.config.max_search_scan_bytes.min(256 * 1024),
                        max_matches: 20,
                    },
                    caller_cwd,
                )?;
                warnings.extend(search.warnings.clone());
                tail = Some(self.log_tail(session_id, &stream, 60)?);
                format!(
                    "{} startup failure matches in the beginning of the log",
                    search.matches.len()
                )
            }
            SessionPresetKind::WatchErrors => {
                let counts = self.log_error_counts(
                    LogErrorCountRequest {
                        session_id: session_id.to_string(),
                        stream: stream.clone(),
                        query: None,
                        regex: false,
                        case_sensitive: false,
                        window_ms: Some(window_ms.unwrap_or(5 * 60 * 1000)),
                        max_scan_bytes: self.config.max_search_scan_bytes.min(512 * 1024),
                        max_matches: 20,
                    },
                    caller_cwd,
                )?;
                warnings.extend(counts.warnings.clone());
                tail = Some(self.log_tail(session_id, &stream, 120)?);
                let window_label = format_window_label(counts.window_ms);
                let matching = counts.matching_lines;
                error_counts = Some(counts);
                format!("{} recent watch-mode errors in {}", matching, window_label)
            }
            SessionPresetKind::SessionSummary => {
                let stats = self.log_stats(session_id, &stream)?;
                tail = Some(self.log_tail(session_id, &stream, 50)?);
                format!(
                    "{} is {} with {} lines and {} error-like lines",
                    session.display_name,
                    self.status_label(&session.status),
                    stats.lines,
                    stats.error_like_lines
                )
            }
        };

        Ok(SessionPresetResult {
            preset,
            session,
            stream,
            summary,
            tail,
            error_counts,
            warnings: dedupe_warnings(warnings),
        })
    }

    pub fn install_client(
        &self,
        client: &str,
        name: &str,
        env_pairs: &[(String, String)],
        dry_run: bool,
        force: bool,
        scope: &str,
    ) -> Result<InstallerResult> {
        let client = Client::parse(client)?;
        let env: BTreeMap<String, String> = env_pairs.iter().cloned().collect();

        install(InstallRequest {
            client,
            name: name.to_string(),
            env,
            dry_run,
            force,
            scope: scope.to_string(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        })
    }

    pub fn suggested_session_name(
        &self,
        explicit: Option<String>,
        cwd: &str,
        tmux_label: Option<&str>,
        command: Option<&str>,
    ) -> String {
        build_default_name(explicit, cwd, tmux_label, command)
    }

    pub fn list_tmux_targets(&self) -> Result<Vec<String>> {
        Ok(list_tmux_panes()?
            .into_iter()
            .map(|pane| format!("{} ({})", pane.label, pane.command))
            .collect())
    }

    pub fn cleanup_expired(&self) -> Result<Vec<String>> {
        self.store.cleanup_expired_sessions()
    }

    pub fn clear_sessions(&self) -> Result<Vec<String>> {
        self.store.clear_all_sessions()
    }

    fn rank_sessions(
        &self,
        sessions: Vec<SessionMetadata>,
        caller_cwd: Option<&Path>,
        intent: Option<&str>,
        current_repo_only: bool,
        limit: Option<usize>,
    ) -> Vec<RankedSession> {
        let caller_git = caller_cwd.and_then(crate::runtime::capture_git_context);
        let mut ranked = sessions
            .into_iter()
            .filter_map(|session| {
                let candidate = self.rank_session(session, caller_cwd, caller_git.as_ref(), intent);
                if current_repo_only
                    && !candidate.repo_match
                    && !candidate.worktree_match
                    && !candidate.cwd_match
                {
                    return None;
                }
                Some(candidate)
            })
            .collect::<Vec<_>>();

        ranked.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.session.last_activity_at.cmp(&a.session.last_activity_at))
                .then_with(|| b.session.updated_at.cmp(&a.session.updated_at))
                .then_with(|| b.session.created_at.cmp(&a.session.created_at))
        });

        if let Some(limit) = limit {
            ranked.truncate(limit.max(1));
        }

        ranked
    }

    fn rank_session(
        &self,
        session: SessionMetadata,
        caller_cwd: Option<&Path>,
        caller_git: Option<&crate::model::GitContext>,
        intent: Option<&str>,
    ) -> RankedSession {
        let mut score = 0_i64;
        let mut reasons = Vec::new();

        match session.status {
            SessionStatus::Running => {
                score += 500;
                reasons.push("running session".to_string());
            }
            SessionStatus::Starting => {
                score += 450;
                reasons.push("starting session".to_string());
            }
            SessionStatus::Crashed | SessionStatus::Failed => {
                score += 180;
                reasons.push("recent failed session".to_string());
            }
            SessionStatus::Stopped => {
                score += 60;
            }
        }

        if session.provider == crate::model::SessionProvider::Managed {
            score += 35;
            reasons.push("managed process".to_string());
        }

        let running = matches!(
            session.status,
            SessionStatus::Running | SessionStatus::Starting
        );

        let mut repo_match = false;
        let mut worktree_match = false;
        let mut branch_match = false;
        let mut cwd_match = false;

        if let (Some(caller_git), Some(session_git)) = (caller_git, session.git_context.as_ref()) {
            if caller_git.repo_root == session_git.repo_root {
                repo_match = true;
                score += 220;
                reasons.push("same repo".to_string());
            }
            if caller_git.worktree_root == session_git.worktree_root {
                worktree_match = true;
                score += 260;
                reasons.push("same worktree".to_string());
            }
            if caller_git.branch == session_git.branch {
                branch_match = true;
                score += 80;
                reasons.push("same branch".to_string());
            }
        }

        if let (Some(caller_cwd), Some(session_cwd)) = (caller_cwd, session.cwd.as_deref()) {
            let session_path = Path::new(session_cwd);
            if same_or_parent_path(caller_cwd, session_path)
                || same_or_parent_path(session_path, caller_cwd)
            {
                cwd_match = true;
                score += if caller_cwd == session_path { 190 } else { 120 };
                reasons.push("near caller cwd".to_string());
            }
        }

        let (label_bonus, label_reasons) = crate::classify::label_score(&session.labels, intent);
        score += label_bonus;
        reasons.extend(label_reasons);

        let command_text_bonus = session
            .command
            .as_deref()
            .map(|cmd| self.command_text_score(cmd))
            .unwrap_or(0);
        score += command_text_bonus;

        let command_match = label_bonus + command_text_bonus;

        let age = (Utc::now() - session.last_activity_at)
            .num_minutes()
            .clamp(0, 180);
        score += 180 - age;

        RankedSession {
            session,
            score,
            repo_match,
            worktree_match,
            branch_match,
            cwd_match,
            running,
            command_match: command_match > 0,
            label_match: label_bonus > 0,
            reasons,
        }
    }

    fn command_text_score(&self, command: &str) -> i64 {
        let lower = command.to_ascii_lowercase();
        if lower.contains("dev") || lower.contains("serve") || lower.contains("server") {
            30
        } else {
            0
        }
    }

    fn default_session_sort_key(
        &self,
        session: &SessionMetadata,
    ) -> (
        i8,
        chrono::DateTime<Utc>,
        chrono::DateTime<Utc>,
        chrono::DateTime<Utc>,
    ) {
        (
            match session.status {
                SessionStatus::Running => 4,
                SessionStatus::Starting => 3,
                SessionStatus::Crashed | SessionStatus::Failed => 2,
                SessionStatus::Stopped => 1,
            },
            session.last_activity_at,
            session.updated_at,
            session.created_at,
        )
    }

    fn status_label(&self, status: &SessionStatus) -> &'static str {
        match status {
            SessionStatus::Starting => "starting",
            SessionStatus::Running => "running",
            SessionStatus::Stopped => "stopped",
            SessionStatus::Failed => "failed",
            SessionStatus::Crashed => "crashed",
        }
    }

    fn stream_path<'a>(&self, meta: &'a SessionMetadata, stream: &str) -> Result<&'a Path> {
        let p = match stream {
            "stdout" => Path::new(&meta.stdout_path),
            "stderr" => Path::new(&meta.stderr_path),
            "combined" => Path::new(&meta.combined_path),
            other => {
                return Err(GreppleError::InvalidArgument(format!(
                    "invalid stream '{other}' (expected stdout|stderr|combined)"
                )));
            }
        };
        Ok(p)
    }

    fn context_warnings(&self, meta: &SessionMetadata, caller_cwd: Option<&Path>) -> Vec<Warning> {
        let mut warnings = Vec::new();
        let Some(caller_cwd) = caller_cwd else {
            return warnings;
        };

        let Some(session_git) = &meta.git_context else {
            return warnings;
        };

        let caller_git = crate::runtime::capture_git_context(caller_cwd);
        if let Some(caller_git) = caller_git {
            if caller_git.branch != session_git.branch
                || caller_git.worktree_root != session_git.worktree_root
            {
                let mut metadata = BTreeMap::new();
                metadata.insert("session_branch".to_string(), session_git.branch.clone());
                metadata.insert(
                    "session_worktree".to_string(),
                    session_git.worktree_root.clone(),
                );
                metadata.insert("caller_branch".to_string(), caller_git.branch);
                metadata.insert("caller_worktree".to_string(), caller_git.worktree_root);
                warnings.push(Warning {
                    code: "GIT_CONTEXT_MISMATCH".to_string(),
                    message: "session git context differs from caller cwd".to_string(),
                    metadata,
                });
            }
        }

        warnings
    }

    fn redact_text(&self, input: &str) -> String {
        let mut output = input.to_string();

        let bearer = Regex::new(r"(?i)(bearer\s+)[a-z0-9._\-]+").expect("valid regex");
        output = bearer.replace_all(&output, "${1}[REDACTED]").to_string();

        let kv = Regex::new(r#"(?i)\b(api[_-]?key|token|password|secret)\b(\s*[:=]\s*)([^\s"']+)"#)
            .expect("valid regex");
        output = kv.replace_all(&output, "${1}${2}[REDACTED]").to_string();

        output
    }
}

fn same_or_parent_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b || a.starts_with(&b) || b.starts_with(&a),
        _ => a == b || a.starts_with(b) || b.starts_with(a),
    }
}

fn format_window_label(window_ms: Option<i64>) -> String {
    match window_ms {
        Some(ms) if ms >= 60 * 60 * 1000 => format!("the last {}h", ms / (60 * 60 * 1000)),
        Some(ms) if ms >= 60 * 1000 => format!("the last {}m", ms / (60 * 1000)),
        Some(ms) if ms > 0 => format!("the last {}s", ms / 1000),
        _ => "the scanned log window".to_string(),
    }
}

fn dedupe_warnings(warnings: Vec<Warning>) -> Vec<Warning> {
    let mut seen = BTreeMap::new();
    let mut out = Vec::new();
    for warning in warnings {
        let key = format!(
            "{}:{}:{:?}",
            warning.code, warning.message, warning.metadata
        );
        if seen.insert(key, true).is_none() {
            out.push(warning);
        }
    }
    out
}
