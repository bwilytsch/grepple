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
    model::{SCHEMA_VERSION, SessionMetadata},
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
