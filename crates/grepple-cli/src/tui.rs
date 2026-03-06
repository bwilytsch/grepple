use std::{
    io::{self, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use crossterm::{
    cursor, execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use grepple_core::{Grepple, GreppleConfig, model::InstallerResult};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

#[derive(Debug, Clone)]
pub struct InstallUiRequest {
    pub client: String,
    pub name: String,
    pub env: Vec<(String, String)>,
    pub force: bool,
    pub scope: String,
}

pub fn run_install_tui(req: InstallUiRequest) -> Result<InstallerResult> {
    let (tx, rx) = mpsc::channel::<std::result::Result<InstallerResult, String>>();
    let req_for_thread = req.clone();

    thread::spawn(move || {
        let result = (|| -> Result<InstallerResult> {
            let app = Grepple::new(GreppleConfig::default())?;
            app.install_client(
                &req_for_thread.client,
                &req_for_thread.name,
                &req_for_thread.env,
                false,
                req_for_thread.force,
                &req_for_thread.scope,
            )
            .map_err(anyhow::Error::from)
        })();

        let _ = tx.send(result.map_err(|e| e.to_string()));
    });

    let install_outcome = {
        let mut terminal = InstallTerminal::new()?;
        let spinner = ["-", "\\", "|", "/"];
        let started = Instant::now();
        let mut idx = 0usize;

        loop {
            terminal.draw(&req, spinner[idx % spinner.len()], started.elapsed())?;
            idx = idx.wrapping_add(1);

            match rx.recv_timeout(Duration::from_millis(90)) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err("installer worker disconnected".to_string());
                }
            }
        }
    };

    let mut stderr = io::stderr();
    match install_outcome {
        Ok(result) => {
            print_install_result(&mut stderr, &result, &req.name)?;
            Ok(result)
        }
        Err(err) => {
            writeln!(stderr, "grepple :: install")?;
            writeln!(stderr, "  client : {}", req.client)?;
            writeln!(stderr, "  status : failed")?;
            writeln!(stderr, "  error  : {}", err)?;
            Err(anyhow!(err))
        }
    }
}

struct InstallTerminal {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
}

impl InstallTerminal {
    fn new() -> Result<Self> {
        enable_raw_mode()?;

        let mut stderr = io::stderr();
        if let Err(err) = execute!(stderr, EnterAlternateScreen, cursor::Hide) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }

        let backend = CrosstermBackend::new(stderr);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                let mut stderr = io::stderr();
                let _ = execute!(stderr, LeaveAlternateScreen, cursor::Show);
                let _ = disable_raw_mode();
                return Err(err.into());
            }
        };

        if let Err(err) = terminal.clear() {
            let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show);
            let _ = disable_raw_mode();
            return Err(err.into());
        }

        Ok(Self { terminal })
    }

    fn draw(&mut self, req: &InstallUiRequest, spinner: &str, elapsed: Duration) -> Result<()> {
        self.terminal
            .draw(|frame| render_install(frame, req, spinner, elapsed))?;
        Ok(())
    }
}

impl Drop for InstallTerminal {
    fn drop(&mut self) {
        let _ = self.terminal.clear();
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            cursor::Show
        );
        let _ = disable_raw_mode();
    }
}

fn render_install(frame: &mut Frame<'_>, req: &InstallUiRequest, spinner: &str, elapsed: Duration) {
    let area = frame.area();
    let block = Block::bordered().title(" grepple install ");
    let inner = block.inner(area);

    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(spinner, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" Installing Grepple MCP client"),
        ])),
        rows[0],
    );
    frame.render_widget(detail_line("status", "in progress"), rows[1]);
    frame.render_widget(detail_line("client", &req.client), rows[2]);
    frame.render_widget(detail_line("name", &req.name), rows[3]);
    frame.render_widget(
        detail_line("elapsed", &format!("{:.1}s", elapsed.as_secs_f32())),
        rows[4],
    );
    frame.render_widget(
        Paragraph::new(
            "Writing config and installing MCP entry. This screen closes automatically.",
        ),
        rows[5],
    );
}

fn detail_line<'a>(label: &'a str, value: &'a str) -> Paragraph<'a> {
    Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{label:>7} : "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value),
    ]))
}

fn print_install_result(
    stderr: &mut io::Stderr,
    result: &InstallerResult,
    configured_name: &str,
) -> Result<()> {
    writeln!(stderr, "grepple :: install")?;
    writeln!(
        stderr,
        "  status : {}",
        if result.success { "success" } else { "failed" }
    )?;
    writeln!(stderr, "  client : {}", result.client)?;
    writeln!(stderr, "  name   : {}", configured_name)?;

    if let Some(command) = &result.plan.command_preview {
        writeln!(stderr, "  cmd    : {}", command)?;
    }
    if let Some(path) = &result.plan.config_path {
        writeln!(stderr, "  config : {}", path)?;
    }

    writeln!(stderr, "  details: {}", result.details)?;
    Ok(())
}
