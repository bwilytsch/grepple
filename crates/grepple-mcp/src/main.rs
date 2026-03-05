use std::process::ExitCode;

fn main() -> ExitCode {
    match grepple_core::mcp::run_from_default_config() {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
