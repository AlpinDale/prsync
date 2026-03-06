use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use tempfile::TempDir;

use parsync::{
    remote::{EntryKind, RemoteClient, RemoteEntry, RemoteFileStat},
    sync::{run_sync_with_client, SyncOptions},
};

#[derive(Debug)]
struct DelayedMockRemote {
    entries: Vec<RemoteEntry>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    per_read_delay: Duration,
}

impl DelayedMockRemote {
    fn new(entries: Vec<RemoteEntry>, files: BTreeMap<PathBuf, Vec<u8>>, delay_ms: u64) -> Self {
        Self {
            entries,
            files,
            per_read_delay: Duration::from_millis(delay_ms),
        }
    }
}

impl RemoteClient for DelayedMockRemote {
    fn list_entries(&self, _recursive: bool) -> Result<Vec<RemoteEntry>> {
        Ok(self.entries.clone())
    }

    fn read_range(&self, relative_path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
        thread::sleep(self.per_read_delay);
        let data = self
            .files
            .get(relative_path)
            .ok_or_else(|| anyhow!("missing file: {}", relative_path.display()))?;
        let start = offset as usize;
        let end = (offset + len).min(data.len() as u64) as usize;
        Ok(data[start..end].to_vec())
    }

    fn stat_file(&self, relative_path: &Path) -> Result<RemoteFileStat> {
        let data = self
            .files
            .get(relative_path)
            .ok_or_else(|| anyhow!("missing file: {}", relative_path.display()))?;
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

struct Scenario {
    name: &'static str,
    entries: Vec<RemoteEntry>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    delay_ms: u64,
}

fn make_entry(path: PathBuf, size: u64) -> RemoteEntry {
    RemoteEntry {
        relative_path: path,
        kind: EntryKind::File,
        size,
        mtime_secs: 1_700_000_000,
        mode: 0o644,
        uid: None,
        gid: None,
        link_target: None,
    }
}

fn build_scenario(
    name: &'static str,
    file_count: usize,
    size_fn: impl Fn(usize) -> usize,
    delay_ms: u64,
) -> Scenario {
    let mut entries = Vec::with_capacity(file_count);
    let mut files = BTreeMap::new();

    for i in 0..file_count {
        let fname = format!("f{i:04}.bin");
        let path = PathBuf::from(fname);
        let size = size_fn(i);
        let content = vec![((i % 251) as u8); size];
        entries.push(make_entry(path.clone(), size as u64));
        files.insert(path, content);
    }

    Scenario {
        name,
        entries,
        files,
        delay_ms,
    }
}

fn scenarios() -> Vec<Scenario> {
    vec![
        build_scenario("Many small (1000x4KB)", 1000, |_| 4 * 1024, 2),
        build_scenario("Medium (40x128KB)", 40, |_| 128 * 1024, 5),
        build_scenario("Few large (5x10MB)", 5, |_| 10 * 1024 * 1024, 10),
        build_scenario(
            "Mixed (200 varied)",
            200,
            |i| match i % 5 {
                0 => 1024,
                1 => 4 * 1024,
                2 => 32 * 1024,
                3 => 256 * 1024,
                _ => 2 * 1024 * 1024,
            },
            5,
        ),
    ]
}

fn sync_opts(jobs: usize) -> SyncOptions {
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
        delta_helper: "parsync --internal-remote-helper".to_string(),
        delta_fallback: true,
        strict_durability: false,
        verify_existing: false,
        sftp_read_concurrency: 4,
        sftp_read_chunk_size: 4 * 1024 * 1024,
        strict_windows_metadata: false,
    }
}

fn run_once(scenario: &Scenario, jobs: usize) -> Duration {
    let remote = DelayedMockRemote::new(
        scenario.entries.clone(),
        scenario.files.clone(),
        scenario.delay_ms,
    );
    let dir = TempDir::new().expect("tmpdir");
    let start = Instant::now();
    run_sync_with_client(&remote, dir.path(), &sync_opts(jobs)).expect("sync failed");
    start.elapsed()
}

fn median(values: &mut Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let len = values.len();
    if len == 0 {
        return 0.0;
    }
    if len % 2 == 1 {
        values[len / 2]
    } else {
        (values[len / 2 - 1] + values[len / 2]) / 2.0
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

struct BenchResult {
    scenario: String,
    jobs: usize,
    median_ms: f64,
    mean_ms: f64,
    speedup: f64,
}

fn main() {
    let runs = 5usize;
    let job_counts = [1, 2, 4, 8, 16];

    eprintln!("parsync benchmark suite");
    eprintln!("runs per config: {runs}");
    eprintln!("job counts: {:?}", job_counts);
    eprintln!();

    let mut results: Vec<BenchResult> = Vec::new();

    for scenario in &scenarios() {
        eprintln!("--- {} ---", scenario.name);

        let mut baseline_median = 0.0f64;

        for &jobs in &job_counts {
            eprint!("  jobs={jobs:>2} ... ");

            let mut times_ms: Vec<f64> = Vec::with_capacity(runs);
            for _ in 0..runs {
                let elapsed = run_once(scenario, jobs);
                times_ms.push(elapsed.as_secs_f64() * 1000.0);
            }

            let med = median(&mut times_ms);
            let avg = mean(&times_ms);

            if jobs == 1 {
                baseline_median = med;
            }

            let speedup = if med > 0.0 {
                baseline_median / med
            } else {
                0.0
            };

            eprintln!("median={med:.1}ms  mean={avg:.1}ms  speedup={speedup:.2}x");

            results.push(BenchResult {
                scenario: scenario.name.to_string(),
                jobs,
                median_ms: med,
                mean_ms: avg,
                speedup,
            });
        }
        eprintln!();
    }

    // JSON output
    print!("{{\"results\":[");
    for (i, r) in results.iter().enumerate() {
        if i > 0 {
            print!(",");
        }
        print!(
            "{{\"scenario\":\"{}\",\"jobs\":{},\"median_ms\":{:.1},\"mean_ms\":{:.1},\"speedup\":{:.2}}}",
            r.scenario, r.jobs, r.median_ms, r.mean_ms, r.speedup
        );
    }
    println!("]}}");

    // Markdown table to stderr for easy capture
    eprintln!("| Scenario | Jobs | Median (ms) | Mean (ms) | vs 1 job |");
    eprintln!("|---|---:|---:|---:|---:|");
    for r in &results {
        eprintln!(
            "| {} | {} | {:.1} | {:.1} | {:.2}x |",
            r.scenario, r.jobs, r.median_ms, r.mean_ms, r.speedup
        );
    }
}
