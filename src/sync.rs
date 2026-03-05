use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, Once,
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use filetime::{set_file_mtime, FileTime};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::prelude::*;

use crate::{
    cli::Cli,
    config::ResolvedConfig,
    hashing::{format_digest, hash_file},
    remote::{EntryKind, RemoteClient, RemoteEntry, RemoteSpec, SshRemote},
    state::{acquire_destination_lock, StateStore},
};

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub transferred_files: u64,
    pub skipped_files: u64,
    pub transferred_bytes: u64,
    pub verbose: bool,
}

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub verbose: bool,
    pub progress: bool,
    pub recursive: bool,
    pub links: bool,
    pub update: bool,
    pub preserve_perms: bool,
    pub preserve_owner: bool,
    pub preserve_group: bool,
    pub preserve_acls: bool,
    pub preserve_xattrs: bool,
    pub jobs: usize,
    pub chunk_size: u64,
    pub chunk_threshold: u64,
    pub retries: usize,
    pub resume: bool,
    pub dry_run: bool,
    pub state_root: Option<PathBuf>,
}

impl SyncOptions {
    fn from_cli(cli: &Cli) -> Result<Self> {
        let resolved = ResolvedConfig::from_cli(cli)?;
        Ok(Self {
            verbose: cli.verbose,
            progress: cli.progress(),
            recursive: cli.recursive,
            links: cli.links,
            update: cli.update,
            preserve_perms: cli.preserve_perms,
            preserve_owner: cli.preserve_owner,
            preserve_group: cli.preserve_group,
            preserve_acls: cli.preserve_acls,
            preserve_xattrs: cli.preserve_xattrs,
            jobs: resolved.jobs,
            chunk_size: resolved.chunk_size,
            chunk_threshold: resolved.chunk_threshold,
            retries: resolved.retries,
            resume: resolved.resume,
            dry_run: cli.dry_run,
            state_root: resolved.state_dir,
        })
    }
}

#[derive(Debug, Clone)]
struct FileJob {
    entry: RemoteEntry,
    destination: PathBuf,
}

pub fn run_sync(cli: Cli) -> Result<RunSummary> {
    let options = SyncOptions::from_cli(&cli)?;
    vlog(
        &options,
        format!(
            "starting sync source={} dest={} jobs={} chunk_size={} threshold={} resume={}",
            cli.remote_source,
            cli.local_destination.display(),
            options.jobs,
            options.chunk_size,
            options.chunk_threshold,
            options.resume
        ),
    );
    let spec = RemoteSpec::parse(&cli.remote_source)?;
    vlog(
        &options,
        format!(
            "parsed remote host={} port={} path={}",
            spec.host, spec.port, spec.path
        ),
    );
    let remote = SshRemote::connect(spec, options.jobs)?;
    vlog(&options, "ssh connection pool established");
    run_sync_with_client(&remote, &cli.local_destination, &options)
}

pub fn run_sync_with_client<R: RemoteClient + Sync>(
    remote: &R,
    local_destination: &Path,
    options: &SyncOptions,
) -> Result<RunSummary> {
    install_signal_handlers()?;
    clear_interrupt_flag();

    fs::create_dir_all(local_destination).with_context(|| {
        format!(
            "create destination directory: {}",
            local_destination.display()
        )
    })?;
    let state_root = options
        .state_root
        .clone()
        .unwrap_or_else(|| local_destination.join(".prsync"));
    vlog(options, format!("state root: {}", state_root.display()));
    let _destination_lock = acquire_destination_lock(&state_root)?;
    vlog(options, "destination lock acquired");

    vlog(options, "listing remote entries...");
    let mut entries = list_entries_with_spinner(remote, options)?;
    entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    vlog(
        options,
        format!("remote listing complete: {} entries", entries.len()),
    );

    let state = Arc::new(Mutex::new(StateStore::load(&state_root)?));
    {
        let valid_file_keys: HashSet<String> = entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .map(|e| StateStore::key_for(&e.relative_path))
            .collect();
        let locked = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
        locked.prune_to_keys(&valid_file_keys)?;
    }
    if !options.resume {
        let locked = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
        locked.clear_all()?;
        locked.save()?;
    }

    let mut jobs = Vec::new();
    let mut dir_count = 0_u64;
    let mut symlink_count = 0_u64;
    let mut file_count = 0_u64;
    let mut skipped = 0_u64;

    for entry in entries {
        check_interrupted()?;
        validate_relative_path(&entry.relative_path)?;
        let destination = local_destination.join(&entry.relative_path);

        match entry.kind {
            EntryKind::Dir => {
                dir_count += 1;
                if options.dry_run {
                    continue;
                }
                fs::create_dir_all(&destination)
                    .with_context(|| format!("create dir: {}", destination.display()))?;
                apply_mtime(&destination, entry.mtime_secs)?;
                apply_metadata(remote, &entry.relative_path, &destination, &entry, options)?;
            }
            EntryKind::Symlink => {
                symlink_count += 1;
                if !options.links {
                    continue;
                }
                if options.dry_run {
                    continue;
                }
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                create_or_replace_symlink(&destination, entry.link_target.as_ref())?;
            }
            EntryKind::File => {
                file_count += 1;
                let should_transfer = should_transfer(&destination, &entry, options, &state)?;
                if should_transfer {
                    jobs.push(FileJob { entry, destination });
                } else {
                    if !options.dry_run && should_apply_file_metadata(options) {
                        apply_metadata(
                            remote,
                            &entry.relative_path,
                            &destination,
                            &entry,
                            options,
                        )?;
                    }
                    skipped += 1;
                }
            }
        }
    }
    let total_bytes: u64 = jobs.iter().map(|j| j.entry.size).sum();
    let plan_summary = PlanSummary {
        total_entries: file_count + dir_count + symlink_count,
        files: file_count,
        dirs: dir_count,
        symlinks: symlink_count,
        queued: jobs.len() as u64,
        skipped,
        queued_bytes: total_bytes,
    };
    print_plan_summary(options, &plan_summary);
    let ui = Arc::new(TransferUi::new(jobs.len() as u64, total_bytes, options));

    let transferred_files = AtomicU64::new(0);
    let transferred_bytes = AtomicU64::new(0);
    let errors: Arc<Mutex<Vec<anyhow::Error>>> = Arc::new(Mutex::new(Vec::new()));

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.jobs)
        .build()
        .context("build thread pool")?;

    pool.install(|| {
        jobs.par_iter().for_each(|job| {
            if is_interrupted() {
                let mut lock = errors.lock().expect("error lock");
                lock.push(anyhow!("interrupted by signal"));
                return;
            }
            if options.dry_run {
                transferred_files.fetch_add(1, Ordering::Relaxed);
                return;
            }

            if let Err(err) = transfer_one(remote, job, options, &state, &ui) {
                let mut lock = errors.lock().expect("error lock");
                lock.push(err.context(format!(
                    "failed transfer: {}",
                    job.entry.relative_path.display()
                )));
                return;
            }

            transferred_files.fetch_add(1, Ordering::Relaxed);
            transferred_bytes.fetch_add(job.entry.size, Ordering::Relaxed);
            ui.finish_file(&job.entry.relative_path);
        });
    });

    let mut errs = errors.lock().map_err(|_| anyhow!("error lock poisoned"))?;
    if let Some(err) = errs.pop() {
        return Err(err);
    }
    ui.finish_all();

    Ok(RunSummary {
        transferred_files: transferred_files.load(Ordering::Relaxed),
        skipped_files: skipped,
        transferred_bytes: transferred_bytes.load(Ordering::Relaxed),
        verbose: options.verbose,
    })
}

fn should_transfer(
    destination: &Path,
    entry: &RemoteEntry,
    options: &SyncOptions,
    state: &Arc<Mutex<StateStore>>,
) -> Result<bool> {
    if !destination.exists() {
        return Ok(true);
    }

    let meta = fs::metadata(destination)?;
    let local_mtime = mtime_secs(&meta)?;
    let local_len = meta.len();

    if options.update && local_mtime > entry.mtime_secs {
        return Ok(false);
    }

    if local_len == entry.size && local_mtime == entry.mtime_secs {
        return Ok(false);
    }

    if options.resume {
        let locked = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
        if let Some(file_state) = locked.file_state(&entry.relative_path)? {
            if file_state.remote_size == entry.size
                && file_state.remote_mtime_secs == entry.mtime_secs
                && file_state.finished
            {
                if let Some(expected) = &file_state.digest_hex {
                    let actual = format_digest(hash_file(destination)?);
                    if &actual == expected {
                        return Ok(false);
                    }
                }
            }
        }
    }

    Ok(true)
}

fn transfer_one<R: RemoteClient + Sync>(
    remote: &R,
    job: &FileJob,
    options: &SyncOptions,
    state: &Arc<Mutex<StateStore>>,
    ui: &Arc<TransferUi>,
) -> Result<()> {
    vlog(
        options,
        format!(
            "transfer start: {} ({} bytes)",
            job.entry.relative_path.display(),
            job.entry.size
        ),
    );
    if let Some(parent) = job.destination.parent() {
        fs::create_dir_all(parent)?;
    }

    for change_retry in 0..2 {
        check_interrupted()?;
        let chunk_size = if job.entry.size >= options.chunk_threshold {
            options.chunk_size
        } else {
            job.entry.size.max(1)
        };

        let part_path = {
            let locked = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
            locked.upsert_file(
                &job.entry.relative_path,
                job.entry.size,
                job.entry.mtime_secs,
                chunk_size,
            )?;
            locked.save()?;
            locked.part_path_for(&job.entry.relative_path)
        };

        let part_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&part_path)
            .with_context(|| format!("open partial file: {}", part_path.display()))?;
        part_file.set_len(job.entry.size)?;
        let part_file = Arc::new(part_file);

        let chunk_count = chunk_count(job.entry.size, chunk_size);
        let completed = {
            let locked = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
            locked
                .file_state(&job.entry.relative_path)?
                .map(|f| f.completed_chunks.clone())
                .unwrap_or_default()
        };

        let missing_chunks: Vec<u64> = (0..chunk_count)
            .filter(|idx| !completed.contains(idx))
            .collect();
        vlog(
            options,
            format!(
                "chunk plan for {}: total={} remaining={}",
                job.entry.relative_path.display(),
                chunk_count,
                missing_chunks.len()
            ),
        );

        let attempt_transferred = AtomicU64::new(0);

        missing_chunks
            .par_iter()
            .try_for_each(|idx| -> Result<()> {
                let offset = *idx * chunk_size;
                let remain = job.entry.size.saturating_sub(offset);
                let len = remain.min(chunk_size);
                let mut last_err: Option<anyhow::Error> = None;

                for _ in 0..options.retries {
                    if is_interrupted() {
                        return Err(anyhow!("interrupted by signal"));
                    }
                    match remote.read_range(&job.entry.relative_path, offset, len) {
                        Ok(buf) => {
                            if buf.len() as u64 != len {
                                last_err = Some(anyhow!(
                                    "short read for {}: expected {}, got {}",
                                    job.entry.relative_path.display(),
                                    len,
                                    buf.len()
                                ));
                                continue;
                            }
                            write_all_at(part_file.as_ref(), &buf, offset)?;

                            let locked =
                                state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                            locked.mark_chunk_completed(&job.entry.relative_path, *idx)?;
                            locked.save()?;
                            ui.inc_chunk_bytes(len);
                            attempt_transferred.fetch_add(len, Ordering::Relaxed);
                            return Ok(());
                        }
                        Err(err) => {
                            last_err = Some(err);
                        }
                    }
                }

                Err(last_err
                    .unwrap_or_else(|| anyhow!("chunk transfer failed"))
                    .context("chunk transfer failed after retries"))
            })?;

        part_file.as_ref().sync_all()?;
        let latest = remote.stat_file(&job.entry.relative_path)?;
        if latest.size != job.entry.size || latest.mtime_secs != job.entry.mtime_secs {
            let already_counted = attempt_transferred.load(Ordering::Relaxed);
            if already_counted > 0 {
                ui.dec_chunk_bytes(already_counted);
            }
            let _ = fs::remove_file(&part_path);
            let locked = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
            locked.reset_progress(&job.entry.relative_path)?;
            locked.save()?;
            if change_retry == 0 {
                continue;
            }
            bail!(
                "remote file changed during transfer: {}",
                job.entry.relative_path.display()
            );
        }

        let digest = format_digest(hash_file(&part_path)?);
        fs::rename(&part_path, &job.destination).with_context(|| {
            format!(
                "rename partial to destination: {} -> {}",
                part_path.display(),
                job.destination.display()
            )
        })?;
        apply_mtime(&job.destination, job.entry.mtime_secs)?;
        apply_metadata(
            remote,
            &job.entry.relative_path,
            &job.destination,
            &job.entry,
            options,
        )?;

        let locked = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
        locked.mark_finished_with_digest(&job.entry.relative_path, digest)?;
        locked.save()?;
        vlog(
            options,
            format!("transfer complete: {}", job.entry.relative_path.display()),
        );
        return Ok(());
    }

    bail!("unexpected transfer retry exhaustion")
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static INTERRUPT_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGNAL_INIT: Once = Once::new();

fn install_signal_handlers() -> Result<()> {
    let mut setup_err: Option<anyhow::Error> = None;
    SIGNAL_INIT.call_once(|| {
        if let Err(err) = ctrlc::set_handler(|| {
            let count = INTERRUPT_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
            INTERRUPTED.store(true, Ordering::SeqCst);
            if count >= 2 {
                eprintln!("[prsync] received second interrupt, forcing exit");
                std::process::exit(130);
            } else {
                eprintln!("[prsync] interrupt received, stopping after current operation...");
            }
        }) {
            setup_err = Some(anyhow!(err));
        }
    });
    if let Some(err) = setup_err {
        return Err(err.context("install signal handler"));
    }
    Ok(())
}

fn clear_interrupt_flag() {
    INTERRUPTED.store(false, Ordering::SeqCst);
    INTERRUPT_COUNT.store(0, Ordering::SeqCst);
}

fn is_interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

fn check_interrupted() -> Result<()> {
    if is_interrupted() {
        bail!("interrupted by signal");
    }
    Ok(())
}

fn list_entries_with_spinner<R: RemoteClient + Sync>(
    remote: &R,
    options: &SyncOptions,
) -> Result<Vec<RemoteEntry>> {
    if !options.verbose {
        return remote.list_entries(options.recursive);
    }

    let indexed = Arc::new(AtomicUsize::new(0));
    let indexed_for_cb = Arc::clone(&indexed);
    let progress_cb = move |n: usize| {
        indexed_for_cb.store(n, Ordering::Relaxed);
    };

    let spinner = ProgressBar::new_spinner();
    let style = ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner());
    spinner.set_style(style);
    let done = Arc::new(AtomicBool::new(false));
    let done_clone = Arc::clone(&done);
    let indexed_clone = Arc::clone(&indexed);
    let spinner_clone = spinner.clone();
    let start = Instant::now();
    let ticker = thread::spawn(move || {
        while !done_clone.load(Ordering::Relaxed) {
            let count = indexed_clone.load(Ordering::Relaxed);
            let elapsed = start.elapsed().as_secs();
            spinner_clone.set_message(format!(
                "Scanning remote tree... indexed {count} entries ({elapsed}s)"
            ));
            thread::sleep(Duration::from_millis(120));
        }
    });
    spinner.enable_steady_tick(Duration::from_millis(100));
    let result = remote.list_entries_with_progress(options.recursive, Some(&progress_cb));
    done.store(true, Ordering::Relaxed);
    let _ = ticker.join();
    match &result {
        Ok(entries) => {
            spinner.finish_with_message(format!("Scanned remote tree: {} entries", entries.len()))
        }
        Err(_) => spinner.finish_with_message("Scan failed"),
    }
    result
}

fn vlog(options: &SyncOptions, message: impl AsRef<str>) {
    if options.verbose && !options.progress {
        eprintln!("[prsync] {}", message.as_ref());
    }
}

struct PlanSummary {
    total_entries: u64,
    files: u64,
    dirs: u64,
    symlinks: u64,
    queued: u64,
    skipped: u64,
    queued_bytes: u64,
}

fn print_plan_summary(options: &SyncOptions, summary: &PlanSummary) {
    let summary = format!(
        "Plan: entries={total_entries} files={files} dirs={dirs} symlinks={symlinks} queued={queued} skipped={skipped} data={}",
        format_bytes_human(summary.queued_bytes),
        total_entries = summary.total_entries,
        files = summary.files,
        dirs = summary.dirs,
        symlinks = summary.symlinks,
        queued = summary.queued,
        skipped = summary.skipped,
    );
    if options.progress || options.verbose {
        eprintln!("[prsync] {summary}");
    }
}

fn format_bytes_human(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let b = bytes as f64;
    if b >= TB {
        return format!("{:.2} TiB", b / TB);
    }
    if b >= GB {
        return format!("{:.2} GiB", b / GB);
    }
    if b >= MB {
        return format!("{:.2} MiB", b / MB);
    }
    if b >= KB {
        return format!("{:.2} KiB", b / KB);
    }
    format!("{bytes} B")
}

#[derive(Debug)]
struct TransferUi {
    _multi: MultiProgress,
    bar: ProgressBar,
    file_line: ProgressBar,
    show_bars: bool,
    total_files: u64,
    started_at: Instant,
    transferred_bytes: AtomicU64,
    transferred_files: AtomicU64,
    last_path: Mutex<Option<String>>,
    last_rate_update: Mutex<Instant>,
}

impl TransferUi {
    fn new(total_files: u64, total_bytes: u64, options: &SyncOptions) -> Self {
        let show_bars = options.progress || options.verbose;
        let multi = MultiProgress::new();
        let bar = multi.add(ProgressBar::new(total_bytes));
        let file_line = multi.add(ProgressBar::new_spinner());

        if show_bars {
            let style = ProgressStyle::with_template(
                "{spinner:.green} {bytes}/{total_bytes} [{bar:28.green/blue}] {msg} ETA {eta}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> ");
            bar.set_style(style);
            bar.set_message(format!("files 0/{total_files} | 0.00 MiB/s"));

            let file_style = ProgressStyle::with_template("{msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner());
            file_line.set_style(file_style);
            file_line.set_message("last: -");
            if total_bytes == 0 {
                bar.finish_with_message(format!("files 0/{total_files} | 0 B"));
            }
        } else {
            bar.set_draw_target(ProgressDrawTarget::hidden());
            file_line.set_draw_target(ProgressDrawTarget::hidden());
        }

        Self {
            _multi: multi,
            bar,
            file_line,
            show_bars,
            total_files,
            started_at: Instant::now(),
            transferred_bytes: AtomicU64::new(0),
            transferred_files: AtomicU64::new(0),
            last_path: Mutex::new(None),
            last_rate_update: Mutex::new(Instant::now()),
        }
    }

    fn inc_chunk_bytes(&self, bytes: u64) {
        self.bar.inc(bytes);
        self.transferred_bytes.fetch_add(bytes, Ordering::Relaxed);
        if !self.show_bars {
            return;
        }

        if let Ok(mut last) = self.last_rate_update.lock() {
            if last.elapsed() >= Duration::from_millis(250) {
                self.refresh_message();
                *last = Instant::now();
            }
        }
    }

    fn dec_chunk_bytes(&self, bytes: u64) {
        self.bar.dec(bytes);
        self.transferred_bytes.fetch_sub(bytes, Ordering::Relaxed);
        if self.show_bars {
            self.refresh_message();
        }
    }

    fn finish_file(&self, path: &Path) {
        self.transferred_files.fetch_add(1, Ordering::Relaxed);
        let truncated = truncate_for_terminal(&path.display().to_string(), 96);
        if let Ok(mut last) = self.last_path.lock() {
            *last = Some(truncated.clone());
        }
        if self.show_bars {
            self.file_line.set_message(format!("last: {truncated}"));
        }
        if self.show_bars {
            self.refresh_message();
        }
    }

    fn refresh_message(&self) {
        let elapsed = self.started_at.elapsed().as_secs_f64().max(0.001);
        let total = self.transferred_bytes.load(Ordering::Relaxed) as f64;
        let mibps = (total / (1024.0 * 1024.0)) / elapsed;
        let files_done = self.transferred_files.load(Ordering::Relaxed);
        let message = format!("files {files_done}/{} | {mibps:.2} MiB/s", self.total_files);
        self.bar.set_message(message);
    }

    fn finish_all(&self) {
        let files_done = self.transferred_files.load(Ordering::Relaxed);
        let total = self.transferred_bytes.load(Ordering::Relaxed);
        if !self.bar.is_finished() {
            self.bar.finish_with_message(format!(
                "files {files_done}/{} | {} B",
                self.total_files, total
            ));
        }
        if !self.file_line.is_finished() {
            self.file_line.finish_and_clear();
        }
    }
}

fn truncate_for_terminal(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let head = keep * 3 / 5;
    let tail = keep.saturating_sub(head);
    let prefix: String = s.chars().take(head).collect();
    let suffix: String = s
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

fn chunk_count(size: u64, chunk_size: u64) -> u64 {
    if size == 0 {
        return 1;
    }
    ((size - 1) / chunk_size) + 1
}

fn mtime_secs(meta: &fs::Metadata) -> Result<i64> {
    let modified = meta.modified()?;
    let secs = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    Ok(secs)
}

fn apply_mtime(path: &Path, mtime_secs: i64) -> Result<()> {
    let ts = FileTime::from_unix_time(mtime_secs, 0);
    set_file_mtime(path, ts).with_context(|| format!("set mtime: {}", path.display()))
}

fn apply_metadata<R: RemoteClient + Sync>(
    remote: &R,
    relative_path: &Path,
    path: &Path,
    entry: &RemoteEntry,
    options: &SyncOptions,
) -> Result<()> {
    #[cfg(unix)]
    {
        use nix::unistd::{chown, Gid, Uid};
        use std::os::unix::fs::PermissionsExt;

        if options.preserve_perms {
            let perms = fs::Permissions::from_mode(entry.mode);
            fs::set_permissions(path, perms)
                .with_context(|| format!("set permissions for {}", path.display()))?;
        }

        if options.preserve_owner || options.preserve_group {
            let uid = if options.preserve_owner {
                entry.uid.map(Uid::from_raw)
            } else {
                None
            };
            let gid = if options.preserve_group {
                entry.gid.map(Gid::from_raw)
            } else {
                None
            };
            chown(path, uid, gid).with_context(|| format!("chown {}", path.display()))?;
        }

        if options.preserve_xattrs {
            for (key, value) in remote.get_xattrs(relative_path)? {
                xattr::set(path, key, &value)
                    .with_context(|| format!("set xattr for {}", path.display()))?;
            }
        }

        if options.preserve_acls {
            if let Some(acl_text) = remote.get_acl_text(relative_path)? {
                apply_acl(path, &acl_text)?;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (remote, relative_path, path, entry, options);
        if options.preserve_xattrs || options.preserve_acls {
            bail!("ACL/xattr preservation is only supported on unix in v1");
        }
    }

    Ok(())
}

#[cfg(unix)]
fn apply_acl(path: &Path, acl_text: &str) -> Result<()> {
    let mut child = Command::new("setfacl")
        .arg("--set-file=-")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn setfacl for {}", path.display()))?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(acl_text.as_bytes())
            .with_context(|| format!("write acl for {}", path.display()))?;
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("wait setfacl for {}", path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("setfacl failed for {}: {}", path.display(), stderr.trim());
    }
    Ok(())
}

fn should_apply_file_metadata(options: &SyncOptions) -> bool {
    options.preserve_perms
        || options.preserve_owner
        || options.preserve_group
        || options.preserve_acls
        || options.preserve_xattrs
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!("remote entry has absolute path: {}", path.display());
    }
    for component in path.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("unsafe remote path component in {}", path.display())
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn create_or_replace_symlink(link_path: &Path, target: Option<&PathBuf>) -> Result<()> {
    let target = target.ok_or_else(|| anyhow!("symlink entry missing target"))?;
    if let Ok(existing) = fs::symlink_metadata(link_path) {
        if existing.file_type().is_symlink() || existing.is_file() {
            fs::remove_file(link_path)?;
        } else if existing.is_dir() {
            fs::remove_dir_all(link_path)?;
        }
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link_path).with_context(|| {
            format!(
                "create symlink {} -> {}",
                link_path.display(),
                target.display()
            )
        })?;
    }

    #[cfg(not(unix))]
    {
        let _ = (target, link_path);
        bail!("symlink creation is only supported on unix in v1");
    }

    Ok(())
}

fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;

        while !buf.is_empty() {
            let n = file.write_at(buf, offset)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write all bytes",
                ));
            }
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, buf, offset);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "write_at not supported on non-unix",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        collections::BTreeSet,
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use anyhow::{anyhow, Result};
    use tempfile::TempDir;

    use crate::remote::{EntryKind, RemoteClient, RemoteEntry, RemoteFileStat};

    use super::{run_sync_with_client, SyncOptions};

    #[derive(Debug)]
    struct MockRemote {
        entries: Vec<RemoteEntry>,
        files: BTreeMap<PathBuf, Vec<u8>>,
        fail_once: Mutex<bool>,
        fail_on_reads: Mutex<BTreeSet<usize>>,
        read_counter: Mutex<usize>,
        stat_sequence: Mutex<Vec<RemoteFileStat>>,
    }

    impl MockRemote {
        fn new(entries: Vec<RemoteEntry>, files: BTreeMap<PathBuf, Vec<u8>>) -> Self {
            Self {
                entries,
                files,
                fail_once: Mutex::new(false),
                fail_on_reads: Mutex::new(BTreeSet::new()),
                read_counter: Mutex::new(0),
                stat_sequence: Mutex::new(Vec::new()),
            }
        }

        fn with_one_failure(self) -> Self {
            *self.fail_once.lock().expect("lock") = true;
            self
        }

        fn with_fail_on_read(self, read_number: usize) -> Self {
            self.fail_on_reads.lock().expect("lock").insert(read_number);
            self
        }

        fn with_stat_sequence(self, stats: Vec<RemoteFileStat>) -> Self {
            *self.stat_sequence.lock().expect("lock") = stats;
            self
        }
    }

    impl RemoteClient for MockRemote {
        fn list_entries(&self, _recursive: bool) -> Result<Vec<RemoteEntry>> {
            Ok(self.entries.clone())
        }

        fn read_range(&self, relative_path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
            let mut read_counter = self.read_counter.lock().expect("lock");
            *read_counter += 1;
            let this_read = *read_counter;
            drop(read_counter);

            if self.fail_on_reads.lock().expect("lock").remove(&this_read) {
                return Err(anyhow!("forced read failure on read {this_read}"));
            }

            let mut fail = self.fail_once.lock().expect("lock");
            if *fail {
                *fail = false;
                return Err(anyhow!("transient error"));
            }
            let data = self
                .files
                .get(relative_path)
                .ok_or_else(|| anyhow!("missing file"))?;
            let start = offset as usize;
            let end = (offset + len) as usize;
            Ok(data[start..end].to_vec())
        }

        fn stat_file(&self, relative_path: &Path) -> Result<RemoteFileStat> {
            let mut sequence = self.stat_sequence.lock().expect("lock");
            if !sequence.is_empty() {
                return Ok(sequence.remove(0));
            }

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

    fn opts() -> SyncOptions {
        SyncOptions {
            verbose: false,
            progress: false,
            recursive: true,
            links: true,
            update: false,
            preserve_perms: false,
            preserve_owner: false,
            preserve_group: false,
            preserve_acls: false,
            preserve_xattrs: false,
            jobs: 4,
            chunk_size: 8,
            chunk_threshold: 8,
            retries: 2,
            resume: true,
            dry_run: false,
            state_root: None,
        }
    }

    #[test]
    fn downloads_file() {
        let dir = TempDir::new().expect("tmp");
        let entry = RemoteEntry {
            relative_path: PathBuf::from("a.txt"),
            kind: EntryKind::File,
            size: 11,
            mtime_secs: 1700000000,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        };
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("a.txt"), b"hello world".to_vec());

        let remote = MockRemote::new(vec![entry], files);
        let summary = run_sync_with_client(&remote, dir.path(), &opts()).expect("sync");

        assert_eq!(summary.transferred_files, 1);
        assert_eq!(
            fs::read(dir.path().join("a.txt")).expect("read"),
            b"hello world"
        );
    }

    #[test]
    fn resumes_from_existing_partial() {
        let dir = TempDir::new().expect("tmp");
        let data = b"abcdefghijklmnopqrstuvwxyz".to_vec();
        let entry = RemoteEntry {
            relative_path: PathBuf::from("b.bin"),
            kind: EntryKind::File,
            size: data.len() as u64,
            mtime_secs: 1700000001,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        };

        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("b.bin"), data.clone());

        let remote = MockRemote::new(vec![entry.clone()], files.clone());
        let mut options = opts();
        options.chunk_size = 5;
        options.chunk_threshold = 1;

        run_sync_with_client(&remote, dir.path(), &options).expect("first run");
        let summary = run_sync_with_client(&remote, dir.path(), &options).expect("second run");

        assert_eq!(summary.transferred_files, 0);
        assert_eq!(summary.skipped_files, 1);
        assert_eq!(fs::read(dir.path().join("b.bin")).expect("read"), data);
    }

    #[test]
    fn resumes_after_interrupted_run() {
        let dir = TempDir::new().expect("tmp");
        let data = b"abcdefghijklmnopqrstuvwxyz".to_vec();
        let entry = RemoteEntry {
            relative_path: PathBuf::from("resume.bin"),
            kind: EntryKind::File,
            size: data.len() as u64,
            mtime_secs: 1700000008,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        };
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("resume.bin"), data.clone());

        let mut options = opts();
        options.chunk_size = 5;
        options.chunk_threshold = 1;
        options.retries = 1;

        let remote_fail = MockRemote::new(vec![entry.clone()], files.clone()).with_fail_on_read(2);
        let first = run_sync_with_client(&remote_fail, dir.path(), &options);
        assert!(first.is_err());

        let remote_ok = MockRemote::new(vec![entry], files);
        let second = run_sync_with_client(&remote_ok, dir.path(), &options).expect("resume run");
        assert_eq!(second.transferred_files, 1);
        assert_eq!(fs::read(dir.path().join("resume.bin")).expect("read"), data);
    }

    #[test]
    fn retries_transient_chunk_error() {
        let dir = TempDir::new().expect("tmp");
        let entry = RemoteEntry {
            relative_path: PathBuf::from("c.txt"),
            kind: EntryKind::File,
            size: 6,
            mtime_secs: 1700000002,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        };
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("c.txt"), b"123456".to_vec());

        let remote = MockRemote::new(vec![entry], files).with_one_failure();
        let summary = run_sync_with_client(&remote, dir.path(), &opts()).expect("sync");

        assert_eq!(summary.transferred_files, 1);
        assert_eq!(fs::read(dir.path().join("c.txt")).expect("read"), b"123456");
    }

    #[test]
    fn retries_if_remote_changes_mid_transfer() {
        let dir = TempDir::new().expect("tmp");
        let entry = RemoteEntry {
            relative_path: PathBuf::from("changing.bin"),
            kind: EntryKind::File,
            size: 10,
            mtime_secs: 1_700_001_000,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        };
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("changing.bin"), b"0123456789".to_vec());

        let remote = MockRemote::new(vec![entry], files).with_stat_sequence(vec![
            RemoteFileStat {
                size: 10,
                mtime_secs: 1_700_001_001,
            },
            RemoteFileStat {
                size: 10,
                mtime_secs: 1_700_001_000,
            },
        ]);
        let mut options = opts();
        options.chunk_size = 4;
        options.chunk_threshold = 1;

        let summary = run_sync_with_client(&remote, dir.path(), &options).expect("sync");
        assert_eq!(summary.transferred_files, 1);
        assert_eq!(
            fs::read(dir.path().join("changing.bin")).expect("read"),
            b"0123456789"
        );
    }

    #[test]
    fn dry_run_does_not_write_files() {
        let dir = TempDir::new().expect("tmp");
        let entry = RemoteEntry {
            relative_path: PathBuf::from("dry.txt"),
            kind: EntryKind::File,
            size: 5,
            mtime_secs: 1700000003,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        };
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("dry.txt"), b"hello".to_vec());
        let remote = MockRemote::new(vec![entry], files);

        let mut options = opts();
        options.dry_run = true;
        let summary = run_sync_with_client(&remote, dir.path(), &options).expect("sync");
        assert_eq!(summary.transferred_files, 1);
        assert!(!dir.path().join("dry.txt").exists());
    }

    #[test]
    fn update_flag_skips_newer_local() {
        let dir = TempDir::new().expect("tmp");
        let local_path = dir.path().join("d.txt");
        fs::write(&local_path, b"local").expect("write local");
        let newer = filetime::FileTime::from_unix_time(2_000_000_000, 0);
        filetime::set_file_mtime(&local_path, newer).expect("set mtime");

        let entry = RemoteEntry {
            relative_path: PathBuf::from("d.txt"),
            kind: EntryKind::File,
            size: 6,
            mtime_secs: 1_000_000_000,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        };
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("d.txt"), b"remote".to_vec());
        let remote = MockRemote::new(vec![entry], files);

        let mut options = opts();
        options.update = true;
        let summary = run_sync_with_client(&remote, dir.path(), &options).expect("sync");

        assert_eq!(summary.transferred_files, 0);
        assert_eq!(summary.skipped_files, 1);
        assert_eq!(fs::read(local_path).expect("read"), b"local");
    }

    #[test]
    fn rejects_parent_traversal_paths() {
        let dir = TempDir::new().expect("tmp");
        let entry = RemoteEntry {
            relative_path: PathBuf::from("../escape.txt"),
            kind: EntryKind::File,
            size: 4,
            mtime_secs: 1700000004,
            mode: 0o644,
            uid: None,
            gid: None,
            link_target: None,
        };
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("../escape.txt"), b"evil".to_vec());
        let remote = MockRemote::new(vec![entry], files);
        let err = run_sync_with_client(&remote, dir.path(), &opts()).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("unsafe remote path component"));
    }

    #[cfg(unix)]
    #[test]
    fn preserves_symlink_when_links_enabled() {
        let dir = TempDir::new().expect("tmp");
        let entry = RemoteEntry {
            relative_path: PathBuf::from("link.txt"),
            kind: EntryKind::Symlink,
            size: 0,
            mtime_secs: 1_700_000_000,
            mode: 0o777,
            uid: None,
            gid: None,
            link_target: Some(PathBuf::from("target.txt")),
        };

        let remote = MockRemote::new(vec![entry], BTreeMap::new());
        run_sync_with_client(&remote, dir.path(), &opts()).expect("sync");

        let link_path = dir.path().join("link.txt");
        let target = fs::read_link(link_path).expect("read link");
        assert_eq!(target, PathBuf::from("target.txt"));
    }
}
