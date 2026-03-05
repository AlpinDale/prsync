use std::process::ExitCode;

use clap::Parser;
use prsync::cli::Cli;

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg == "--internal-remote-helper") {
        return match prsync::remote_helper::run_stdio() {
            Ok(_) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::from(1)
            }
        };
    }

    let cli = Cli::parse();
    match prsync::run_sync(cli) {
        Ok(summary) => {
            if summary.verbose {
                eprintln!(
                    "completed: transferred={}, skipped={}, bytes={}, delta_files={}, delta_fallbacks={}, bytes_saved={}, listing_ms={}, planning_ms={}, read_ms={}, write_ms={}, finalize_ms={}, metadata_ms={}, state_commit_ms={}",
                    summary.transferred_files,
                    summary.skipped_files,
                    summary.transferred_bytes,
                    summary.delta_files,
                    summary.delta_fallback_files,
                    summary.bytes_saved,
                    summary.listing_ms,
                    summary.planning_ms,
                    summary.transfer_read_ms,
                    summary.transfer_write_ms,
                    summary.transfer_finalize_ms,
                    summary.metadata_ms,
                    summary.state_commit_ms
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
