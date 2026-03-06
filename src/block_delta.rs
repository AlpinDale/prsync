//! Block-level delta sync for S3 sources.
//!
//! Instead of SSH-exec-based rolling checksums, this module uses local BLAKE3
//! hashing + SQLite block map + S3 range reads to transfer only changed blocks.
//! Same pattern as Restic / Dropbox.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result};
use blake3;
use rayon::prelude::*;

use crate::remote::RemoteClient;
use crate::state::StateStore;

/// Default block size for block-level delta: 5 MiB.
pub const DEFAULT_BLOCK_SIZE: u32 = 5 * 1024 * 1024;

/// Default minimum file size to enable block-delta (100 MiB).
pub const DEFAULT_DELTA_MIN_SIZE: u64 = 100 * 1024 * 1024;

/// If more than this fraction of blocks changed, fall back to full download.
const CHANGE_DENSITY_THRESHOLD: f64 = 0.70;

/// Result of a block-delta transfer attempt.
pub enum BlockDeltaResult {
    /// Only changed blocks were downloaded.
    Partial {
        blocks_downloaded: u64,
        blocks_skipped: u64,
        bytes_downloaded: u64,
    },
    /// Fell back to full download (too many changes or first sync).
    FullDownload,
    /// File is too small for block-delta; caller should do full transfer.
    TooSmall,
}

/// Compute the BLAKE3 hash of a single block from a local file.
fn hash_local_block(path: &Path, block_index: u64, block_size: u32) -> Result<String> {
    let mut f = File::open(path)
        .with_context(|| format!("open local file for hashing: {}", path.display()))?;
    let offset = block_index * block_size as u64;
    f.seek(SeekFrom::Start(offset))?;

    let file_size = f.metadata()?.len();
    let remaining = file_size.saturating_sub(offset);
    let to_read = remaining.min(block_size as u64) as usize;

    let mut buf = vec![0u8; to_read];
    f.read_exact(&mut buf)?;
    Ok(blake3::hash(&buf).to_hex().to_string())
}

/// Attempt a block-level delta transfer for a single file.
///
/// Returns `BlockDeltaResult` indicating what happened. The caller is
/// responsible for setting mtimes, permissions, etc.
pub fn transfer_block_delta<R: RemoteClient + Sync>(
    remote: &R,
    relative_path: &Path,
    local_path: &Path,
    state: &StateStore,
    block_size: u32,
    min_size: u64,
) -> Result<BlockDeltaResult> {
    // Check remote file size.
    let remote_stat = remote.stat_file(relative_path)?;
    if remote_stat.size < min_size {
        return Ok(BlockDeltaResult::TooSmall);
    }

    // If local file doesn't exist or size differs, can't do block-delta.
    let local_meta = match fs::metadata(local_path) {
        Ok(m) => m,
        Err(_) => return Ok(BlockDeltaResult::FullDownload),
    };
    if local_meta.len() != remote_stat.size {
        // Size changed — full download is simpler and often faster.
        state.clear_blocks(relative_path)?;
        return Ok(BlockDeltaResult::FullDownload);
    }

    let total_blocks =
        (remote_stat.size + block_size as u64 - 1) / block_size as u64;

    // Hash local blocks in parallel.
    let local_hashes: Vec<Result<String>> = (0..total_blocks)
        .into_par_iter()
        .map(|i| hash_local_block(local_path, i, block_size))
        .collect();

    // Check for hashing errors.
    let local_hashes: Vec<String> = local_hashes
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .context("hashing local blocks")?;

    // Compare with stored hashes in SQLite.
    let mut changed_blocks: Vec<u64> = Vec::new();
    for (i, hash) in local_hashes.iter().enumerate() {
        if !state.block_matches(relative_path, i as u64, hash)? {
            changed_blocks.push(i as u64);
        }
    }

    // If no blocks changed, file is up-to-date.
    if changed_blocks.is_empty() {
        return Ok(BlockDeltaResult::Partial {
            blocks_downloaded: 0,
            blocks_skipped: total_blocks,
            bytes_downloaded: 0,
        });
    }

    // Change density check.
    let change_ratio = changed_blocks.len() as f64 / total_blocks as f64;
    if change_ratio > CHANGE_DENSITY_THRESHOLD {
        state.clear_blocks(relative_path)?;
        return Ok(BlockDeltaResult::FullDownload);
    }

    // Download only changed blocks in parallel.
    let results: Vec<Result<(u64, Vec<u8>)>> = changed_blocks
        .par_iter()
        .map(|&block_idx| {
            let offset = block_idx * block_size as u64;
            let len = std::cmp::min(
                block_size as u64,
                remote_stat.size - offset,
            );
            let data = remote.read_range(relative_path, offset, len)?;
            Ok((block_idx, data))
        })
        .collect();

    // Write downloaded blocks to the local file.
    let mut file = OpenOptions::new()
        .write(true)
        .open(local_path)
        .with_context(|| format!("open local file for block writes: {}", local_path.display()))?;

    let mut bytes_downloaded: u64 = 0;
    for result in results {
        let (block_idx, data) = result?;
        let offset = block_idx * block_size as u64;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&data)?;
        bytes_downloaded += data.len() as u64;
    }
    file.flush()?;

    // Update SQLite with hashes for all blocks (not just changed ones).
    // Re-hash the changed blocks since we wrote new data.
    for (i, hash) in local_hashes.iter().enumerate() {
        let block_idx = i as u64;
        let final_hash = if changed_blocks.contains(&block_idx) {
            // Re-hash the block we just wrote.
            hash_local_block(local_path, block_idx, block_size)?
        } else {
            hash.clone()
        };
        state.upsert_block(relative_path, block_idx, &final_hash, block_size, "")?;
    }

    let blocks_skipped = total_blocks - changed_blocks.len() as u64;
    Ok(BlockDeltaResult::Partial {
        blocks_downloaded: changed_blocks.len() as u64,
        blocks_skipped,
        bytes_downloaded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn hash_local_block_works() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let data = vec![0xABu8; 1024];
        fs::write(&path, &data).unwrap();

        let hash = hash_local_block(&path, 0, 1024).unwrap();
        let expected = blake3::hash(&data).to_hex().to_string();
        assert_eq!(hash, expected);
    }

    #[test]
    fn hash_local_block_partial_last() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        // 1500 bytes with block_size 1024 → block 0 is 1024, block 1 is 476
        let data = vec![0x42u8; 1500];
        fs::write(&path, &data).unwrap();

        let hash0 = hash_local_block(&path, 0, 1024).unwrap();
        let hash1 = hash_local_block(&path, 1, 1024).unwrap();

        assert_eq!(hash0, blake3::hash(&data[..1024]).to_hex().to_string());
        assert_eq!(hash1, blake3::hash(&data[1024..]).to_hex().to_string());
    }

    #[test]
    fn block_db_round_trip() {
        let dir = TempDir::new().unwrap();
        let state_root = dir.path().join(".parsync");
        let state = StateStore::load(&state_root).unwrap();

        let path = Path::new("big/file.bin");
        let hash = "abcdef1234567890";

        // Initially no match.
        assert!(!state.block_matches(path, 0, hash).unwrap());

        // Upsert and verify match.
        state.upsert_block(path, 0, hash, 5242880, "etag-1").unwrap();
        assert!(state.block_matches(path, 0, hash).unwrap());

        // Different hash doesn't match.
        assert!(!state.block_matches(path, 0, "different").unwrap());

        // Clear blocks.
        state.clear_blocks(path).unwrap();
        assert!(!state.block_matches(path, 0, hash).unwrap());
    }
}
