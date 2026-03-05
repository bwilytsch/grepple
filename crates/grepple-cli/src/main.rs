use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand};
use grepple_core::{
    Grepple, GreppleConfig, mcp,
    model::{
        AttachSessionRequest, LogReadRequest, LogSearchRequest, StartSessionRequest,
        StopSessionRequest,
    },
    runtime::list_tmux_panes,
};

#[derive(Parser, Debug)]
#[command(
    name = "grepple",
    version,
    about = "Session-centric terminal log observer for agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Run {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    Attach {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
    Sessions {
        #[arg(long)]
        json: bool,
    },
    Logs {
        session_id: String,
        #[arg(long, default_value = "combined")]
        stream: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 32768)]
        max_bytes: usize,
        #[arg(long)]
        tail: Option<usize>,
        #[arg(long)]
        search: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        regex: bool,
        #[arg(long, action = ArgAction::SetTrue)]
        case_sensitive: bool,
    },
    Stop {
        session_id: String,
        #[arg(long, default_value_t = 1500)]
        grace_ms: u64,
    },
    Add {
        client: String,
        #[arg(long, default_value = "grepple")]
        name: String,
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        dry_run: bool,
        #[arg(long, action = ArgAction::SetTrue)]
        force: bool,
        #[arg(long, default_value = "user")]
        scope: String,
    },
    Install {
        client: String,
        #[arg(long, default_value = "grepple")]
        name: String,
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        dry_run: bool,
        #[arg(long, action = ArgAction::SetTrue)]
        force: bool,
        #[arg(long, default_value = "user")]
        scope: String,
    },
    Mcp,
}

fn main() -> ExitCode {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let app = Grepple::new(GreppleConfig::default())?;

    match cli.command {
        Commands::Run {
            name,
            cwd,
            env,
            command,
        } => {
            let command = shell_join(&command);
            let env = parse_env_pairs(env)?;
            let meta = app.start_session(StartSessionRequest {
                name,
                cwd,
                command,
                env,
            })?;
            println!(
                "started {} ({}) pid={} status={:?}",
                meta.session_id,
                meta.display_name,
                meta.pid.unwrap_or_default(),
                meta.status
            );
        }
        Commands::Attach { name, target } => {
            let target = resolve_attach_target(target)?;
            let meta = app.attach_session(AttachSessionRequest { name, target })?;
            println!(
                "attached {} ({}) source={}",
                meta.session_id,
                meta.display_name,
                meta.provider_ref.unwrap_or_default()
            );
        }
        Commands::Sessions { json } => {
            let sessions = app.list_sessions()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for session in sessions {
                    println!(
                        "{}  {:<10}  {:<34}  {}",
                        session.session_id,
                        format!("{:?}", session.status).to_lowercase(),
                        session.display_name,
                        session.summary_last_line.unwrap_or_default()
                    );
                }
            }
        }
        Commands::Logs {
            session_id,
            stream,
            offset,
            max_bytes,
            tail,
            search,
            regex,
            case_sensitive,
        } => {
            if let Some(lines) = tail {
                let out = app.log_tail(&session_id, &stream, lines)?;
                println!("{out}");
                return Ok(());
            }

            if let Some(query) = search {
                let out = app.log_search(
                    LogSearchRequest {
                        session_id,
                        stream,
                        query,
                        regex,
                        case_sensitive,
                        start_offset: offset,
                        max_scan_bytes: max_bytes,
                        max_matches: 200,
                    },
                    std::env::current_dir().ok().as_deref(),
                )?;
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }

            let out = app.log_read(
                LogReadRequest {
                    session_id,
                    stream,
                    offset,
                    max_bytes,
                },
                std::env::current_dir().ok().as_deref(),
            )?;
            print!("{}", out.chunk);
            eprintln!("\nnext_offset={} eof={}", out.next_offset, out.eof);
        }
        Commands::Stop {
            session_id,
            grace_ms,
        } => {
            let meta = app.stop_session(StopSessionRequest {
                session_id,
                grace_ms,
            })?;
            println!("stopped {} ({})", meta.session_id, meta.display_name);
        }
        Commands::Add {
            client,
            name,
            env,
            dry_run,
            force,
            scope,
        }
        | Commands::Install {
            client,
            name,
            env,
            dry_run,
            force,
            scope,
        } => {
            let env = parse_env_pairs(env)?;
            let out = app.install_client(&client, &name, &env, dry_run, force, &scope)?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            if !out.success {
                anyhow::bail!("installer failed: {}", out.details);
            }
        }
        Commands::Mcp => {
            mcp::serve_stdio(&app).context("running MCP server")?;
        }
    }

    Ok(())
}

fn parse_env_pairs(values: Vec<String>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for item in values {
        let (k, v) = item
            .split_once('=')
            .with_context(|| format!("invalid env format '{item}', expected KEY=VALUE"))?;
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn resolve_attach_target(target: Option<String>) -> Result<Option<String>> {
    if target.is_some() {
        return Ok(target);
    }

    let panes = list_tmux_panes()?;
    if panes.is_empty() {
        anyhow::bail!("no tmux panes found");
    }
    if panes.len() == 1 {
        return Ok(Some(panes[0].pane_id.clone()));
    }

    if !io::stdin().is_terminal() {
        let options = panes
            .iter()
            .map(|p| format!("{} ({})", p.label, p.pane_id))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "AMBIGUOUS_TARGET: multiple tmux panes found; run with --target. candidates: {options}"
        );
    }

    eprintln!("Select a tmux pane:");
    for (idx, pane) in panes.iter().enumerate() {
        eprintln!(
            "  {}. {} ({}) [{}]",
            idx + 1,
            pane.label,
            pane.pane_id,
            pane.command
        );
    }
    eprint!("Enter number: ");
    io::stderr().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let choice: usize = line.trim().parse().with_context(|| "invalid selection")?;
    if !(1..=panes.len()).contains(&choice) {
        anyhow::bail!("selection out of range");
    }

    Ok(Some(panes[choice - 1].pane_id.clone()))
}
