use std::process::ExitCode;

use clap::Parser;
use prsync::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match prsync::run_sync(cli) {
        Ok(summary) => {
            if summary.verbose {
                eprintln!(
                    "completed: transferred={}, skipped={}, bytes={}",
                    summary.transferred_files, summary.skipped_files, summary.transferred_bytes
                );
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}
