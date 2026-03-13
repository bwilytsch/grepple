use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};

use chrono::Utc;
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{
    error::{GreppleError, Result},
    model::{InstallerPlan, InstallerResult},
    storage::SessionStore,
};

#[derive(Debug, Clone, Copy)]
pub enum Client {
    Codex,
    Claude,
    ClaudeSkill,
    Opencode,
}

impl Client {
    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "claude-skill" => Ok(Self::ClaudeSkill),
            "opencode" => Ok(Self::Opencode),
            _ => Err(GreppleError::InvalidArgument(format!(
                "unsupported client: {input}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::ClaudeSkill => "claude-skill",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub client: Client,
    pub name: String,
    pub env: BTreeMap<String, String>,
    pub dry_run: bool,
    pub force: bool,
    pub scope: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct UninstallRequest {
    pub client: Client,
    pub name: String,
    pub dry_run: bool,
    pub scope: String,
    pub cwd: PathBuf,
}

pub fn uninstall(req: UninstallRequest) -> Result<InstallerResult> {
    match req.client {
        Client::Codex => uninstall_codex(req),
        Client::Claude => uninstall_claude(req),
        Client::ClaudeSkill => uninstall_claude_skill(req),
        Client::Opencode => uninstall_opencode(req),
    }
}

pub fn install(req: InstallRequest) -> Result<InstallerResult> {
    match req.client {
        Client::Codex => install_codex(req),
        Client::Claude => install_claude(req),
        Client::ClaudeSkill => install_claude_skill(req),
        Client::Opencode => install_opencode(req),
    }
}

fn install_codex(req: InstallRequest) -> Result<InstallerResult> {
    const DEFAULT_STARTUP_TIMEOUT_SEC: i64 = 30;

    let mut preview = format!("codex mcp add {}", req.name);
    for (k, v) in &req.env {
        preview.push_str(&format!(" --env {}={}", k, v));
    }
    preview.push_str(" -- grepple mcp");

    let plan = InstallerPlan {
        client: "codex".to_string(),
        description: "Install grepple MCP into codex config".to_string(),
        command_preview: Some(preview.clone()),
        config_path: Some("~/.codex/config.toml".to_string()),
    };

    if req.dry_run {
        return Ok(InstallerResult {
            client: "codex".to_string(),
            dry_run: true,
            plan,
            success: true,
            details: "dry run".to_string(),
        });
    }

    let server_name = req.name.clone();
    let mut cmd = Command::new("codex");
    cmd.arg("mcp").arg("add").arg(&server_name);
    for (k, v) in &req.env {
        cmd.arg("--env").arg(format!("{}={}", k, v));
    }
    cmd.arg("--").arg("grepple").arg("mcp");
    let output = cmd.output()?;

    let success = output.status.success();
    let details = if success {
        match set_codex_startup_timeout(&server_name, DEFAULT_STARTUP_TIMEOUT_SEC) {
            Ok(()) => format!(
                "codex mcp add succeeded (startup_timeout_sec={DEFAULT_STARTUP_TIMEOUT_SEC})"
            ),
            Err(err) => format!(
                "codex mcp add succeeded, but failed to set startup_timeout_sec={DEFAULT_STARTUP_TIMEOUT_SEC}: {err}"
            ),
        }
    } else {
        String::from_utf8_lossy(&output.stderr).to_string()
    };

    Ok(InstallerResult {
        client: "codex".to_string(),
        dry_run: false,
        plan,
        success,
        details,
    })
}

fn set_codex_startup_timeout(server_name: &str, timeout_sec: i64) -> Result<()> {
    let config_path = codex_config_path();
    let content = if config_path.exists() {
        fs::read_to_string(&config_path)?
    } else {
        String::new()
    };

    let mut doc = if content.trim().is_empty() {
        DocumentMut::new()
    } else {
        content
            .parse::<DocumentMut>()
            .map_err(|e| GreppleError::Tool(format!("invalid codex toml config: {e}")))?
    };

    if !doc["mcp_servers"].is_table() {
        doc["mcp_servers"] = Item::Table(Table::new());
    }
    if !doc["mcp_servers"][server_name].is_table() {
        doc["mcp_servers"][server_name] = Item::Table(Table::new());
    }

    doc["mcp_servers"][server_name]["startup_timeout_sec"] = value(timeout_sec);

    let parent = config_path
        .parent()
        .ok_or_else(|| GreppleError::Tool("invalid codex config path".to_string()))?;
    fs::create_dir_all(parent)?;
    let tmp = config_path.with_extension("tmp");
    fs::write(&tmp, doc.to_string())?;
    fs::rename(&tmp, &config_path)?;
    Ok(())
}

fn codex_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".codex")
        .join("config.toml")
}

fn install_claude(req: InstallRequest) -> Result<InstallerResult> {
    let mut preview = format!("claude mcp add --scope {} {}", req.scope, req.name);
    for (k, v) in &req.env {
        preview.push_str(&format!(" --env {}={}", k, v));
    }
    preview.push_str(" -- grepple mcp");

    let plan = InstallerPlan {
        client: "claude".to_string(),
        description: "Install grepple MCP into claude code config".to_string(),
        command_preview: Some(preview.clone()),
        config_path: None,
    };

    if req.dry_run {
        return Ok(InstallerResult {
            client: "claude".to_string(),
            dry_run: true,
            plan,
            success: true,
            details: "dry run".to_string(),
        });
    }

    let mut cmd = Command::new("claude");
    cmd.arg("mcp")
        .arg("add")
        .arg("--scope")
        .arg(req.scope)
        .arg(req.name);
    for (k, v) in req.env {
        cmd.arg("--env").arg(format!("{}={}", k, v));
    }
    cmd.arg("--").arg("grepple").arg("mcp");
    let output = cmd.output()?;

    let success = output.status.success();
    let details = if success {
        "claude mcp add succeeded".to_string()
    } else {
        String::from_utf8_lossy(&output.stderr).to_string()
    };

    Ok(InstallerResult {
        client: "claude".to_string(),
        dry_run: false,
        plan,
        success,
        details,
    })
}

fn install_claude_skill(req: InstallRequest) -> Result<InstallerResult> {
    let skill_dir = match req.scope.as_str() {
        "user" => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".claude")
            .join("commands"),
        "project" => req.cwd.join(".claude").join("commands"),
        _ => {
            return Err(GreppleError::InvalidArgument(format!(
                "unsupported scope '{}' for claude-skill (use 'user' or 'project')",
                req.scope
            )));
        }
    };

    let skill_path = skill_dir.join(format!("{}.md", req.name));

    let plan = InstallerPlan {
        client: "claude-skill".to_string(),
        description: format!(
            "Install grepple CLI skill into Claude Code ({} scope)",
            req.scope
        ),
        command_preview: None,
        config_path: Some(skill_path.display().to_string()),
    };

    if req.dry_run {
        return Ok(InstallerResult {
            client: "claude-skill".to_string(),
            dry_run: true,
            plan,
            success: true,
            details: "dry run".to_string(),
        });
    }

    if skill_path.exists() && !req.force {
        return Err(GreppleError::Tool(format!(
            "skill file already exists at {} (use --force to overwrite)",
            skill_path.display()
        )));
    }

    fs::create_dir_all(&skill_dir)?;
    fs::write(&skill_path, claude_skill_content())?;

    Ok(InstallerResult {
        client: "claude-skill".to_string(),
        dry_run: false,
        plan,
        success: true,
        details: format!("wrote {}", skill_path.display()),
    })
}

fn claude_skill_content() -> &'static str {
    r#"---
description: Check live local logs, dev server output, runtime errors, and stack traces using grepple
---

Use grepple CLI to inspect live local sessions: dev servers, backend/frontend runtimes,
startup failures, and stack traces. Prefer grepple session discovery before code search
when asked about logs, errors, servers, or stack traces.

User's request: $ARGUMENTS

## Workflow

### 1. Discover sessions

List all sessions as JSON:

```bash
grepple sessions --json
```

From the output, find the most relevant session by:

- **Status**: prefer `running` > `starting` > `crashed`/`failed` > `stopped`
- **Git context**: match `git_context.repo_root` or `git_context.worktree_root` to the current repo
- **Labels**: match labels like `dev-server`, `frontend`, `backend`, `next`, `vite`, `flask` to the user's intent
- **Recency**: prefer sessions with the most recent `last_activity_at`

If no sessions exist or none match the current repo, tell the user. Suggest starting one
with `grepple run` if appropriate.

### 2. Read logs

Tail recent output:

```bash
grepple logs <session_id> --tail 50
```

Search for errors:

```bash
grepple logs <session_id> --search "error|Error|ERROR|panic|PANIC|fatal|FATAL|exception|Exception|fail|FAIL|traceback|Traceback" --regex
```

Read a specific byte range (for incremental reading of large logs):

```bash
grepple logs <session_id> --stream combined --offset <byte_offset> --max-bytes 32768
```

Use `next_offset` from the output to continue reading from where you left off.

### 3. Session management

Start a command in the background:

```bash
grepple run --detached --name <name> -- <command>
```

Start a command in the foreground (mirrors output to terminal):

```bash
grepple run --name <name> -- <command>
```

Stop a session:

```bash
grepple stop <session_id>
```

## Response guidelines

- Answer the user's question directly first (Yes/No when applicable)
- For simple factual questions, respond in one sentence with minimum evidence
- Do not mention session IDs or internal details unless the user asks
"#
}

fn install_opencode(req: InstallRequest) -> Result<InstallerResult> {
    let config_path = SessionStore::opencode_config_path(&req.scope, &req.cwd);
    let instructions_name = "grepple-opencode-instructions.md";
    let instructions_path = config_path
        .parent()
        .unwrap_or_else(|| req.cwd.as_path())
        .join(instructions_name);
    let plan = InstallerPlan {
        client: "opencode".to_string(),
        description: "Patch OpenCode config with grepple MCP entry and log-debugging guidance"
            .to_string(),
        command_preview: None,
        config_path: Some(config_path.display().to_string()),
    };

    if req.dry_run {
        return Ok(InstallerResult {
            client: "opencode".to_string(),
            dry_run: true,
            plan,
            success: true,
            details: "dry run".to_string(),
        });
    }

    let parent = config_path
        .parent()
        .ok_or_else(|| GreppleError::Tool("invalid config path".to_string()))?;
    fs::create_dir_all(parent)?;

    let mut root = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        serde_json::from_str::<Value>(&content)
            .map_err(|e| GreppleError::Tool(format!("invalid opencode json config: {e}")))?
    } else {
        json!({})
    };

    if !root.is_object() {
        root = json!({});
    }

    let mcp = root
        .as_object_mut()
        .expect("object")
        .entry("mcp")
        .or_insert_with(|| json!({}));
    if !mcp.is_object() {
        *mcp = json!({});
    }

    let entry = json!({
        "type": "local",
        "command": ["grepple", "mcp"],
        "enabled": true,
        "environment": req.env,
    });

    let mcp_obj = mcp.as_object_mut().expect("object");
    if mcp_obj.contains_key(&req.name) && !req.force {
        return Err(GreppleError::Tool(format!(
            "opencode MCP '{0}' already exists (use --force)",
            req.name
        )));
    }
    mcp_obj.insert(req.name.clone(), entry);

    let instructions = root
        .as_object_mut()
        .expect("object")
        .entry("instructions")
        .or_insert_with(|| json!([]));
    if !instructions.is_array() {
        *instructions = json!([]);
    }
    let instructions_array = instructions.as_array_mut().expect("array");
    if !instructions_array
        .iter()
        .any(|item| item.as_str() == Some(instructions_name))
    {
        instructions_array.push(json!(instructions_name));
    }

    let backup = config_path.with_extension(format!("bak-{}", Utc::now().format("%Y%m%d%H%M%S")));
    if config_path.exists() {
        fs::copy(&config_path, &backup)?;
    }

    fs::write(&instructions_path, opencode_instructions_content())?;

    let tmp = config_path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
    if let Err(e) = fs::rename(&tmp, &config_path) {
        if backup.exists() {
            let _ = fs::copy(&backup, &config_path);
        }
        return Err(e.into());
    }

    Ok(InstallerResult {
        client: "opencode".to_string(),
        dry_run: false,
        plan,
        success: true,
        details: format!("updated {}", config_path.display()),
    })
}

fn uninstall_codex(req: UninstallRequest) -> Result<InstallerResult> {
    let preview = format!("codex mcp remove {}", req.name);
    let config_path = codex_config_path();

    let plan = InstallerPlan {
        client: "codex".to_string(),
        description: "Remove grepple MCP from codex config".to_string(),
        command_preview: Some(preview.clone()),
        config_path: Some(config_path.display().to_string()),
    };

    if req.dry_run {
        return Ok(InstallerResult {
            client: "codex".to_string(),
            dry_run: true,
            plan,
            success: true,
            details: "dry run".to_string(),
        });
    }

    let output = Command::new("codex")
        .arg("mcp")
        .arg("remove")
        .arg(&req.name)
        .output()?;

    let cmd_success = output.status.success();
    let mut details = if cmd_success {
        "codex mcp remove succeeded".to_string()
    } else {
        String::from_utf8_lossy(&output.stderr).to_string()
    };

    if config_path.exists() {
        match remove_codex_startup_timeout(&req.name) {
            Ok(true) => {
                details.push_str("; removed startup_timeout_sec from config.toml");
            }
            Ok(false) => {}
            Err(err) => {
                details.push_str(&format!(
                    "; warning: failed to clean startup_timeout_sec: {err}"
                ));
            }
        }
    }

    Ok(InstallerResult {
        client: "codex".to_string(),
        dry_run: false,
        plan,
        success: cmd_success,
        details,
    })
}

fn remove_codex_startup_timeout(server_name: &str) -> Result<bool> {
    let config_path = codex_config_path();
    if !config_path.exists() {
        return Ok(false);
    }

    let content = fs::read_to_string(&config_path)?;
    let mut doc = content
        .parse::<DocumentMut>()
        .map_err(|e| GreppleError::Tool(format!("invalid codex toml config: {e}")))?;

    if !doc["mcp_servers"].is_table() {
        return Ok(false);
    }
    if !doc["mcp_servers"][server_name].is_table() {
        return Ok(false);
    }

    doc["mcp_servers"]
        .as_table_mut()
        .expect("table")
        .remove(server_name);

    let tmp = config_path.with_extension("tmp");
    fs::write(&tmp, doc.to_string())?;
    fs::rename(&tmp, &config_path)?;
    Ok(true)
}

fn uninstall_claude(req: UninstallRequest) -> Result<InstallerResult> {
    let preview = format!("claude mcp remove --scope {} {}", req.scope, req.name);

    let plan = InstallerPlan {
        client: "claude".to_string(),
        description: "Remove grepple MCP from claude code config".to_string(),
        command_preview: Some(preview.clone()),
        config_path: None,
    };

    if req.dry_run {
        return Ok(InstallerResult {
            client: "claude".to_string(),
            dry_run: true,
            plan,
            success: true,
            details: "dry run".to_string(),
        });
    }

    let output = Command::new("claude")
        .arg("mcp")
        .arg("remove")
        .arg("--scope")
        .arg(&req.scope)
        .arg(&req.name)
        .output()?;

    let success = output.status.success();
    let details = if success {
        "claude mcp remove succeeded".to_string()
    } else {
        String::from_utf8_lossy(&output.stderr).to_string()
    };

    Ok(InstallerResult {
        client: "claude".to_string(),
        dry_run: false,
        plan,
        success,
        details,
    })
}

fn uninstall_claude_skill(req: UninstallRequest) -> Result<InstallerResult> {
    let skill_path = match req.scope.as_str() {
        "user" => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".claude")
            .join("commands")
            .join(format!("{}.md", req.name)),
        "project" => req
            .cwd
            .join(".claude")
            .join("commands")
            .join(format!("{}.md", req.name)),
        _ => {
            return Err(GreppleError::InvalidArgument(format!(
                "unsupported scope '{}' for claude-skill (use 'user' or 'project')",
                req.scope
            )));
        }
    };

    let plan = InstallerPlan {
        client: "claude-skill".to_string(),
        description: format!(
            "Remove grepple CLI skill from Claude Code ({} scope)",
            req.scope
        ),
        command_preview: None,
        config_path: Some(skill_path.display().to_string()),
    };

    if req.dry_run {
        return Ok(InstallerResult {
            client: "claude-skill".to_string(),
            dry_run: true,
            plan,
            success: true,
            details: "dry run".to_string(),
        });
    }

    if !skill_path.exists() {
        return Ok(InstallerResult {
            client: "claude-skill".to_string(),
            dry_run: false,
            plan,
            success: true,
            details: "skill file not found, nothing to remove".to_string(),
        });
    }

    fs::remove_file(&skill_path)?;

    Ok(InstallerResult {
        client: "claude-skill".to_string(),
        dry_run: false,
        plan,
        success: true,
        details: format!("removed {}", skill_path.display()),
    })
}

fn uninstall_opencode(req: UninstallRequest) -> Result<InstallerResult> {
    let config_path = SessionStore::opencode_config_path(&req.scope, &req.cwd);
    let instructions_name = "grepple-opencode-instructions.md";
    let instructions_path = config_path
        .parent()
        .unwrap_or_else(|| req.cwd.as_path())
        .join(instructions_name);

    let plan = InstallerPlan {
        client: "opencode".to_string(),
        description: "Remove grepple MCP entry and instructions from OpenCode config".to_string(),
        command_preview: None,
        config_path: Some(config_path.display().to_string()),
    };

    if req.dry_run {
        return Ok(InstallerResult {
            client: "opencode".to_string(),
            dry_run: true,
            plan,
            success: true,
            details: "dry run".to_string(),
        });
    }

    if !config_path.exists() {
        return Ok(InstallerResult {
            client: "opencode".to_string(),
            dry_run: false,
            plan,
            success: true,
            details: "config not found, nothing to remove".to_string(),
        });
    }

    let content = fs::read_to_string(&config_path)?;
    let mut root = serde_json::from_str::<Value>(&content)
        .map_err(|e| GreppleError::Tool(format!("invalid opencode json config: {e}")))?;

    let mut changed = false;

    if let Some(mcp) = root.get_mut("mcp").and_then(Value::as_object_mut) {
        if mcp.remove(&req.name).is_some() {
            changed = true;
        }
    }

    if let Some(instructions) = root.get_mut("instructions").and_then(Value::as_array_mut) {
        let before = instructions.len();
        instructions.retain(|item| item.as_str() != Some(instructions_name));
        if instructions.len() != before {
            changed = true;
        }
    }

    if changed {
        let backup =
            config_path.with_extension(format!("bak-{}", Utc::now().format("%Y%m%d%H%M%S")));
        fs::copy(&config_path, &backup)?;

        let tmp = config_path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
        fs::rename(&tmp, &config_path)?;
    }

    if instructions_path.exists() {
        fs::remove_file(&instructions_path)?;
    }

    Ok(InstallerResult {
        client: "opencode".to_string(),
        dry_run: false,
        plan,
        success: true,
        details: if changed {
            format!("updated {}", config_path.display())
        } else {
            "grepple entry not found in config, nothing to remove".to_string()
        },
    })
}

fn opencode_instructions_content() -> &'static str {
    r#"# Grepple local log debugging

When a prompt mentions logs, errors, server, dev server, stack trace, backend, frontend, or runtime output:

- check Grepple before broad code search
- start with `pick_best_session` or `current_repo_sessions` using the current repo cwd
- use `session_preset` for `recent_errors`, `startup_failures`, `watch_errors`, or `session_summary`
- use `log_error_counts`, `log_search`, `log_tail`, and `log_read` for deeper inspection

Prefer running sessions in the current repo/worktree over older stopped sessions unless the user explicitly wants historical logs.
"#
}
