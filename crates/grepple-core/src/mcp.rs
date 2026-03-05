use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::{
    app::Grepple,
    error::{GreppleError, Result},
    model::{
        AttachSessionRequest, LogReadRequest, LogSearchRequest, StartSessionRequest,
        StopSessionRequest,
    },
};

#[derive(Debug, Clone, Copy)]
enum Framing {
    ContentLength,
    JsonLine,
}

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
            "instructions": "Use Grepple sessions to inspect runtime logs. Always check session status before deep log inspection: prefer the newest running/starting session and ignore older stopped sessions when an active newer one exists. Prefer log_read/log_search over shelling out.",
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
                        "text": "Call session_list first, then confirm the target with session_status. Prefer the newest running/starting session; if one exists, ignore older stopped sessions unless explicitly requested. Then use log_search and log_read incrementally by offsets."
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
            let out = app.log_read(
                LogReadRequest {
                    session_id,
                    stream,
                    offset,
                    max_bytes,
                },
                caller_cwd.as_deref(),
            )?;
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

            let out = app.log_search(
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
            json!(out)
        }
        "log_tail" => {
            let session_id = required_string(&args, "session_id")?;
            let stream = optional_string(&args, "stream").unwrap_or_else(|| "combined".to_string());
            let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(200) as usize;
            json!({"tail": app.log_tail(&session_id, &stream, lines)?})
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
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "session_status",
            "Session status",
            "Get one session status",
            json!({"type": "object", "required": ["session_id"], "properties": {"session_id": {"type": "string"}}}),
        ),
        tool(
            "session_start_command",
            "Start command",
            "Start a managed command session",
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
            json!({"type": "object", "properties": {"target": {"type": "string"}, "name": {"type": "string"}}}),
        ),
        tool(
            "session_stop",
            "Stop session",
            "Stop a managed session process group",
            json!({"type": "object", "required": ["session_id"], "properties": {"session_id": {"type": "string"}, "grace_ms": {"type": "number"}}}),
        ),
        tool(
            "log_read",
            "Read logs",
            "Read logs incrementally by byte offset",
            json!({
                "type": "object",
                "required": ["session_id"],
                "properties": {
                    "session_id": {"type":"string"},
                    "stream": {"type":"string", "enum": ["stdout", "stderr", "combined"]},
                    "offset": {"type":"number"},
                    "max_bytes": {"type":"number"}
                }
            }),
        ),
        tool(
            "log_search",
            "Search logs",
            "Search logs using plain text or regex",
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
                    "max_matches": {"type":"number"}
                }
            }),
        ),
        tool(
            "log_tail",
            "Tail logs",
            "Read the last N lines from a stream",
            json!({"type":"object", "required": ["session_id"], "properties": {"session_id": {"type":"string"}, "stream": {"type":"string"}, "lines": {"type":"number"}}}),
        ),
        tool(
            "log_stats",
            "Log stats",
            "Compute line and error-like counts for a stream",
            json!({"type":"object", "required": ["session_id"], "properties": {"session_id": {"type":"string"}, "stream": {"type":"string"}}}),
        ),
        tool(
            "install_client",
            "Install client",
            "Install grepple into codex, claude, or opencode",
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

fn tool(name: &str, title: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input_schema,
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

#[cfg(test)]
mod tests {
    use super::read_message;
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
}
