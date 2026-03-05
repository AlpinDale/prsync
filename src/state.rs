use std::{
    collections::BTreeSet,
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Debug, Clone, Default)]
pub struct FileState {
    pub remote_size: u64,
    pub remote_mtime_secs: i64,
    pub chunk_size: u64,
    pub completed_chunks: BTreeSet<u64>,
    pub digest_hex: Option<String>,
    pub finished: bool,
    pub part_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct DeltaSessionState {
    pub basis_digest_hex: String,
    pub source_size: u64,
    pub source_mtime_secs: i64,
    pub block_size: u32,
    pub protocol_version: u32,
    pub finished: bool,
    pub last_op_index: u64,
}

#[derive(Debug)]
pub struct StateStore {
    conn: Connection,
    partials_dir: PathBuf,
}

#[derive(Debug)]
pub struct DestinationLock {
    lock_path: PathBuf,
    _file: File,
}

impl Drop for DestinationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

pub fn acquire_destination_lock(state_root: &Path) -> Result<DestinationLock> {
    fs::create_dir_all(state_root)
        .with_context(|| format!("create lock root: {}", state_root.display()))?;

    let lock_path = state_root.join("lock");
    let file = match open_new_lock(&lock_path) {
        Ok(file) => file,
        Err(initial_err) => {
            if !clear_stale_lock_if_any(&lock_path)? {
                return Err(initial_err);
            }
            open_new_lock(&lock_path)?
        }
    };

    write_lock_metadata(&file)?;

    Ok(DestinationLock {
        lock_path,
        _file: file,
    })
}

fn open_new_lock(lock_path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(lock_path)
        .with_context(|| {
            format!(
                "acquire destination lock failed (another prsync may be running): {}",
                lock_path.display()
            )
        })
}

fn write_lock_metadata(file: &File) -> Result<()> {
    let mut file = file;
    writeln!(&mut file, "pid={}", std::process::id()).context("write lock metadata")?;
    Ok(())
}

fn clear_stale_lock_if_any(lock_path: &Path) -> Result<bool> {
    let content = match fs::read_to_string(lock_path) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    let pid = parse_lock_pid(&content);
    let Some(pid) = pid else {
        return Ok(false);
    };
    if process_is_alive(pid) {
        return Ok(false);
    }
    fs::remove_file(lock_path)
        .with_context(|| format!("remove stale lock file: {}", lock_path.display()))?;
    Ok(true)
}

fn parse_lock_pid(content: &str) -> Option<u32> {
    for line in content.lines() {
        if let Some(value) = line.strip_prefix("pid=") {
            if let Ok(pid) = value.trim().parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

    match kill(Pid::from_raw(pid as i32), None) {
        Ok(_) => true,
        Err(Errno::EPERM) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

impl StateStore {
    pub fn load(state_root: &Path) -> Result<Self> {
        fs::create_dir_all(state_root)
            .with_context(|| format!("create state root: {}", state_root.display()))?;
        let partials_dir = state_root.join("partials");
        fs::create_dir_all(&partials_dir)
            .with_context(|| format!("create partials dir: {}", partials_dir.display()))?;

        let db_path = state_root.join("state.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open state db: {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        migrate(&conn)?;

        Ok(Self { conn, partials_dir })
    }

    pub fn save(&self) -> Result<()> {
        Ok(())
    }

    pub fn clear_all(&self) -> Result<()> {
        self.conn.execute("DELETE FROM file_chunks", [])?;
        self.conn.execute("DELETE FROM files", [])?;
        self.conn.execute("DELETE FROM delta_sessions", [])?;
        Ok(())
    }

    pub fn key_for(path: &Path) -> String {
        path.components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub fn file_state(&self, rel: &Path) -> Result<Option<FileState>> {
        let key = Self::key_for(rel);
        let row = self
            .conn
            .query_row(
                "SELECT remote_size, remote_mtime_secs, chunk_size, digest_hex, finished, part_name FROM files WHERE path_key = ?1",
                params![key],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;

        let Some((remote_size, remote_mtime_secs, chunk_size, digest_hex, finished, part_name)) =
            row
        else {
            return Ok(None);
        };

        let mut completed = BTreeSet::new();
        let mut stmt = self.conn.prepare(
            "SELECT chunk_idx FROM file_chunks WHERE path_key = ?1 ORDER BY chunk_idx ASC",
        )?;
        let rows = stmt.query_map(params![key], |r| r.get::<_, i64>(0))?;
        for row in rows {
            let idx = row?;
            completed.insert(idx as u64);
        }

        Ok(Some(FileState {
            remote_size: remote_size as u64,
            remote_mtime_secs,
            chunk_size: chunk_size as u64,
            completed_chunks: completed,
            digest_hex,
            finished: finished != 0,
            part_name,
        }))
    }

    pub fn upsert_file(
        &self,
        rel: &Path,
        remote_size: u64,
        remote_mtime_secs: i64,
        chunk_size: u64,
    ) -> Result<()> {
        let key = Self::key_for(rel);
        let part_name = part_name_for(&key);

        let existing = self.file_state(rel)?;
        let changed = existing
            .as_ref()
            .map(|e| {
                e.remote_size != remote_size
                    || e.remote_mtime_secs != remote_mtime_secs
                    || e.chunk_size != chunk_size
            })
            .unwrap_or(true);

        self.conn.execute(
            "INSERT INTO files(path_key, remote_size, remote_mtime_secs, chunk_size, digest_hex, finished, part_name)
             VALUES(?1, ?2, ?3, ?4, NULL, 0, ?5)
             ON CONFLICT(path_key) DO UPDATE SET remote_size = excluded.remote_size, remote_mtime_secs = excluded.remote_mtime_secs, chunk_size = excluded.chunk_size, part_name = excluded.part_name",
            params![key, remote_size as i64, remote_mtime_secs, chunk_size as i64, part_name],
        )?;

        if changed {
            self.conn.execute(
                "DELETE FROM file_chunks WHERE path_key = ?1",
                params![Self::key_for(rel)],
            )?;
            self.conn.execute(
                "UPDATE files SET digest_hex = NULL, finished = 0 WHERE path_key = ?1",
                params![Self::key_for(rel)],
            )?;
        }

        Ok(())
    }

    pub fn mark_chunk_completed(&self, rel: &Path, idx: u64) -> Result<()> {
        let key = Self::key_for(rel);
        self.conn.execute(
            "INSERT OR IGNORE INTO file_chunks(path_key, chunk_idx) VALUES(?1, ?2)",
            params![key, idx as i64],
        )?;
        Ok(())
    }

    pub fn mark_chunks_completed_batch(&mut self, rel: &Path, chunks: &[u64]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let key = Self::key_for(rel);
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx
                .prepare("INSERT OR IGNORE INTO file_chunks(path_key, chunk_idx) VALUES(?1, ?2)")?;
            for idx in chunks {
                stmt.execute(params![&key, *idx as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn part_path_for(&self, rel: &Path) -> PathBuf {
        let key = Self::key_for(rel);
        self.partials_dir.join(part_name_for(&key))
    }

    pub fn reset_progress(&self, rel: &Path) -> Result<()> {
        let key = Self::key_for(rel);
        self.conn.execute(
            "DELETE FROM file_chunks WHERE path_key = ?1",
            params![key.clone()],
        )?;
        self.conn.execute(
            "UPDATE files SET finished = 0, digest_hex = NULL WHERE path_key = ?1",
            params![key],
        )?;
        Ok(())
    }

    pub fn mark_finished_with_digest(&self, rel: &Path, digest_hex: String) -> Result<()> {
        let key = Self::key_for(rel);
        self.conn.execute(
            "UPDATE files SET finished = 1, digest_hex = ?2 WHERE path_key = ?1",
            params![key, digest_hex],
        )?;
        Ok(())
    }

    pub fn mark_finished(&self, rel: &Path) -> Result<()> {
        let key = Self::key_for(rel);
        self.conn.execute(
            "UPDATE files SET finished = 1, digest_hex = NULL WHERE path_key = ?1",
            params![key],
        )?;
        Ok(())
    }

    pub fn prune_to_keys(&self, valid_keys: &HashSet<String>) -> Result<()> {
        let mut stmt = self.conn.prepare("SELECT path_key FROM files")?;
        let keys = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for key_row in keys {
            let key = key_row?;
            if !valid_keys.contains(&key) {
                self.conn
                    .execute("DELETE FROM files WHERE path_key = ?1", params![key])?;
                self.conn.execute(
                    "DELETE FROM delta_sessions WHERE path_key = ?1",
                    params![key],
                )?;
            }
        }

        let expected_partials: HashSet<String> =
            valid_keys.iter().map(|k| part_name_for(k)).collect();
        for entry in fs::read_dir(&self.partials_dir)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().to_string();
            if !expected_partials.contains(&file_name) {
                let _ = fs::remove_file(entry.path());
            }
        }

        Ok(())
    }

    pub fn upsert_delta_session(
        &self,
        rel: &Path,
        basis_digest_hex: &str,
        source_size: u64,
        source_mtime_secs: i64,
        block_size: u32,
    ) -> Result<()> {
        let key = Self::key_for(rel);
        self.conn.execute(
            "INSERT INTO delta_sessions(path_key, basis_digest_hex, source_size, source_mtime_secs, block_size, protocol_version, finished, last_op_index)
             VALUES(?1, ?2, ?3, ?4, ?5, 1, 0, 0)
             ON CONFLICT(path_key) DO UPDATE
               SET basis_digest_hex=excluded.basis_digest_hex,
                   source_size=excluded.source_size,
                   source_mtime_secs=excluded.source_mtime_secs,
                   block_size=excluded.block_size,
                   protocol_version=1,
                   finished=0,
                   last_op_index=0",
            params![
                key,
                basis_digest_hex,
                source_size as i64,
                source_mtime_secs,
                block_size as i64
            ],
        )?;
        Ok(())
    }

    pub fn delta_session(&self, rel: &Path) -> Result<Option<DeltaSessionState>> {
        let key = Self::key_for(rel);
        self.conn
            .query_row(
                "SELECT basis_digest_hex, source_size, source_mtime_secs, block_size, protocol_version, finished, last_op_index
                 FROM delta_sessions WHERE path_key = ?1",
                params![key],
                |r| {
                    Ok(DeltaSessionState {
                        basis_digest_hex: r.get::<_, String>(0)?,
                        source_size: r.get::<_, i64>(1)? as u64,
                        source_mtime_secs: r.get::<_, i64>(2)?,
                        block_size: r.get::<_, i64>(3)? as u32,
                        protocol_version: r.get::<_, i64>(4)? as u32,
                        finished: r.get::<_, i64>(5)? != 0,
                        last_op_index: r.get::<_, i64>(6)? as u64,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn mark_delta_op_progress(&self, rel: &Path, last_op_index: u64) -> Result<()> {
        let key = Self::key_for(rel);
        self.conn.execute(
            "UPDATE delta_sessions SET last_op_index = ?2 WHERE path_key = ?1",
            params![key, last_op_index as i64],
        )?;
        Ok(())
    }

    pub fn mark_delta_finished(&self, rel: &Path) -> Result<()> {
        let key = Self::key_for(rel);
        self.conn.execute(
            "UPDATE delta_sessions SET finished = 1 WHERE path_key = ?1",
            params![key],
        )?;
        Ok(())
    }

    pub fn clear_delta_session(&self, rel: &Path) -> Result<()> {
        let key = Self::key_for(rel);
        self.conn.execute(
            "DELETE FROM delta_sessions WHERE path_key = ?1",
            params![key],
        )?;
        Ok(())
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY
        );

        CREATE TABLE IF NOT EXISTS files (
            path_key TEXT PRIMARY KEY,
            remote_size INTEGER NOT NULL,
            remote_mtime_secs INTEGER NOT NULL,
            chunk_size INTEGER NOT NULL,
            digest_hex TEXT,
            finished INTEGER NOT NULL,
            part_name TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS file_chunks (
            path_key TEXT NOT NULL,
            chunk_idx INTEGER NOT NULL,
            PRIMARY KEY(path_key, chunk_idx),
            FOREIGN KEY(path_key) REFERENCES files(path_key) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS delta_sessions (
            path_key TEXT PRIMARY KEY,
            basis_digest_hex TEXT NOT NULL,
            source_size INTEGER NOT NULL,
            source_mtime_secs INTEGER NOT NULL,
            block_size INTEGER NOT NULL,
            protocol_version INTEGER NOT NULL,
            finished INTEGER NOT NULL,
            last_op_index INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn part_name_for(key: &str) -> String {
    let digest = xxh3_64(key.as_bytes());
    format!("{digest:016x}.part")
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, path::Path};

    use tempfile::TempDir;

    use super::{acquire_destination_lock, StateStore};

    #[test]
    fn state_round_trip() {
        let dir = TempDir::new().expect("tmp");
        let state_root = dir.path().join(".prsync");
        let state = StateStore::load(&state_root).expect("load");
        state
            .upsert_file(Path::new("a/b.txt"), 10, 123, 1024)
            .expect("upsert");
        state
            .mark_chunk_completed(Path::new("a/b.txt"), 0)
            .expect("mark chunk");

        let state2 = StateStore::load(&state_root).expect("reload");
        let fs = state2
            .file_state(Path::new("a/b.txt"))
            .expect("load state")
            .expect("exists");
        assert_eq!(fs.remote_size, 10);
        assert!(fs.completed_chunks.contains(&0));
    }

    #[test]
    fn destination_lock_is_exclusive() {
        let dir = TempDir::new().expect("tmp");
        let state_root = dir.path().join(".prsync");
        let _lock = acquire_destination_lock(&state_root).expect("first lock");
        let second = acquire_destination_lock(&state_root);
        assert!(second.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stale_destination_lock_is_recovered() {
        let dir = TempDir::new().expect("tmp");
        let state_root = dir.path().join(".prsync");
        fs::create_dir_all(&state_root).expect("mkdir");
        let lock_path = state_root.join("lock");
        fs::write(&lock_path, "pid=999999\n").expect("write lock");

        let lock = acquire_destination_lock(&state_root).expect("recovered lock");
        drop(lock);
        assert!(!lock_path.exists());
    }

    #[test]
    fn prune_removes_orphan_state_and_partials() {
        let dir = TempDir::new().expect("tmp");
        let state_root = dir.path().join(".prsync");
        let state = StateStore::load(&state_root).expect("load");
        state
            .upsert_file(Path::new("keep.txt"), 1, 1, 1)
            .expect("upsert keep");
        state
            .upsert_file(Path::new("drop.txt"), 1, 1, 1)
            .expect("upsert drop");

        let drop_part = state.part_path_for(Path::new("drop.txt"));
        fs::write(&drop_part, b"x").expect("write part");

        let mut valid = HashSet::new();
        valid.insert(StateStore::key_for(Path::new("keep.txt")));
        state.prune_to_keys(&valid).expect("prune");

        assert!(state
            .file_state(Path::new("keep.txt"))
            .expect("state")
            .is_some());
        assert!(state
            .file_state(Path::new("drop.txt"))
            .expect("state")
            .is_none());
        assert!(!drop_part.exists());
    }
}
