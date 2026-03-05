use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use regex::Regex;

use crate::{
    error::{GreppleError, Result},
    model::{
        LogReadRequest, LogReadResult, LogSearchMatch, LogSearchRequest, LogSearchResult, LogStats,
        Warning,
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
        let low = line.to_ascii_lowercase();
        if low.contains("error") || low.contains("panic") || low.contains("exception") {
            error_like += 1;
        }
    }

    Ok(LogStats {
        bytes: metadata.len(),
        lines,
        error_like_lines: error_like,
    })
}
