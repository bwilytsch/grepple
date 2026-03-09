use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use grepple_core::{
    Grepple, GreppleConfig, mcp,
    model::{
        AttachSessionRequest, InstallerResult, LogReadRequest, LogSearchRequest,
        StartSessionRequest, StartShellSessionRequest, StopSessionRequest,
    },
    runtime::list_tmux_panes,
};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    widgets::{Block, Row, Table, Widget},
};

mod tui;

#[derive(Parser, Debug)]
#[command(
    name = "grepple",
    version,
    about = "Session-centric terminal log observer for agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
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
        #[arg(long, action = ArgAction::SetTrue)]
        detached: bool,
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
        #[command(subcommand)]
        command: Option<SessionsCommands>,
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
        #[arg(long, action = ArgAction::SetTrue)]
        json: bool,
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
        #[arg(long, action = ArgAction::SetTrue)]
        json: bool,
    },
    Shell {
        #[command(subcommand)]
        command: ShellCommands,
    },
    Mcp,
}

#[derive(Subcommand, Debug)]
enum SessionsCommands {
    Clear {
        #[arg(long, action = ArgAction::SetTrue)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ShellCommands {
    Init {
        #[arg(value_enum)]
        shell: ShellFlavor,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum ShellFlavor {
    Zsh,
    Fish,
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

    match &cli.command {
        Some(Commands::Mcp) => {
            let app = Grepple::new_for_mcp(GreppleConfig::default())?;
            mcp::serve_stdio(&app).context("running MCP server")?;
            return Ok(());
        }
        Some(Commands::Shell { command }) => {
            print_shell_command(command);
            return Ok(());
        }
        _ => {}
    }

    let app = Grepple::new(GreppleConfig::default())?;

    match cli.command {
        None => {
            let meta = app.start_shell_session(StartShellSessionRequest {
                name: None,
                cwd: None,
            })?;
            if meta.exit_code.unwrap_or_default() != 0 {
                return Err(anyhow::anyhow!(
                    "shell exited with status {}",
                    meta.exit_code.unwrap_or_default()
                ));
            }
        }
        Some(Commands::Run {
            name,
            cwd,
            env,
            detached,
            command,
        }) => {
            let command = shell_join(&command);
            let env = parse_env_pairs(env)?;
            let meta = app.start_session(StartSessionRequest {
                name,
                cwd,
                command,
                env,
                foreground: !detached,
            })?;
            if detached {
                println!(
                    "started {} ({}) pid={} status={:?}",
                    meta.session_id,
                    meta.display_name,
                    meta.pid.unwrap_or_default(),
                    meta.status
                );
            } else if meta.exit_code.unwrap_or_default() != 0 {
                return Err(anyhow::anyhow!(
                    "command exited with status {}",
                    meta.exit_code.unwrap_or_default()
                ));
            }
        }
        Some(Commands::Attach { name, target }) => {
            let target = resolve_attach_target(target)?;
            let meta = app.attach_session(AttachSessionRequest { name, target })?;
            println!(
                "attached {} ({}) source={}",
                meta.session_id,
                meta.display_name,
                meta.provider_ref.unwrap_or_default()
            );
        }
        Some(Commands::Sessions { command, json }) => match command {
            Some(SessionsCommands::Clear { yes }) => {
                if json {
                    anyhow::bail!("--json is not supported for 'sessions clear'");
                }
                confirm_sessions_clear(yes)?;
                let deleted = app.clear_sessions()?;
                if deleted.is_empty() {
                    println!("No sessions to clear.");
                } else {
                    println!("Cleared {} session(s).", deleted.len());
                }
            }
            None => {
                let sessions = app.list_sessions()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&sessions)?);
                } else {
                    print_sessions_table(&sessions);
                }
            }
        },
        Some(Commands::Logs {
            session_id,
            stream,
            offset,
            max_bytes,
            tail,
            search,
            regex,
            case_sensitive,
        }) => {
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
        Some(Commands::Stop {
            session_id,
            grace_ms,
        }) => {
            let meta = app.stop_session(StopSessionRequest {
                session_id,
                grace_ms,
            })?;
            println!("stopped {} ({})", meta.session_id, meta.display_name);
        }
        Some(Commands::Add {
            client,
            name,
            env,
            dry_run,
            force,
            scope,
            json,
        })
        | Some(Commands::Install {
            client,
            name,
            env,
            dry_run,
            force,
            scope,
            json,
        }) => {
            let env = parse_env_pairs(env)?;
            let use_line_ui = !dry_run && !json && std::io::stdout().is_terminal();
            let out = if use_line_ui {
                tui::run_install_tui(tui::InstallUiRequest {
                    client: client.clone(),
                    name: name.clone(),
                    env: env.clone(),
                    force,
                    scope: scope.clone(),
                })?
            } else {
                app.install_client(&client, &name, &env, dry_run, force, &scope)?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if !use_line_ui {
                print_install_summary(&out, &name);
            }
            if !out.success {
                anyhow::bail!("installer failed: {}", out.details);
            }
        }
        Some(Commands::Shell { .. }) => unreachable!("handled before app initialization"),
        Some(Commands::Mcp) => unreachable!("handled before app initialization"),
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

fn print_install_summary(out: &InstallerResult, configured_name: &str) {
    if out.success {
        if out.dry_run {
            println!("Dry run for {} complete.", out.client);
        } else {
            println!(
                "Installed Grepple MCP for {} as '{}'.",
                out.client, configured_name
            );
        }
    } else {
        println!("Grepple MCP install failed for {}.", out.client);
    }

    if let Some(preview) = &out.plan.command_preview {
        println!("Command: {}", preview);
    }
    if let Some(path) = &out.plan.config_path {
        println!("Config: {}", path);
    }
    println!("Details: {}", out.details);
}

fn confirm_sessions_clear(yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }

    if !io::stdin().is_terminal() {
        anyhow::bail!("sessions clear requires --yes in non-interactive mode");
    }

    eprint!("This will remove all local grepple sessions. Continue? [y/N]: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let accepted = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !accepted {
        anyhow::bail!("aborted");
    }
    Ok(())
}

fn print_shell_command(command: &ShellCommands) {
    match command {
        ShellCommands::Init { shell } => print!("{}", shell_init_snippet(*shell)),
    }
}

fn shell_init_snippet(shell: ShellFlavor) -> &'static str {
    match shell {
        ShellFlavor::Zsh => {
            r#"grepple() {
    if [[ "${GREPPLE_SHELL:-}" = "1" ]]; then
        case "$1" in
            exit|quit)
                builtin exit
                ;;
        esac
    fi
    command grepple "$@"
}
alias g="grepple"
gr() { command grepple run -- "$@"; }
"#
        }
        ShellFlavor::Fish => {
            r#"function grepple
    if test "$GREPPLE_SHELL" = "1"
        switch "$argv[1]"
            case exit quit
                builtin exit
        end
    end
    command grepple $argv
end
alias g grepple
function gr
    command grepple run -- $argv
end
"#
        }
    }
}

fn print_sessions_table(sessions: &[grepple_core::model::SessionMetadata]) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }

    print!("{}", render_sessions_table(sessions));
}

fn render_sessions_table(sessions: &[grepple_core::model::SessionMetadata]) -> String {
    const ID_W: u16 = 26;
    const STATUS_W: u16 = 10;
    const PROVIDER_W: u16 = 12;
    const NAME_W: u16 = 28;
    const BRANCH_W: u16 = 18;
    const LAST_W: u16 = 54;
    const COLUMN_SPACING: u16 = 1;
    const COLUMNS: u16 = 6;
    const BORDERS_W: u16 = 2;
    const HEADER_AND_BORDERS_H: u16 = 3;

    let max_rows = usize::from(u16::MAX.saturating_sub(HEADER_AND_BORDERS_H));
    let visible_rows = sessions.len().min(max_rows);

    let rows = sessions.iter().take(visible_rows).map(|session| {
        let status = format!("{:?}", session.status).to_lowercase();
        let provider = format!("{:?}", session.provider).to_lowercase();
        let branch = session
            .git_context
            .as_ref()
            .map(|g| g.branch.as_str())
            .unwrap_or("-");
        let last = session.summary_last_line.as_deref().unwrap_or("-");

        Row::new(vec![
            truncate(&session.session_id, usize::from(ID_W)),
            truncate(&status, usize::from(STATUS_W)),
            truncate(&provider, usize::from(PROVIDER_W)),
            truncate(&session.display_name, usize::from(NAME_W)),
            truncate(branch, usize::from(BRANCH_W)),
            truncate(last, usize::from(LAST_W)),
        ])
    });

    let min_width = ID_W
        .saturating_add(STATUS_W)
        .saturating_add(PROVIDER_W)
        .saturating_add(NAME_W)
        .saturating_add(BRANCH_W)
        .saturating_add(LAST_W)
        .saturating_add(COLUMN_SPACING.saturating_mul(COLUMNS.saturating_sub(1)))
        .saturating_add(BORDERS_W);

    let width = terminal_table_width().max(min_width);
    let height = u16::try_from(visible_rows)
        .unwrap_or(u16::MAX.saturating_sub(HEADER_AND_BORDERS_H))
        .saturating_add(HEADER_AND_BORDERS_H);

    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);

    let table = Table::new(
        rows,
        [
            Constraint::Length(ID_W),
            Constraint::Length(STATUS_W),
            Constraint::Length(PROVIDER_W),
            Constraint::Length(NAME_W),
            Constraint::Length(BRANCH_W),
            Constraint::Length(LAST_W),
        ],
    )
    .column_spacing(COLUMN_SPACING)
    .header(
        Row::new([
            "SESSION ID",
            "STATUS",
            "PROVIDER",
            "NAME",
            "BRANCH",
            "LAST LINE",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::bordered().title("SESSIONS"));

    table.render(area, &mut buffer);
    buffer_to_string(&buffer)
}

fn terminal_table_width() -> u16 {
    crossterm::terminal::size().map_or(153, |(width, _)| width)
}

fn buffer_to_string(buffer: &Buffer) -> String {
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.y..area.y.saturating_add(area.height) {
        let mut line = String::new();
        for x in area.x..area.x.saturating_add(area.width) {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end_matches(' '));
        out.push('\n');
    }
    out
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }

    let keep = max.saturating_sub(3);
    let mut out = String::new();
    for ch in value.chars().take(keep) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, ShellFlavor, shell_init_snippet};

    #[test]
    fn bare_cli_defaults_to_no_subcommand() {
        let cli = Cli::parse_from(["grepple"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn shell_init_zsh_contains_alias_and_helper() {
        let snippet = shell_init_snippet(ShellFlavor::Zsh);
        assert!(snippet.contains("grepple() {"));
        assert!(snippet.contains("exit|quit"));
        assert!(snippet.contains("alias g=\"grepple\""));
        assert!(snippet.contains("gr() { command grepple run -- \"$@\"; }"));
    }

    #[test]
    fn shell_init_fish_contains_alias_and_helper() {
        let snippet = shell_init_snippet(ShellFlavor::Fish);
        assert!(snippet.contains("function grepple"));
        assert!(snippet.contains("case exit quit"));
        assert!(snippet.contains("alias g grepple"));
        assert!(snippet.contains("function gr"));
        assert!(snippet.contains("command grepple run -- $argv"));
    }
}
