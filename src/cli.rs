use clap::{ArgAction, Parser};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "prsync",
    version,
    about = "Parallel rsync-like pull sync over SSH"
)]
pub struct Cli {
    /// Increase log verbosity
    #[arg(short = 'v', long = "verbose", action = ArgAction::SetTrue)]
    pub verbose: bool,

    /// Recurse into directories
    #[arg(short = 'r', long = "recursive", action = ArgAction::SetTrue)]
    pub recursive: bool,

    /// Equivalent to --partial --progress
    #[arg(short = 'P', action = ArgAction::SetTrue)]
    pub progress_partial: bool,

    /// Preserve symlinks
    #[arg(short = 'l', long = "links", action = ArgAction::SetTrue)]
    pub links: bool,

    /// Skip files newer on receiver
    #[arg(short = 'u', long = "update", action = ArgAction::SetTrue)]
    pub update: bool,

    /// Preserve permissions
    #[arg(short = 'p', long = "perms", action = ArgAction::SetTrue)]
    pub preserve_perms: bool,

    /// Preserve owner
    #[arg(short = 'o', long = "owner", action = ArgAction::SetTrue)]
    pub preserve_owner: bool,

    /// Preserve group
    #[arg(short = 'g', long = "group", action = ArgAction::SetTrue)]
    pub preserve_group: bool,

    /// Preserve ACLs
    #[arg(short = 'A', long = "acls", action = ArgAction::SetTrue)]
    pub preserve_acls: bool,

    /// Preserve xattrs
    #[arg(short = 'X', long = "xattrs", action = ArgAction::SetTrue)]
    pub preserve_xattrs: bool,

    /// Number of parallel transfer workers
    #[arg(long = "jobs")]
    pub jobs: Option<usize>,

    /// Chunk size for large files in bytes
    #[arg(long = "chunk-size")]
    pub chunk_size: Option<u64>,

    /// Files >= threshold are transferred in chunks
    #[arg(long = "chunk-threshold")]
    pub chunk_threshold: Option<u64>,

    /// Retry attempts per failed chunk/read
    #[arg(long = "retries")]
    pub retries: Option<usize>,

    /// Override state directory path (default: `<destination>/.prsync`)
    #[arg(long = "state-dir")]
    pub state_dir: Option<std::path::PathBuf>,

    /// Disable resume logic
    #[arg(long = "no-resume", action = ArgAction::SetTrue, conflicts_with = "resume")]
    pub no_resume: bool,

    /// Force resume logic on
    #[arg(long = "resume", action = ArgAction::SetTrue)]
    pub resume: bool,

    /// Dry run only (plan/skip output, no file writes)
    #[arg(long = "dry-run", action = ArgAction::SetTrue)]
    pub dry_run: bool,

    /// Enable rsync-style block-delta transfer for eligible files
    #[arg(long = "delta", action = ArgAction::SetTrue)]
    pub delta: bool,

    /// Minimum file size in bytes eligible for delta mode
    #[arg(long = "delta-min-size")]
    pub delta_min_size: Option<u64>,

    /// Fixed delta block size in bytes (auto if omitted)
    #[arg(long = "delta-block-size")]
    pub delta_block_size: Option<u32>,

    /// Max unmatched literal bytes before falling back to full transfer
    #[arg(long = "delta-max-literals")]
    pub delta_max_literals: Option<u64>,

    /// Remote helper command (default: prsync --internal-remote-helper)
    #[arg(long = "delta-helper")]
    pub delta_helper: Option<String>,

    /// Fail instead of falling back to full transfer when delta path fails
    #[arg(long = "no-delta-fallback", action = ArgAction::SetTrue)]
    pub no_delta_fallback: bool,

    /// Enable strict crash-durability semantics (extra fsync/checkpoint costs)
    #[arg(long = "strict-durability", action = ArgAction::SetTrue)]
    pub strict_durability: bool,

    /// Verify digests for already-existing files before skip decisions (expensive)
    #[arg(long = "verify-existing", action = ArgAction::SetTrue)]
    pub verify_existing: bool,

    /// Parallel SFTP read requests per file for large files
    #[arg(long = "sftp-read-concurrency")]
    pub sftp_read_concurrency: Option<usize>,

    /// SFTP range request chunk size in bytes
    #[arg(long = "sftp-read-chunk-size")]
    pub sftp_read_chunk_size: Option<u64>,

    /// SSH remote source spec: `[user@]host:/path`
    pub remote_source: String,

    /// Local destination path
    pub local_destination: std::path::PathBuf,
}

impl Cli {
    pub fn partial(&self) -> bool {
        self.progress_partial
    }

    pub fn progress(&self) -> bool {
        self.progress_partial
    }

    pub fn resume(&self) -> bool {
        self.resume || !self.no_resume
    }

    pub fn default_jobs() -> usize {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        (cpus * 2).clamp(4, 32)
    }

    pub fn effective_jobs(&self) -> usize {
        self.jobs.unwrap_or_else(Self::default_jobs)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn parses_vrplu_flags() {
        let cli = Cli::parse_from(["prsync", "-vrPlu", "user@h:/r", "/tmp/d"]);
        assert!(cli.verbose);
        assert!(cli.recursive);
        assert!(cli.progress_partial);
        assert!(cli.links);
        assert!(cli.update);
        assert!(cli.partial());
        assert!(cli.progress());
    }
}
