use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use tempfile::TempDir;

use prsync::{
    remote::{EntryKind, RemoteClient, RemoteEntry, RemoteFileStat},
    sync::{run_sync_with_client, SyncOptions},
};

#[derive(Debug)]
struct DelayedMockRemote {
    entries: Vec<RemoteEntry>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    per_read_delay: Duration,
    reads: Mutex<u64>,
}

impl DelayedMockRemote {
    fn new(entries: Vec<RemoteEntry>, files: BTreeMap<PathBuf, Vec<u8>>, delay_ms: u64) -> Self {
        Self {
            entries,
            files,
            per_read_delay: Duration::from_millis(delay_ms),
            reads: Mutex::new(0),
        }
    }
}

impl RemoteClient for DelayedMockRemote {
    fn list_entries(&self, _recursive: bool) -> Result<Vec<RemoteEntry>> {
        Ok(self.entries.clone())
    }

    fn read_range(&self, relative_path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
        thread::sleep(self.per_read_delay);
        *self.reads.lock().expect("reads lock") += 1;

        let data = self
            .files
            .get(relative_path)
            .ok_or_else(|| anyhow!("missing file"))?;
        let start = offset as usize;
        let end = (offset + len) as usize;
        Ok(data[start..end].to_vec())
    }

    fn stat_file(&self, relative_path: &Path) -> Result<RemoteFileStat> {
        let data = self
            .files
            .get(relative_path)
            .ok_or_else(|| anyhow!("missing file"))?;
        let mtime = self
            .entries
            .iter()
            .find(|e| e.relative_path == relative_path)
            .map(|e| e.mtime_secs)
            .unwrap_or(0);
        Ok(RemoteFileStat {
            size: data.len() as u64,
            mtime_secs: mtime,
        })
    }
}

fn build_dataset() -> (Vec<RemoteEntry>, BTreeMap<PathBuf, Vec<u8>>) {
    let mut entries = Vec::new();
    let mut files = BTreeMap::new();

    for i in 0..40 {
        let name = format!("f{i:03}.bin");
        let path = PathBuf::from(name);
        let content = vec![b'x'; 128 * 1024];
        entries.push(RemoteEntry {
            relative_path: path.clone(),
            kind: EntryKind::File,
            size: content.len() as u64,
            mtime_secs: 1_700_000_000,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        });
        files.insert(path, content);
    }

    (entries, files)
}

fn opts(jobs: usize) -> SyncOptions {
    SyncOptions {
        verbose: false,
        debug: false,
        progress: false,
        recursive: true,
        links: true,
        update: false,
        preserve_perms: false,
        preserve_owner: false,
        preserve_group: false,
        preserve_acls: false,
        preserve_xattrs: false,
        jobs,
        chunk_size: 64 * 1024,
        chunk_threshold: 64 * 1024,
        retries: 2,
        resume: true,
        dry_run: false,
        state_root: None,
        delta_enabled: false,
        delta_min_size: 8 * 1024 * 1024,
        delta_block_size: None,
        delta_max_literals: 64 * 1024 * 1024,
        delta_helper: "prsync --internal-remote-helper".to_string(),
        delta_fallback: true,
        strict_durability: false,
        verify_existing: false,
        sftp_read_concurrency: 4,
        sftp_read_chunk_size: 4 * 1024 * 1024,
        strict_windows_metadata: false,
    }
}

fn run_once(remote: &DelayedMockRemote, jobs: usize) -> Duration {
    let dir = TempDir::new().expect("tmp");
    let start = Instant::now();
    run_sync_with_client(remote, dir.path(), &opts(jobs)).expect("sync");
    start.elapsed()
}

#[test]
#[ignore = "performance smoke test"]
fn parallel_is_faster_than_single_worker() {
    let (entries, files) = build_dataset();
    let remote_single = DelayedMockRemote::new(entries.clone(), files.clone(), 8);
    let remote_parallel = DelayedMockRemote::new(entries, files, 8);

    let single = run_once(&remote_single, 1);
    let parallel = run_once(&remote_parallel, 8);

    assert!(
        parallel < single,
        "expected parallel ({parallel:?}) to be faster than single ({single:?})"
    );
}
