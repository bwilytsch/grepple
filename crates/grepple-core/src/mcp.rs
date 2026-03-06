use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
};

use serde_json::{Value, json};

use crate::{
    app::Grepple,
    error::{GreppleError, Result},
    model::{
        AttachSessionRequest, LogReadRequest, LogSearchRequest, StartSessionRequest,
        StopSessionRequest, Warning,
    },
};

#[derive(Debug, Clone, Copy)]
enum Framing {
    ContentLength,
    JsonLine,
}

const MCP_LOG_DEFAULT_MAX_CHARS: usize = 12_000;
const MCP_LOG_MIN_MAX_CHARS: usize = 128;
const MCP_LOG_HARD_MAX_CHARS: usize = 200_000;

pub fn serve_stdio(app: &Grepple) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    loop {
        let Some((msg, framing)) = read_message(&mut reader)? else {
            break;
        };

        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = msg.get("id").cloned();

        if method == "notifications/initialized" {
            continue;
        }

        let response = match handle_request(app, &msg) {
            Ok(result) => {
                if let Some(id) = id {
                    Some(json!({"jsonrpc":"2.0", "id": id, "result": result}))
                } else {
                    None
                }
            }
            Err(err) => {
                if let Some(id) = id {
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": err.to_string(),
                        }
                    }))
                } else {
                    None
                }
            }
        };

        if let Some(response) = response {
            write_message(&mut writer, &response, framing)?;
            writer.flush()?;
        }
    }

    Ok(())
}

fn handle_request(app: &Grepple, msg: &Value) -> Result<Value> {
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| GreppleError::Tool("missing method".to_string()))?;

    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {
                "tools": {"listChanged": false},
                "prompts": {},
            },
            "serverInfo": {
                "name": "grepple",
                "title": "Grepple Terminal Log Observer",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "Use Grepple sessions to inspect runtime logs. Answer the user's question directly first (for yes/no questions, start with Yes or No). Keep the answer concise and include only the minimum evidence needed. Always check session status before deep log inspection: prefer the newest running/starting session and ignore older stopped sessions when an active newer one exists. Prefer log_read/log_search over shelling out.",
        })),
        "tools/list" => Ok(json!({"tools": tool_list()})),
        "prompts/list" => Ok(json!({"prompts": [
            {
                "name": "debug_with_grepple",
                "title": "Debug with Grepple",
                "description": "Guide for session discovery and log inspection"
            }
        ]})),
        "prompts/get" => Ok(json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": {
                        "type": "text",
                        "text": "Call session_list first, then confirm the target with session_status. Prefer the newest running/starting session; if one exists, ignore older stopped sessions unless explicitly requested. Then use log_search and log_read incrementally by offsets. In the final response, answer the user's question in the first sentence (Yes/No when applicable), then add brief evidence."
                    }
                }
            ]
        })),
        "tools/call" => handle_tool_call(app, msg),
        _ => Err(GreppleError::Tool(format!("unsupported method: {method}"))),
    }
}

fn handle_tool_call(app: &Grepple, msg: &Value) -> Result<Value> {
    let params = msg
        .get("params")
        .ok_or_else(|| GreppleError::Tool("missing params".to_string()))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| GreppleError::Tool("missing tool name".to_string()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let caller_cwd = parse_caller_cwd(&args).or_else(|| std::env::current_dir().ok());

    let payload = match name {
        "session_list" => json!({"sessions": app.list_sessions()?}),
        "session_status" => {
            let session_id = required_string(&args, "session_id")?;
            let (meta, warnings) = app.session_status(&session_id, caller_cwd.as_deref())?;
            json!({"session": meta, "warnings": warnings})
        }
        "session_start_command" => {
            let command = required_string(&args, "command")?;
            let name = optional_string(&args, "name");
            let cwd = optional_string(&args, "cwd");
            let env = parse_env_map(&args);
            let started = app.start_session(StartSessionRequest {
                name,
                cwd,
                command,
                env,
                foreground: false,
            })?;
            json!(started)
        }
        "session_attach" => {
            let name = optional_string(&args, "name");
            let target = optional_string(&args, "target");
            let session = app.attach_session(AttachSessionRequest { name, target })?;
            json!(session)
        }
        "session_stop" => {
            let session_id = required_string(&args, "session_id")?;
            let grace_ms = args
                .get("grace_ms")
                .and_then(Value::as_u64)
                .unwrap_or(1_500);
            let stopped = app.stop_session(StopSessionRequest {
                session_id,
                grace_ms,
            })?;
            json!(stopped)
        }
        "log_read" => {
            let session_id = required_string(&args, "session_id")?;
            let stream = optional_string(&args, "stream").unwrap_or_else(|| "combined".to_string());
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(32_768) as usize;
            let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
            let text_max_chars = mcp_text_max_chars(&args);
            let mut out = app.log_read(
                LogReadRequest {
                    session_id,
                    stream,
                    offset,
                    max_bytes,
                },
                caller_cwd.as_deref(),
            )?;
            let shaped = shape_log_text(&out.chunk, raw, text_max_chars, TruncateKeep::Start);
            out.chunk = shaped.text;
            if shaped.cleaned {
                out.warnings.push(mcp_sanitized_warning(
                    "chunk",
                    shaped.original_chars,
                    shaped.returned_chars,
                ));
            }
            if shaped.truncated {
                out.warnings.push(mcp_truncated_warning(
                    "chunk",
                    shaped.original_chars,
                    shaped.returned_chars,
                    text_max_chars,
                ));
            }
            json!(out)
        }
        "log_search" => {
            let session_id = required_string(&args, "session_id")?;
            let stream = optional_string(&args, "stream").unwrap_or_else(|| "combined".to_string());
            let query = required_string(&args, "query")?;
            let regex = args.get("regex").and_then(Value::as_bool).unwrap_or(false);
            let case_sensitive = args
                .get("case_sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
            let text_max_chars = mcp_text_max_chars(&args);
            let start_offset = args
                .get("start_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let max_scan_bytes = args
                .get("max_scan_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(1024 * 1024) as usize;
            let max_matches = args
                .get("max_matches")
                .and_then(Value::as_u64)
                .unwrap_or(100) as usize;

            let mut out = app.log_search(
                LogSearchRequest {
                    session_id,
                    stream,
                    query,
                    regex,
                    case_sensitive,
                    start_offset,
                    max_scan_bytes,
                    max_matches,
                },
                caller_cwd.as_deref(),
            )?;
            let mut sanitized_count = 0usize;
            let mut truncated_count = 0usize;
            for m in &mut out.matches {
                let shaped = shape_log_text(&m.line, raw, text_max_chars, TruncateKeep::Start);
                if shaped.cleaned {
                    sanitized_count += 1;
                }
                if shaped.truncated {
                    truncated_count += 1;
                }
                m.line = shaped.text;
            }
            if sanitized_count > 0 {
                out.warnings.push(mcp_count_warning(
                    "OUTPUT_SANITIZED",
                    "search match lines sanitized for MCP output",
                    "sanitized_lines",
                    sanitized_count,
                ));
            }
            if truncated_count > 0 {
                out.warnings.push(mcp_count_warning(
                    "OUTPUT_TRUNCATED",
                    "search match lines truncated for MCP output",
                    "truncated_lines",
                    truncated_count,
                ));
            }
            json!(out)
        }
        "log_tail" => {
            let session_id = required_string(&args, "session_id")?;
            let stream = optional_string(&args, "stream").unwrap_or_else(|| "combined".to_string());
            let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(200) as usize;
            let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
            let text_max_chars = mcp_text_max_chars(&args);
            let tail = app.log_tail(&session_id, &stream, lines)?;
            let shaped = shape_log_text(&tail, raw, text_max_chars, TruncateKeep::End);
            json!({
                "tail": shaped.text,
                "tail_sanitized": shaped.cleaned,
                "tail_truncated": shaped.truncated,
                "tail_original_chars": shaped.original_chars,
                "tail_returned_chars": shaped.returned_chars
            })
        }
        "log_stats" => {
            let session_id = required_string(&args, "session_id")?;
            let stream = optional_string(&args, "stream").unwrap_or_else(|| "combined".to_string());
            json!(app.log_stats(&session_id, &stream)?)
        }
        "install_client" => {
            let client = required_string(&args, "client")?;
            let name = optional_string(&args, "name").unwrap_or_else(|| "grepple".to_string());
            let scope = optional_string(&args, "scope").unwrap_or_else(|| "user".to_string());
            let dry_run = args
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let env = parse_env_map(&args);
            json!(app.install_client(&client, &name, &env, dry_run, force, &scope)?)
        }
        other => {
            return Err(GreppleError::Tool(format!("unsupported tool: {other}")));
        }
    };

    let structured = payload
        .as_object()
        .map(|_| payload.clone())
        .unwrap_or_else(|| json!({ "result": payload }));

    Ok(json!({
        "content":[{"type":"text","text": serde_json::to_string_pretty(&structured)?}],
        "structuredContent": structured
    }))
}

fn required_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| GreppleError::InvalidArgument(format!("missing '{key}'")))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

fn parse_env_map(value: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(map) = value.get("env").and_then(Value::as_object) {
        for (k, v) in map {
            if let Some(vs) = v.as_str() {
                out.push((k.clone(), vs.to_string()));
            }
        }
    }
    out
}

fn tool_list() -> Vec<Value> {
    vec![
        tool(
            "session_list",
            "List sessions",
            "List grepple sessions with status and git context",
            tool_hints(true, false, true),
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "session_status",
            "Session status",
            "Get one session status",
            tool_hints(true, false, true),
            json!({"type": "object", "required": ["session_id"], "properties": {"session_id": {"type": "string"}}}),
        ),
        tool(
            "session_start_command",
            "Start command",
            "Start a managed command session",
            tool_hints(false, true, false),
            json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {"type": "string"},
                    "name": {"type": "string"},
                    "cwd": {"type": "string"},
                    "env": {"type": "object", "additionalProperties": {"type":"string"}}
                }
            }),
        ),
        tool(
            "session_attach",
            "Attach tmux",
            "Attach to tmux pane and create session snapshot",
            tool_hints(false, false, false),
            json!({"type": "object", "properties": {"target": {"type": "string"}, "name": {"type": "string"}}}),
        ),
        tool(
            "session_stop",
            "Stop session",
            "Stop a managed session process group",
            tool_hints(false, true, true),
            json!({"type": "object", "required": ["session_id"], "properties": {"session_id": {"type": "string"}, "grace_ms": {"type": "number"}}}),
        ),
        tool(
            "log_read",
            "Read logs",
            "Read logs incrementally by byte offset",
            tool_hints(true, false, true),
            json!({
                "type": "object",
                "required": ["session_id"],
                "properties": {
                    "session_id": {"type":"string"},
                    "stream": {"type":"string", "enum": ["stdout", "stderr", "combined"]},
                    "offset": {"type":"number"},
                    "max_bytes": {"type":"number"},
                    "max_chars": {"type":"number", "description": "max characters returned in chunk (default 12000)"},
                    "raw": {"type":"boolean", "description": "if true, disable MCP sanitation/truncation"}
                }
            }),
        ),
        tool(
            "log_search",
            "Search logs",
            "Search logs using plain text or regex",
            tool_hints(true, false, true),
            json!({
                "type": "object",
                "required": ["session_id", "query"],
                "properties": {
                    "session_id": {"type":"string"},
                    "stream": {"type":"string", "enum": ["stdout", "stderr", "combined"]},
                    "query": {"type":"string"},
                    "regex": {"type":"boolean"},
                    "case_sensitive": {"type":"boolean"},
                    "start_offset": {"type":"number"},
                    "max_scan_bytes": {"type":"number"},
                    "max_matches": {"type":"number"},
                    "max_chars": {"type":"number", "description": "max characters returned per match line (default 12000)"},
                    "raw": {"type":"boolean", "description": "if true, disable MCP sanitation/truncation"}
                }
            }),
        ),
        tool(
            "log_tail",
            "Tail logs",
            "Read the last N lines from a stream",
            tool_hints(true, false, true),
            json!({
                "type":"object",
                "required": ["session_id"],
                "properties": {
                    "session_id": {"type":"string"},
                    "stream": {"type":"string"},
                    "lines": {"type":"number"},
                    "max_chars": {"type":"number", "description": "max characters returned in tail (default 12000)"},
                    "raw": {"type":"boolean", "description": "if true, disable MCP sanitation/truncation"}
                }
            }),
        ),
        tool(
            "log_stats",
            "Log stats",
            "Compute line and error-like counts for a stream",
            tool_hints(true, false, true),
            json!({"type":"object", "required": ["session_id"], "properties": {"session_id": {"type":"string"}, "stream": {"type":"string"}}}),
        ),
        tool(
            "install_client",
            "Install client",
            "Install grepple into codex, claude, or opencode",
            tool_hints(false, true, false),
            json!({
                "type": "object",
                "required": ["client"],
                "properties": {
                    "client": {"type":"string", "enum": ["codex", "claude", "opencode"]},
                    "name": {"type":"string"},
                    "scope": {"type":"string"},
                    "dry_run": {"type":"boolean"},
                    "force": {"type":"boolean"},
                    "env": {"type":"object", "additionalProperties": {"type":"string"}}
                }
            }),
        ),
    ]
}

fn tool(
    name: &str,
    title: &str,
    description: &str,
    annotations: Value,
    input_schema: Value,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "annotations": annotations,
        "inputSchema": input_schema,
    })
}

fn tool_hints(read_only: bool, destructive: bool, idempotent: bool) -> Value {
    json!({
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent
    })
}

fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<(Value, Framing)>> {
    let mut first_line = String::new();
    let mut n = reader.read_line(&mut first_line)?;
    if n == 0 {
        return Ok(None);
    }

    // Support JSONL-style MCP transports that send one JSON-RPC payload per line.
    while first_line.trim().is_empty() {
        first_line.clear();
        n = reader.read_line(&mut first_line)?;
        if n == 0 {
            return Ok(None);
        }
    }

    let trimmed = first_line.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Ok(Some((
            serde_json::from_str::<Value>(trimmed)?,
            Framing::JsonLine,
        )));
    }

    let mut content_length: Option<usize> = parse_content_length_header(&first_line)?;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }

        if let Some(parsed) = parse_content_length_header(&line)? {
            content_length = Some(parsed);
        }
    }

    let len = content_length
        .ok_or_else(|| GreppleError::Tool("missing Content-Length header".to_string()))?;
    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body)?;
    let value = serde_json::from_slice::<Value>(&body)?;
    Ok(Some((value, Framing::ContentLength)))
}

fn parse_content_length_header(line: &str) -> Result<Option<usize>> {
    let line = line.trim_end_matches(&['\r', '\n'][..]);
    let Some((name, value)) = line.split_once(':') else {
        return Ok(None);
    };
    if !name.trim().eq_ignore_ascii_case("content-length") {
        return Ok(None);
    }
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|e| GreppleError::Tool(format!("invalid content-length: {e}")))?;
    Ok(Some(parsed))
}

fn write_message<W: Write>(writer: &mut W, value: &Value, framing: Framing) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    match framing {
        Framing::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
            writer.write_all(&body)?;
        }
        Framing::JsonLine => {
            writer.write_all(&body)?;
            writer.write_all(b"\n")?;
        }
    }
    Ok(())
}

pub fn run_from_default_config() -> Result<()> {
    let config = crate::app::GreppleConfig::default();
    let app = crate::app::Grepple::new_for_mcp(config)?;
    serve_stdio(&app)
}

pub fn parse_caller_cwd(args: &Value) -> Option<PathBuf> {
    args.get("caller_cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

fn mcp_text_max_chars(args: &Value) -> usize {
    let raw = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .unwrap_or(MCP_LOG_DEFAULT_MAX_CHARS as u64);
    raw.clamp(MCP_LOG_MIN_MAX_CHARS as u64, MCP_LOG_HARD_MAX_CHARS as u64) as usize
}

fn mcp_sanitized_warning(field: &str, original_chars: usize, returned_chars: usize) -> Warning {
    let mut metadata = BTreeMap::new();
    metadata.insert("field".to_string(), field.to_string());
    metadata.insert("original_chars".to_string(), original_chars.to_string());
    metadata.insert("returned_chars".to_string(), returned_chars.to_string());
    Warning {
        code: "OUTPUT_SANITIZED".to_string(),
        message: "terminal control sequences removed for MCP output".to_string(),
        metadata,
    }
}

fn mcp_truncated_warning(
    field: &str,
    original_chars: usize,
    returned_chars: usize,
    max_chars: usize,
) -> Warning {
    let mut metadata = BTreeMap::new();
    metadata.insert("field".to_string(), field.to_string());
    metadata.insert("original_chars".to_string(), original_chars.to_string());
    metadata.insert("returned_chars".to_string(), returned_chars.to_string());
    metadata.insert("max_chars".to_string(), max_chars.to_string());
    Warning {
        code: "OUTPUT_TRUNCATED".to_string(),
        message: "log output truncated for MCP response size".to_string(),
        metadata,
    }
}

fn mcp_count_warning(code: &str, message: &str, key: &str, count: usize) -> Warning {
    let mut metadata = BTreeMap::new();
    metadata.insert(key.to_string(), count.to_string());
    Warning {
        code: code.to_string(),
        message: message.to_string(),
        metadata,
    }
}

#[derive(Clone, Copy)]
enum TruncateKeep {
    Start,
    End,
}

struct ShapedText {
    text: String,
    original_chars: usize,
    returned_chars: usize,
    cleaned: bool,
    truncated: bool,
}

fn shape_log_text(input: &str, raw: bool, max_chars: usize, keep: TruncateKeep) -> ShapedText {
    if raw {
        let chars = input.chars().count();
        return ShapedText {
            text: input.to_string(),
            original_chars: chars,
            returned_chars: chars,
            cleaned: false,
            truncated: false,
        };
    }

    let original_chars = input.chars().count();
    let cleaned_text = strip_terminal_control(input);
    let cleaned = cleaned_text != input;
    let (text, truncated) = truncate_chars(&cleaned_text, max_chars, keep);
    let returned_chars = text.chars().count();

    ShapedText {
        text,
        original_chars,
        returned_chars,
        cleaned,
        truncated,
    }
}

fn truncate_chars(input: &str, max_chars: usize, keep: TruncateKeep) -> (String, bool) {
    let total_chars = input.chars().count();
    if total_chars <= max_chars {
        return (input.to_string(), false);
    }

    match keep {
        TruncateKeep::Start => {
            let end_byte = input
                .char_indices()
                .nth(max_chars)
                .map(|(idx, _)| idx)
                .unwrap_or(input.len());
            (input[..end_byte].to_string(), true)
        }
        TruncateKeep::End => {
            let skip = total_chars - max_chars;
            let start_byte = input
                .char_indices()
                .nth(skip)
                .map(|(idx, _)| idx)
                .unwrap_or(0);
            (input[start_byte..].to_string(), true)
        }
    }
}

fn strip_terminal_control(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
                    i += 1;
                    while i < bytes.len() {
                        let c = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                }
                b']' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    i += 1;
                }
            }
            continue;
        }

        if b == b'\r' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 1;
                continue;
            }
            out.push(b'\n');
            i += 1;
            continue;
        }

        if b < 0x20 && b != b'\n' && b != b'\t' {
            i += 1;
            continue;
        }

        out.push(b);
        i += 1;
    }

    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::{TruncateKeep, read_message, shape_log_text, strip_terminal_control, tool_list};
    use std::io::BufReader;

    #[test]
    fn read_message_accepts_content_length_framing() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let input = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = BufReader::new(input.as_bytes());

        let parsed = read_message(&mut reader)
            .expect("read should succeed")
            .expect("message should exist");

        let (msg, framing) = parsed;
        assert!(matches!(framing, super::Framing::ContentLength));
        assert_eq!(msg["method"], "initialize");
        assert_eq!(msg["id"], 1);
    }

    #[test]
    fn read_message_accepts_jsonl_framing() {
        let input = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}
"#;
        let mut reader = BufReader::new(input.as_bytes());

        let parsed = read_message(&mut reader)
            .expect("read should succeed")
            .expect("message should exist");

        let (msg, framing) = parsed;
        assert!(matches!(framing, super::Framing::JsonLine));
        assert_eq!(msg["method"], "initialize");
        assert_eq!(msg["id"], 0);
    }

    #[test]
    fn strip_terminal_control_removes_ansi_and_carriage_control() {
        let raw = "\u{1b}[34mblue\u{1b}[0m\r\u{1b}[2Knext\nok";
        let cleaned = strip_terminal_control(raw);
        assert!(!cleaned.contains('\u{1b}'));
        assert!(!cleaned.contains('\r'));
        assert!(cleaned.contains("blue"));
        assert!(cleaned.contains("next"));
        assert!(cleaned.contains("ok"));
    }

    #[test]
    fn shape_log_text_truncates_from_end_for_tail() {
        let shaped = shape_log_text("1234567890", false, 4, TruncateKeep::End);
        assert_eq!(shaped.text, "7890");
        assert!(shaped.truncated);
    }

    #[test]
    fn tool_list_contains_safety_annotations() {
        let tools = tool_list();

        let log_search = tools
            .iter()
            .find(|t| t["name"] == "log_search")
            .expect("log_search tool");
        assert_eq!(log_search["annotations"]["readOnlyHint"], true);
        assert_eq!(log_search["annotations"]["destructiveHint"], false);
        assert_eq!(log_search["annotations"]["idempotentHint"], true);

        let session_start = tools
            .iter()
            .find(|t| t["name"] == "session_start_command")
            .expect("session_start_command tool");
        assert_eq!(session_start["annotations"]["readOnlyHint"], false);
        assert_eq!(session_start["annotations"]["destructiveHint"], true);
    }
}
