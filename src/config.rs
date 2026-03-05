use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cli::Cli;

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub jobs: usize,
    pub chunk_size: u64,
    pub chunk_threshold: u64,
    pub retries: usize,
    pub resume: bool,
    pub state_dir: Option<PathBuf>,
    pub delta_enabled: bool,
    pub delta_min_size: u64,
    pub delta_block_size: Option<u32>,
    pub delta_max_literals: u64,
    pub delta_helper: String,
    pub delta_fallback: bool,
    pub strict_durability: bool,
    pub verify_existing: bool,
    pub sftp_read_concurrency: usize,
    pub sftp_read_chunk_size: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FileConfig {
    jobs: Option<usize>,
    chunk_size: Option<u64>,
    chunk_threshold: Option<u64>,
    retries: Option<usize>,
    resume: Option<bool>,
    state_dir: Option<PathBuf>,
    delta_enabled: Option<bool>,
    delta_min_size: Option<u64>,
    delta_block_size: Option<u32>,
    delta_max_literals: Option<u64>,
    delta_helper: Option<String>,
    delta_fallback: Option<bool>,
    strict_durability: Option<bool>,
    verify_existing: Option<bool>,
    sftp_read_concurrency: Option<usize>,
    sftp_read_chunk_size: Option<u64>,
}

impl ResolvedConfig {
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let file_cfg = load_file_config()?;

        let jobs = cli
            .jobs
            .or_else(|| env_parse::<usize>("PRSYNC_JOBS"))
            .or(file_cfg.jobs)
            .unwrap_or_else(Cli::default_jobs)
            .max(1);

        let chunk_size = cli
            .chunk_size
            .or_else(|| env_parse::<u64>("PRSYNC_CHUNK_SIZE"))
            .or(file_cfg.chunk_size)
            .unwrap_or(8 * 1024 * 1024)
            .max(1);

        let chunk_threshold = cli
            .chunk_threshold
            .or_else(|| env_parse::<u64>("PRSYNC_CHUNK_THRESHOLD"))
            .or(file_cfg.chunk_threshold)
            .unwrap_or(64 * 1024 * 1024)
            .max(1);

        let retries = cli
            .retries
            .or_else(|| env_parse::<usize>("PRSYNC_RETRIES"))
            .or(file_cfg.retries)
            .unwrap_or(5)
            .max(1);

        let resume = if cli.resume {
            true
        } else if cli.no_resume {
            false
        } else if let Some(v) = env_parse::<bool>("PRSYNC_RESUME") {
            v
        } else {
            file_cfg.resume.unwrap_or(true)
        };

        let state_dir = cli
            .state_dir
            .clone()
            .or_else(|| std::env::var("PRSYNC_STATE_DIR").ok().map(PathBuf::from))
            .or(file_cfg.state_dir);

        let delta_enabled = if cli.delta {
            true
        } else {
            env_parse::<bool>("PRSYNC_DELTA")
                .or(file_cfg.delta_enabled)
                .unwrap_or(false)
        };
        let delta_min_size = cli
            .delta_min_size
            .or_else(|| env_parse::<u64>("PRSYNC_DELTA_MIN_SIZE"))
            .or(file_cfg.delta_min_size)
            .unwrap_or(8 * 1024 * 1024)
            .max(1);
        let delta_block_size = cli
            .delta_block_size
            .or_else(|| env_parse::<u32>("PRSYNC_DELTA_BLOCK_SIZE"))
            .or(file_cfg.delta_block_size);
        let delta_max_literals = cli
            .delta_max_literals
            .or_else(|| env_parse::<u64>("PRSYNC_DELTA_MAX_LITERALS"))
            .or(file_cfg.delta_max_literals)
            .unwrap_or(64 * 1024 * 1024);
        let delta_helper = cli
            .delta_helper
            .clone()
            .or_else(|| std::env::var("PRSYNC_DELTA_HELPER").ok())
            .or(file_cfg.delta_helper)
            .unwrap_or_else(|| "prsync --internal-remote-helper".to_string());
        let delta_fallback = if cli.no_delta_fallback {
            false
        } else {
            env_parse::<bool>("PRSYNC_DELTA_FALLBACK")
                .or(file_cfg.delta_fallback)
                .unwrap_or(true)
        };
        let strict_durability = if cli.strict_durability {
            true
        } else {
            env_parse::<bool>("PRSYNC_STRICT_DURABILITY")
                .or(file_cfg.strict_durability)
                .unwrap_or(false)
        };
        let verify_existing = if cli.verify_existing {
            true
        } else {
            env_parse::<bool>("PRSYNC_VERIFY_EXISTING")
                .or(file_cfg.verify_existing)
                .unwrap_or(false)
        };
        let sftp_read_concurrency = cli
            .sftp_read_concurrency
            .or_else(|| env_parse::<usize>("PRSYNC_SFTP_READ_CONCURRENCY"))
            .or(file_cfg.sftp_read_concurrency)
            .unwrap_or(4)
            .max(1);
        let sftp_read_chunk_size = cli
            .sftp_read_chunk_size
            .or_else(|| env_parse::<u64>("PRSYNC_SFTP_READ_CHUNK_SIZE"))
            .or(file_cfg.sftp_read_chunk_size)
            .unwrap_or(4 * 1024 * 1024)
            .max(1);

        Ok(Self {
            jobs,
            chunk_size,
            chunk_threshold,
            retries,
            resume,
            state_dir,
            delta_enabled,
            delta_min_size,
            delta_block_size,
            delta_max_literals,
            delta_helper,
            delta_fallback,
            strict_durability,
            verify_existing,
            sftp_read_concurrency,
            sftp_read_chunk_size,
        })
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.parse::<T>().ok())
}

fn load_file_config() -> Result<FileConfig> {
    let Some(path) = default_config_path() else {
        return Ok(FileConfig::default());
    };
    if !path.exists() {
        return Ok(FileConfig::default());
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read config file: {}", path.display()))?;
    let cfg = toml::from_str::<FileConfig>(&raw)
        .with_context(|| format!("parse config file: {}", path.display()))?;
    Ok(cfg)
}

fn default_config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/prsync/config.toml"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use crate::cli::Cli;

    use super::ResolvedConfig;

    #[test]
    fn cli_values_take_priority() {
        let cli = Cli::parse_from([
            "prsync",
            "--jobs",
            "9",
            "--chunk-size",
            "1024",
            "--chunk-threshold",
            "2048",
            "--retries",
            "3",
            "--state-dir",
            "/tmp/state",
            "h:/r",
            "/tmp/d",
        ]);
        let cfg = ResolvedConfig::from_cli(&cli).expect("resolve");
        assert_eq!(cfg.jobs, 9);
        assert_eq!(cfg.chunk_size, 1024);
        assert_eq!(cfg.chunk_threshold, 2048);
        assert_eq!(cfg.retries, 3);
        assert_eq!(cfg.state_dir, Some(PathBuf::from("/tmp/state")));
    }
}
