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
        AttachSessionRequest, InstallerResult, LogReadRequest, LogReadResult, LogSearchRequest,
        LogSearchResult, LogStats, SessionMetadata, SessionStatus, StartSessionRequest,
        StopSessionRequest, Warning,
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
        Ok(sessions)
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
