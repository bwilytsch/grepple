use std::{
    io::{self, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use grepple_core::{Grepple, GreppleConfig, model::InstallerResult};

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

    let mut stderr = io::stderr();
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let started = Instant::now();
    let mut idx = 0usize;

    loop {
        match rx.try_recv() {
            Ok(Ok(result)) => {
                clear_progress_line(&mut stderr)?;
                print_install_result(&mut stderr, &result, &req.name)?;
                return Ok(result);
            }
            Ok(Err(err)) => {
                clear_progress_line(&mut stderr)?;
                writeln!(stderr, "grepple :: install")?;
                writeln!(stderr, "  client : {}", req.client)?;
                writeln!(stderr, "  status : failed")?;
                writeln!(stderr, "  error  : {}", err)?;
                return Err(anyhow!(err));
            }
            Err(mpsc::TryRecvError::Empty) => {
                let frame = spinner[idx % spinner.len()];
                idx = idx.wrapping_add(1);
                let elapsed = started.elapsed().as_secs_f32();
                write!(
                    stderr,
                    "\r{frame} grepple :: installing MCP for {} ({elapsed:.1}s)",
                    req.client
                )?;
                stderr.flush()?;
                thread::sleep(Duration::from_millis(90));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                clear_progress_line(&mut stderr)?;
                return Err(anyhow!("installer worker disconnected"));
            }
        }
    }
}

fn clear_progress_line(stderr: &mut io::Stderr) -> Result<()> {
    write!(stderr, "\r{}\r", " ".repeat(80))?;
    stderr.flush()?;
    Ok(())
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
