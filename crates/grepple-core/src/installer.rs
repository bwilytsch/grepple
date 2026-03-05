use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};

use chrono::Utc;
use serde_json::{Value, json};

use crate::{
    error::{GreppleError, Result},
    model::{InstallerPlan, InstallerResult},
    storage::SessionStore,
};

#[derive(Debug, Clone, Copy)]
pub enum Client {
    Codex,
    Claude,
    Opencode,
}

impl Client {
    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
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

pub fn install(req: InstallRequest) -> Result<InstallerResult> {
    match req.client {
        Client::Codex => install_codex(req),
        Client::Claude => install_claude(req),
        Client::Opencode => install_opencode(req),
    }
}

fn install_codex(req: InstallRequest) -> Result<InstallerResult> {
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

    let mut cmd = Command::new("codex");
    cmd.arg("mcp").arg("add").arg(req.name);
    for (k, v) in req.env {
        cmd.arg("--env").arg(format!("{}={}", k, v));
    }
    cmd.arg("--").arg("grepple").arg("mcp");
    let output = cmd.output()?;

    let success = output.status.success();
    let details = if success {
        "codex mcp add succeeded".to_string()
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

fn install_opencode(req: InstallRequest) -> Result<InstallerResult> {
    let config_path = SessionStore::opencode_config_path(&req.scope, &req.cwd);
    let plan = InstallerPlan {
        client: "opencode".to_string(),
        description: "Patch OpenCode config with grepple MCP entry".to_string(),
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

    let backup = config_path.with_extension(format!("bak-{}", Utc::now().format("%Y%m%d%H%M%S")));
    if config_path.exists() {
        fs::copy(&config_path, &backup)?;
    }

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
