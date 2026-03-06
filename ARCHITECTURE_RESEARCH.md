# Parsync Architecture: How It Outperforms rsync

## Executive Summary

Parsync is a ~2,500 LOC Rust tool that achieves **~686% faster** performance than rsync (and ~61% faster than rclone) when transferring many small files. It does this through a fundamentally different architecture that eliminates rsync's two biggest bottlenecks: **serial file processing** and **mandatory remote-side installation**.

## The Core Architectural Differences

### 1. Parallel SSH Connection Pool (the primary speedup)

**rsync**: Uses a single SSH connection with a single-threaded pipeline. Every file is listed, checksummed, and transferred sequentially over one connection. For 100,000 small files, this means 100,000 sequential round-trips.

**parsync**: Creates a **pool of N independent SSH/SFTP sessions** (default: `2 * CPU_COUNT`, clamped to 4-32), then uses **rayon's work-stealing thread pool** to transfer files in parallel across all connections.

From `remote.rs:900-960` — the `ConnectionPool`:
- Eagerly opens 1 connection for fast-fail auth validation
- Lazily grows up to `pool_size` connections as workers demand them
- Uses `Mutex<PoolState> + Condvar` for checkout/return semantics
- Each `Connection` holds its own `Session + Sftp` handles with a **cached open file handle** (`open_read_path`/`open_read_file`) to avoid reopening the same file across chunk reads

From `sync.rs:379-422` — the transfer loop:
```rust
let pool = rayon::ThreadPoolBuilder::new()
    .num_threads(options.jobs)
    .build()?;
pool.install(|| {
    jobs.par_iter().for_each(|job| {
        transfer_one(remote, job, options, &state, &ui, &perf, &warnings)
    });
});
```

This is the **single biggest factor** for the small-file benchmark. rsync's serial model means each small file pays the full SSH round-trip latency. Parsync amortizes that latency across N concurrent connections.

### 2. Pull-Only, No Remote Installation Required

**rsync**: Requires rsync installed on **both** machines. Uses its own wire protocol between the two rsync processes.

**parsync**: Operates as a **pull-only client**. It only needs SSH access to the remote — no special software. It uses standard SFTP operations (`readdir`, `lstat`, `open`, `read`, `seek`) over `libssh2`. This is the "easier to setup" claim.

For directory listing, it has a two-tier strategy (`remote.rs:406-424`):
1. **Fast path**: `ssh exec("find . -printf '...\0'")` — streams a null-delimited listing over a single SSH channel, parsing entries as they arrive (streaming, not buffered). This avoids per-file `lstat` RTTs entirely.
2. **Fallback**: SFTP `readdir()` walk if `find -printf` isn't available (e.g., macOS/busybox). Uses `readdir` attributes directly to avoid extra `lstat` calls where possible.

### 3. Chunked Transfer with Per-Chunk Resume State

**rsync**: Resume semantics require `--partial` and restart the whole file.

**parsync**: Large files (>= `chunk_threshold`, default 64MB) are split into chunks (default 8MB). Each chunk is independently tracked in a **SQLite state database** (`state.rs`):
- `files` table: tracks `path_key`, `remote_size`, `remote_mtime_secs`, `chunk_size`, `finished`
- `file_chunks` table: tracks which chunks are completed (`path_key, chunk_idx`)
- Uses WAL journal mode + NORMAL synchronous for performance
- On resume, only missing chunks are re-transferred

For large files, there's also **intra-file parallelism** (`sync.rs:620-705`): files >= 32MB get `sftp_read_concurrency` (default 4) parallel chunk reads within a single file, using `par_iter()` over chunk groups.

### 4. Delta Transfer (rsync-algorithm Reimplementation)

Parsync has an optional `--delta` mode that implements the **rsync rolling-checksum algorithm** from scratch:

- **`checksum.rs`**: Adler32-style rolling checksum (weak hash) + MD5 (strong hash), matching rsync's classic two-level approach
- **`signature.rs`**: Builds a file signature by chunking the local "basis" file into blocks and computing weak+strong hashes. Adaptive block sizes: 32KB for <64MB files, 64KB for <1GB, 128KB for larger.
- **`matcher.rs`**: The delta engine. Uses a HashMap lookup on weak checksums, then verifies with strong hash. Emits `Copy{block_index, len}` or `Literal{data_b64}` operations.
- **`patch.rs`**: Applies delta ops using `pread`-style random access on the basis file.

The clever part: the delta computation runs **on the remote machine** via either:
1. `parsync --internal-remote-helper` (if parsync is installed remotely) — the binary has a hidden `--internal-remote-helper` flag that reads a JSON request from stdin and writes the delta plan to stdout
2. An **embedded Python3 fallback** (`remote.rs:343-402`) — a complete reimplementation of the delta algorithm in an inline Python script, passed via SSH exec with the request in a base64 env var. This means delta works even without parsync installed remotely.

### 5. Fast Hashing with xxHash

**rsync**: Uses MD5/MD4 for file checksums.

**parsync**: Uses **xxHash (xxh3_128)** for file integrity verification and state tracking. xxh3 is ~10-20x faster than MD5 for bulk data. MD5 is only used in the delta subsystem (where it needs to match rsync's algorithm semantics).

### 6. Write-to-Partial-then-Rename Pattern

All file writes go to a `.part` file in the state directory first, then atomically `rename()` to the final destination. This prevents partial/corrupt files at the destination. The part file name is an xxh3 hash of the path key, avoiding filesystem issues with deeply nested paths.

### 7. TCP_NODELAY on SSH Connections

`remote.rs:634`: `tcp.set_nodelay(true)` — disables Nagle's algorithm on every SSH connection. For many small operations (which is the exact benchmark case), this eliminates 40ms+ delays from TCP buffering.

## Why It's Fast for Many Small Files (Specifically)

The **686% rsync speedup** on small files comes from the multiplicative effect of:

| Factor | rsync | parsync | Speedup contribution |
|--------|-------|---------|---------------------|
| Connections | 1 serial | N parallel (default 8-32) | **8-32x** potential |
| Listing | Per-file stat RTT | Batch `find -printf` stream | **Eliminates per-file RTT** |
| Hashing | MD5 | xxh3 | ~10x for integrity checks |
| TCP | Default (Nagle) | NODELAY | Reduces small-packet latency |
| File handles | Open/close per file | Cached per connection | Reduces syscall overhead |

For large files, the advantage narrows (network bandwidth dominates), which is why the rclone comparison is only ~61% — rclone already does parallel transfers.

## Architecture Diagram

```
┌─────────────────────────────────────────────────────┐
│                   parsync (local)                    │
│                                                      │
│  main.rs ──► cli.rs ──► config.rs                   │
│       │                                              │
│       ▼                                              │
│  sync::run_sync()                                    │
│       │                                              │
│       ├─ 1. Connect: ConnectionPool(N sessions)      │
│       │      └─ SSH+SFTP per connection              │
│       │                                              │
│       ├─ 2. List: find -printf stream OR readdir walk│
│       │                                              │
│       ├─ 3. Plan: should_transfer() per file         │
│       │      └─ Check size+mtime, resume state       │
│       │                                              │
│       ├─ 4. Transfer: rayon par_iter over FileJobs   │
│       │      ├─ Small files: single chunk read       │
│       │      ├─ Large files: multi-chunk parallel    │
│       │      └─ Delta files: signature+remote helper │
│       │                                              │
│       └─ 5. Finalize: rename .part → dest, cleanup   │
│                                                      │
│  state.rs ──► SQLite (WAL mode)                      │
│       └─ files, file_chunks, delta_sessions tables   │
│                                                      │
│  delta/ ──► checksum + signature + matcher + patch   │
│       └─ Rolling checksum + MD5 (rsync algorithm)    │
└──────────────────────┬──────────────────────────────┘
                       │ N SSH connections
                       ▼
            ┌──────────────────┐
            │  Remote (SSH)     │
            │  No parsync needed│
            │  SFTP operations  │
            │  OR exec commands │
            └──────────────────┘
```

## Module Map

| File | Purpose | Key Types |
|------|---------|-----------|
| `main.rs` | Entry point, dispatches to sync or remote helper | — |
| `cli.rs` | Clap CLI definition, rsync-compatible flags | `Cli` |
| `config.rs` | 3-tier config: CLI > env > `~/.config/parsync/config.toml` | `ResolvedConfig` |
| `sync.rs` | Core orchestrator: list → plan → parallel transfer → finalize | `run_sync`, `FileJob`, `TransferOutcome` |
| `remote.rs` | SSH/SFTP transport, connection pool, delta helper dispatch | `SshRemote`, `ConnectionPool`, `RemoteClient` trait |
| `remote_helper.rs` | `--internal-remote-helper` stdin/stdout delta mode | `run_stdio()` |
| `state.rs` | SQLite resume state, file-level locking | `StateStore`, `FileState`, `DeltaSessionState` |
| `hashing.rs` | xxh3_128 file hashing | `hash_file`, `hash_bytes` |
| `delta/checksum.rs` | Rolling checksum (Adler32-style) + MD5 strong hash | `RollingChecksum` |
| `delta/signature.rs` | Build block signatures from basis file | `BlockSig`, `FileSignature` |
| `delta/matcher.rs` | Delta computation (rsync algorithm) | `build_delta_ops` |
| `delta/patch.rs` | Apply delta ops to reconstruct file | `apply_delta_ops` |
| `delta/protocol.rs` | JSON wire format for helper communication | `HelperRequest`, `HelperResponse`, `DeltaOp` |
