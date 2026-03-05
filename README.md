# prsync

`prsync` is a high-throughput, resumable pull syncs from SSH remotes. In
essence, a parallelized `rsync` implementation.

![demo](assets/demo.gif)

## Installation
Download the binary for your platform from the [releases page](https://github.com/AlpinDale/prsync/releases).

## Building

Make sure you have Rust stable installed (via rustup), then:

```bash
make build
make install
```

At the moment, only Linux and macOS are supported.

## Usage

```bash
prsync -vrPlu user@example.com:/remote/path /local/destination
```

To specify a non-default SSH port:

```bash
prsync -vrPlu user@example.com:2222:/remote/path /local/destination
```

Reading the hostname from SSH config is also supported.

## Performance tuning

```bash
prsync -vrPlu --jobs 16 --chunk-size 16777216 --chunk-threshold 134217728 user@host:/src /dst
```

Balanced mode defaults:

- No per-file `sync_all` barriers (atomic rename still preserved)
- Existing-file digest checks are skipped unless requested
- Chunk completion state is committed in batches
- Post-transfer remote mutation `stat` check is skipped (enabled in strict mode)

Throughput flags:

- `--strict-durability`: enable fsync-heavy strict mode
- `--verify-existing`: hash existing files before skip decisions
- `--sftp-read-concurrency`: parallel per-file read requests for large files
- `--sftp-read-chunk-size`: read request size for SFTP range pulls

## Delta mode (opt-in)

Enable rsync-class block deltas for large changed files:

```bash
prsync -vrPlu --delta --delta-min-size 8388608 user@host:/src /dst
```

Delta flags:

- `--delta`: enable delta transfer
- `--delta-min-size`: minimum file size eligible for deltas
- `--delta-block-size`: fixed block size (auto if omitted)
- `--delta-max-literals`: fallback to full transfer when unmatched literals exceed threshold
- `--delta-helper`: remote helper command (default `prsync --internal-remote-helper`)
- `--no-delta-fallback`: fail if delta path fails (instead of full-transfer fallback)

Remote helper deployment:

1. Build locally: `cargo build --release --bin prsync`
2. Copy the same `prsync` binary to remote PATH.
3. Delta mode invokes remote helper via `prsync --internal-remote-helper --stdio`.

## Config and precedence

- Optional config file: `~/.config/prsync/config.toml`
- Supported keys: `jobs`, `chunk_size`, `chunk_threshold`, `retries`, `resume`, `state_dir`, `delta_enabled`, `delta_min_size`, `delta_block_size`, `delta_max_literals`, `delta_helper`, `delta_fallback`, `strict_durability`, `verify_existing`, `sftp_read_concurrency`, `sftp_read_chunk_size`
- Environment overrides: `PRSYNC_JOBS`, `PRSYNC_CHUNK_SIZE`, `PRSYNC_CHUNK_THRESHOLD`, `PRSYNC_RETRIES`, `PRSYNC_RESUME`, `PRSYNC_STATE_DIR`, `PRSYNC_DELTA`, `PRSYNC_DELTA_MIN_SIZE`, `PRSYNC_DELTA_BLOCK_SIZE`, `PRSYNC_DELTA_MAX_LITERALS`, `PRSYNC_DELTA_HELPER`, `PRSYNC_DELTA_FALLBACK`, `PRSYNC_STRICT_DURABILITY`, `PRSYNC_VERIFY_EXISTING`, `PRSYNC_SFTP_READ_CONCURRENCY`, `PRSYNC_SFTP_READ_CHUNK_SIZE`
The order of precedence is CLI > env > config file > built-in defaults.

You can override state location explicitly:

```bash
prsync -vrPlu --state-dir /var/tmp/prsync-state user@host:/src /dst
```

## Benchmark harness

Run comparable local measurements against `prsync`, `rclone`, and `rsync`:

```bash
./scripts/bench_sftp.sh user@host:/src /tmp/prsync-bench 5 16
```

## Metadata flags

Optional metadata preservation beyond `-vrPlu`:

- `-p`: permissions
- `-o`: owner
- `-g`: group
- `-A`: ACLs (`getfacl` on remote, `setfacl` on local)
- `-X`: xattrs (`getfattr` on remote, local xattr apply)
-
