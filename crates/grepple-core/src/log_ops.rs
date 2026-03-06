use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use chrono::{DateTime, NaiveDateTime, Utc};
use regex::Regex;

use crate::{
    error::{GreppleError, Result},
    model::{
        LogErrorCountRequest, LogErrorCounts, LogReadRequest, LogReadResult, LogSearchMatch,
        LogSearchRequest, LogSearchResult, LogStats, Warning,
    },
};

pub fn read_logs(path: &Path, req: &LogReadRequest) -> Result<LogReadResult> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();
    let mut warnings = Vec::new();

    let offset = if req.offset > len {
        warnings.push(Warning {
            code: "OFFSET_RESET".to_string(),
            message: "offset exceeded file length; reset to end-of-file".to_string(),
            metadata: BTreeMap::new(),
        });
        len
    } else {
        req.offset
    };

    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0_u8; req.max_bytes.max(1)];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    let next_offset = offset + n as u64;
    let eof = next_offset >= len;

    Ok(LogReadResult {
        chunk: String::from_utf8_lossy(&buf).to_string(),
        next_offset,
        eof,
        warnings,
    })
}

pub fn search_logs(path: &Path, req: &LogSearchRequest) -> Result<LogSearchResult> {
    if req.max_matches == 0 {
        return Err(GreppleError::InvalidArgument(
            "max_matches must be > 0".to_string(),
        ));
    }

    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let len = meta.len();

    let start = req.start_offset.min(len);
    file.seek(SeekFrom::Start(start))?;

    let mut limited = vec![0_u8; req.max_scan_bytes.max(1)];
    let read_n = file.read(&mut limited)?;
    limited.truncate(read_n);
    let text = String::from_utf8_lossy(&limited);

    let regex = if req.regex {
        let pattern = if req.case_sensitive {
            req.query.clone()
        } else {
            format!("(?i){0}", req.query)
        };
        Some(Regex::new(&pattern).map_err(|e| GreppleError::InvalidArgument(e.to_string()))?)
    } else {
        None
    };

    let mut matches = Vec::new();
    let mut running_offset = start;

    for (idx, line) in text.lines().enumerate() {
        let matched = if let Some(re) = &regex {
            re.is_match(line)
        } else if req.case_sensitive {
            line.contains(&req.query)
        } else {
            line.to_ascii_lowercase()
                .contains(&req.query.to_ascii_lowercase())
        };

        if matched {
            matches.push(LogSearchMatch {
                byte_offset: running_offset,
                line_number: idx + 1,
                line: line.to_string(),
            });
            if matches.len() >= req.max_matches {
                break;
            }
        }

        running_offset += line.len() as u64 + 1;
    }

    Ok(LogSearchResult {
        matches,
        scanned_until_offset: (start + limited.len() as u64).min(len),
        warnings: Vec::new(),
    })
}

pub fn tail_lines(path: &Path, count: usize) -> Result<String> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines = std::collections::VecDeque::with_capacity(count.max(1));

    for line in reader.lines() {
        let line = line?;
        if lines.len() == count.max(1) {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    Ok(lines.into_iter().collect::<Vec<_>>().join("\n"))
}

pub fn stats(path: &Path) -> Result<LogStats> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let reader = BufReader::new(file);

    let mut lines = 0_usize;
    let mut error_like = 0_usize;
    for line in reader.lines() {
        let line = line?;
        lines += 1;
        if is_error_like(&line) {
            error_like += 1;
        }
    }

    Ok(LogStats {
        bytes: metadata.len(),
        lines,
        error_like_lines: error_like,
    })
}

pub fn error_counts(path: &Path, req: &LogErrorCountRequest) -> Result<LogErrorCounts> {
    if req.max_matches == 0 {
        return Err(GreppleError::InvalidArgument(
            "max_matches must be > 0".to_string(),
        ));
    }

    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();
    let start = len.saturating_sub(req.max_scan_bytes as u64);
    file.seek(SeekFrom::Start(start))?;

    let mut limited = vec![0_u8; req.max_scan_bytes.max(1)];
    let read_n = file.read(&mut limited)?;
    limited.truncate(read_n);
    let text = String::from_utf8_lossy(&limited);

    let regex = build_match_regex(req)?;
    let mut lines = 0_usize;
    let mut error_like_lines = 0_usize;
    let mut matching_lines = 0_usize;
    let mut timestamped_lines = 0_usize;
    let mut recent_matches = Vec::new();
    let mut warnings = Vec::new();
    let mut running_offset = start;
    let mut newest_timestamp = None;
    let mut parsed_lines = Vec::new();

    for line in text.lines() {
        let timestamp = extract_line_timestamp(line);
        if timestamp.is_some() {
            timestamped_lines += 1;
            newest_timestamp = timestamp.or(newest_timestamp);
        }
        parsed_lines.push((running_offset, line.to_string(), timestamp));
        running_offset += line.len() as u64 + 1;
    }

    let window_end = newest_timestamp;
    let window_start = match (window_end, req.window_ms) {
        (Some(end), Some(window_ms)) if window_ms > 0 => {
            Some(end - chrono::Duration::milliseconds(window_ms))
        }
        _ => None,
    };

    if req.window_ms.is_some() && timestamped_lines == 0 {
        warnings.push(Warning {
            code: "WINDOW_UNAVAILABLE".to_string(),
            message: "time window requested but no parseable timestamps were found in the scanned log window".to_string(),
            metadata: BTreeMap::new(),
        });
    }
    if start > 0 {
        let mut metadata = BTreeMap::new();
        metadata.insert("scanned_from_offset".to_string(), start.to_string());
        metadata.insert("bytes".to_string(), req.max_scan_bytes.to_string());
        warnings.push(Warning {
            code: "PARTIAL_SCAN".to_string(),
            message: "error counts were computed from the recent tail of the log".to_string(),
            metadata,
        });
    }

    for (byte_offset, line, timestamp) in parsed_lines {
        if let Some(window_start) = window_start {
            if let Some(timestamp) = timestamp {
                if timestamp < window_start {
                    continue;
                }
            }
        }

        lines += 1;
        if is_error_like(&line) {
            error_like_lines += 1;
        }
        if matches_query(&line, req, regex.as_ref()) {
            matching_lines += 1;
            if recent_matches.len() < req.max_matches {
                recent_matches.push(LogSearchMatch {
                    byte_offset,
                    line_number: lines,
                    line,
                });
            }
        }
    }

    Ok(LogErrorCounts {
        session_id: String::new(),
        stream: req.stream.clone(),
        bytes: metadata.len(),
        lines,
        error_like_lines,
        matching_lines,
        timestamped_lines,
        scanned_from_offset: start,
        scanned_until_offset: start + limited.len() as u64,
        window_ms: req.window_ms,
        window_start,
        window_end,
        recent_matches,
        warnings,
    })
}

fn build_match_regex(req: &LogErrorCountRequest) -> Result<Option<Regex>> {
    if req.regex {
        let pattern = req
            .query
            .clone()
            .unwrap_or_else(|| error_pattern().to_string());
        let pattern = if req.case_sensitive {
            pattern
        } else {
            format!("(?i){pattern}")
        };
        return Ok(Some(
            Regex::new(&pattern).map_err(|e| GreppleError::InvalidArgument(e.to_string()))?,
        ));
    }

    Ok(None)
}

fn matches_query(line: &str, req: &LogErrorCountRequest, regex: Option<&Regex>) -> bool {
    if let Some(regex) = regex {
        return regex.is_match(line);
    }

    if let Some(query) = &req.query {
        if req.case_sensitive {
            return line.contains(query);
        }
        return line
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase());
    }

    is_error_like(line)
}

fn extract_line_timestamp(line: &str) -> Option<DateTime<Utc>> {
    let regex =
        Regex::new(r"(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?)")
            .expect("valid timestamp regex");
    let token = regex
        .captures(line)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str())?;

    if let Ok(parsed) = DateTime::parse_from_rfc3339(token) {
        return Some(parsed.with_timezone(&Utc));
    }

    if let Ok(parsed) = NaiveDateTime::parse_from_str(token, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(parsed.and_utc());
    }
    if let Ok(parsed) = NaiveDateTime::parse_from_str(token, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(parsed.and_utc());
    }

    None
}

pub fn is_error_like(line: &str) -> bool {
    let low = line.to_ascii_lowercase();
    [
        "error",
        "panic",
        "exception",
        "traceback",
        "stack trace",
        "fatal",
        "failed",
    ]
    .iter()
    .any(|needle| low.contains(needle))
}

fn error_pattern() -> &'static str {
    "error|panic|exception|traceback|stack trace|fatal|failed"
}

#[cfg(test)]
mod tests {
    use super::{extract_line_timestamp, is_error_like};

    #[test]
    fn detects_error_like_lines() {
        assert!(is_error_like("RuntimeError: boom"));
        assert!(is_error_like("fatal: startup failed"));
        assert!(!is_error_like("server listening on :3000"));
    }

    #[test]
    fn parses_rfc3339_timestamps_from_log_lines() {
        let ts = extract_line_timestamp("[2026-03-06T12:34:56Z] ERROR failed to boot")
            .expect("timestamp should parse");
        assert_eq!(ts.to_rfc3339(), "2026-03-06T12:34:56+00:00");
    }
}
