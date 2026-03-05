use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use chrono::{Duration, Utc};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::{
    error::{GreppleError, Result},
    model::{SCHEMA_VERSION, SessionMetadata, SessionStatus},
};

#[derive(Debug, Clone)]
pub struct SessionStore {
    pub state_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub ttl_days: i64,
}

impl SessionStore {
    pub fn new(state_dir: PathBuf, ttl_days: i64) -> Result<Self> {
        fs::create_dir_all(&state_dir)?;
        let sessions_dir = state_dir.join("sessions");
        fs::create_dir_all(&sessions_dir)?;
        Ok(Self {
            state_dir,
            sessions_dir,
            ttl_days,
        })
    }

    pub fn allocate_session_id(&self) -> String {
        Ulid::new().to_string()
    }

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(session_id)
    }

    pub fn meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("meta.json")
    }

    pub fn create_session_files(&self, session_id: &str) -> Result<(PathBuf, PathBuf, PathBuf)> {
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir)?;
        let stdout_path = dir.join("stdout.log");
        let stderr_path = dir.join("stderr.log");
        let combined_path = dir.join("combined.log");
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)?;
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)?;
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&combined_path)?;
        Ok((stdout_path, stderr_path, combined_path))
    }

    pub fn write_meta(&self, meta: &SessionMetadata) -> Result<()> {
        let path = self.meta_path(&meta.session_id);
        let tmp = path.with_extension("json.tmp");
        let mut f = File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut f, meta)?;
        f.write_all(b"\n")?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn read_meta(&self, session_id: &str) -> Result<SessionMetadata> {
        let path = self.meta_path(session_id);
        if !path.exists() {
            return Err(GreppleError::SessionNotFound(session_id.to_string()));
        }
        let data = fs::read_to_string(path)?;
        let mut meta: SessionMetadata = serde_json::from_str(&data)?;
        if meta.schema_version != SCHEMA_VERSION {
            meta.schema_version = SCHEMA_VERSION;
            self.write_meta(&meta)?;
        }
        Ok(meta)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path().join("meta.json");
            if !path.exists() {
                continue;
            }
            let data = fs::read_to_string(path)?;
            if let Ok(mut meta) = serde_json::from_str::<SessionMetadata>(&data) {
                if meta.schema_version != SCHEMA_VERSION {
                    meta.schema_version = SCHEMA_VERSION;
                    let _ = self.write_meta(&meta);
                }
                out.push(meta);
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn append_event(&self, session_id: &str, kind: &str, payload: Value) -> Result<()> {
        let path = self.session_dir(session_id).join("events.ndjson");
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        let line = json!({
            "timestamp": Utc::now(),
            "kind": kind,
            "payload": payload,
        });
        f.write_all(serde_json::to_string(&line)?.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }

    pub fn update_summary_from_combined(&self, session_id: &str) -> Result<Option<String>> {
        let path = self.session_dir(session_id).join("combined.log");
        if !path.exists() {
            return Ok(None);
        }
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut last = None;
        for line in reader.lines() {
            let line = line?;
            if !line.trim().is_empty() {
                last = Some(line);
            }
        }
        Ok(last)
    }

    pub fn compact_session_logs(
        &self,
        session_id: &str,
        tail_lines: usize,
        archive_full: bool,
    ) -> Result<()> {
        for file in ["stdout.log", "stderr.log", "combined.log"] {
            let path = self.session_dir(session_id).join(file);
            if !path.exists() {
                continue;
            }
            let content = fs::read(&path)?;
            if archive_full {
                let archive_path = self
                    .session_dir(session_id)
                    .join(format!("{}.full.log", file.trim_end_matches(".log")));
                fs::write(archive_path, &content)?;
            }

            let text = String::from_utf8_lossy(&content);
            let lines: Vec<&str> = text.lines().collect();
            if lines.len() <= tail_lines {
                continue;
            }
            let kept = lines[lines.len().saturating_sub(tail_lines)..].join("\n");
            fs::write(&path, kept.as_bytes())?;
        }

        self.append_event(
            session_id,
            "compacted",
            json!({
                "tail_lines": tail_lines,
                "archive_full": archive_full,
            }),
        )?;

        Ok(())
    }

    pub fn cleanup_expired_sessions(&self) -> Result<Vec<String>> {
        let sessions = self.list_sessions()?;
        let mut deleted = Vec::new();
        let cutoff = Utc::now() - Duration::days(self.ttl_days);

        for meta in sessions {
            let ended = meta.stopped_at.unwrap_or(meta.updated_at);
            if ended >= cutoff {
                continue;
            }
            let dir = self.session_dir(&meta.session_id);
            if dir.exists() {
                fs::remove_dir_all(&dir)?;
                deleted.push(meta.session_id);
            }
        }
        Ok(deleted)
    }

    pub fn enforce_storage_limit(&self, max_bytes: u64) -> Result<Vec<String>> {
        if max_bytes == 0 {
            return Ok(Vec::new());
        }

        let mut total_bytes = self.dir_size_bytes(&self.state_dir)?;
        if total_bytes <= max_bytes {
            return Ok(Vec::new());
        }

        let mut sessions = self.list_sessions()?;
        sessions.sort_by(|a, b| {
            let a_ended = a.stopped_at.unwrap_or(a.updated_at);
            let b_ended = b.stopped_at.unwrap_or(b.updated_at);
            a_ended.cmp(&b_ended)
        });

        let mut deleted = Vec::new();
        for meta in sessions {
            if matches!(
                meta.status,
                SessionStatus::Running | SessionStatus::Starting
            ) {
                continue;
            }
            if total_bytes <= max_bytes {
                break;
            }
            let dir = self.session_dir(&meta.session_id);
            if !dir.exists() {
                continue;
            }
            let dir_bytes = self.dir_size_bytes(&dir)?;
            fs::remove_dir_all(&dir)?;
            total_bytes = total_bytes.saturating_sub(dir_bytes);
            deleted.push(meta.session_id);
        }

        Ok(deleted)
    }

    fn dir_size_bytes(&self, dir: &Path) -> Result<u64> {
        if !dir.exists() {
            return Ok(0);
        }
        let mut total = 0_u64;
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry.map_err(|err| {
                GreppleError::Tool(format!("failed to scan '{}': {err}", dir.display()))
            })?;
            if entry.file_type().is_file() {
                let metadata = entry.metadata().map_err(|err| {
                    GreppleError::Tool(format!(
                        "failed to read metadata under '{}': {err}",
                        dir.display()
                    ))
                })?;
                total = total.saturating_add(metadata.len());
            }
        }
        Ok(total)
    }

    pub fn opencode_config_path(scope: &str, cwd: &Path) -> PathBuf {
        match scope {
            "project" => cwd.join("opencode.json"),
            _ => dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("~/.config"))
                .join("opencode")
                .join("opencode.json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{Duration, Utc};

    use super::SessionStore;
    use crate::model::{SCHEMA_VERSION, SessionMetadata, SessionProvider, SessionStatus};

    fn temp_state_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("grepple-storage-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn mk_meta(
        store: &SessionStore,
        session_id: &str,
        status: SessionStatus,
        ended_at: chrono::DateTime<Utc>,
    ) -> SessionMetadata {
        let dir = store.session_dir(session_id);
        let stdout_path = dir.join("stdout.log");
        let stderr_path = dir.join("stderr.log");
        let combined_path = dir.join("combined.log");

        SessionMetadata {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.to_string(),
            session_slug: format!("slug-{session_id}"),
            display_name: format!("name-{session_id}"),
            status,
            provider: SessionProvider::Managed,
            cwd: None,
            command: None,
            pid: None,
            exit_code: Some(0),
            created_at: ended_at - Duration::minutes(5),
            updated_at: ended_at,
            last_activity_at: ended_at,
            stopped_at: Some(ended_at),
            summary_last_line: None,
            stdout_path: stdout_path.to_string_lossy().to_string(),
            stderr_path: stderr_path.to_string_lossy().to_string(),
            combined_path: combined_path.to_string_lossy().to_string(),
            git_context: None,
            provider_ref: None,
        }
    }

    fn create_session(store: &SessionStore, session_id: &str, size_bytes: usize) {
        let dir = store.session_dir(session_id);
        std::fs::create_dir_all(&dir).expect("create session dir");
        let payload = vec![b'x'; size_bytes];
        std::fs::write(dir.join("combined.log"), &payload).expect("write combined.log");
        std::fs::write(dir.join("stdout.log"), &payload).expect("write stdout.log");
        std::fs::write(dir.join("stderr.log"), &payload).expect("write stderr.log");
    }

    #[test]
    fn enforce_storage_limit_removes_oldest_stopped_sessions_first() {
        let state_dir = temp_state_dir();
        let store = SessionStore::new(state_dir.clone(), 7).expect("create store");
        let now = Utc::now();

        create_session(&store, "old", 200);
        let old = mk_meta(
            &store,
            "old",
            SessionStatus::Stopped,
            now - Duration::hours(3),
        );
        store.write_meta(&old).expect("write old");

        create_session(&store, "mid", 200);
        let mid = mk_meta(
            &store,
            "mid",
            SessionStatus::Stopped,
            now - Duration::hours(2),
        );
        store.write_meta(&mid).expect("write mid");

        create_session(&store, "new", 200);
        let new = mk_meta(
            &store,
            "new",
            SessionStatus::Stopped,
            now - Duration::hours(1),
        );
        store.write_meta(&new).expect("write new");

        let old_bytes = store
            .dir_size_bytes(&store.session_dir("old"))
            .expect("old dir bytes");
        let total_bytes = store.dir_size_bytes(&state_dir).expect("total bytes");
        let cap = total_bytes.saturating_sub(old_bytes).saturating_add(1);
        let deleted = store.enforce_storage_limit(cap).expect("enforce limit");
        assert_eq!(deleted, vec!["old".to_string()]);
        assert!(!store.session_dir("old").exists());
        assert!(store.session_dir("mid").exists());
        assert!(store.session_dir("new").exists());

        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn enforce_storage_limit_skips_running_sessions() {
        let state_dir = temp_state_dir();
        let store = SessionStore::new(state_dir.clone(), 7).expect("create store");
        let now = Utc::now();

        create_session(&store, "running", 300);
        let mut running = mk_meta(
            &store,
            "running",
            SessionStatus::Running,
            now - Duration::hours(2),
        );
        running.stopped_at = None;
        store.write_meta(&running).expect("write running");

        create_session(&store, "stopped", 300);
        let stopped = mk_meta(
            &store,
            "stopped",
            SessionStatus::Stopped,
            now - Duration::hours(3),
        );
        store.write_meta(&stopped).expect("write stopped");

        let stopped_bytes = store
            .dir_size_bytes(&store.session_dir("stopped"))
            .expect("stopped dir bytes");
        let total_bytes = store.dir_size_bytes(&state_dir).expect("total bytes");
        let cap = total_bytes.saturating_sub(stopped_bytes).saturating_add(1);
        let deleted = store.enforce_storage_limit(cap).expect("enforce limit");
        assert_eq!(deleted, vec!["stopped".to_string()]);
        assert!(store.session_dir("running").exists());
        assert!(!store.session_dir("stopped").exists());

        let _ = std::fs::remove_dir_all(state_dir);
    }
}
