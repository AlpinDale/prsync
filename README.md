# parsync

`parsync` is a high-throughput, resumable sync tool for SSH remotes and local-to-local transfers, with parallel file transfers and optional block-delta sync.

![demo](assets/demo.gif)

## Installation

**Linux and macOS:**

```bash
curl -fsSL https://alpindale.net/install.sh | bash
```

**Windows:**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://alpindale.net/install.ps1 | iex"
```

You can also install with cargo:

```bash
cargo install parsync
```

You may also download the binary for your platform from the
[releases page](https://github.com/AlpinDale/parsync/releases), or install from source:

```bash
make build
make install
```

## Platform support

- Linux: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`
- macOS: `aarch64-apple-darwin`, `x86_64-apple-darwin`
- Windows: `x86_64-pc-windows-msvc` (best-effort metadata support)

## Usage

```bash
parsync -vrPlu user@example.com:/remote/path /local/destination
```

With non-default SSH port:

```bash
parsync -vrPlu user@example.com:2222:/remote/path /local/destination
```

SSH config host aliases are supported.

## Performance tuning

```bash
parsync -vrPlu --jobs 16 --chunk-size 16777216 --chunk-threshold 134217728 user@host:/src /dst
```

Balanced mode defaults:

- no per-file `sync_all` barriers (atomic rename preserved)
- existing-file digest checks are skipped unless requested
- chunk completion state is committed in batches
- post-transfer remote mutation `stat` check is skipped (enabled in strict mode)

Throughput flags:

- `--strict-durability`: enable fsync-heavy strict mode
- `--verify-existing`: hash existing files before skip decisions
- `--sftp-read-concurrency`: parallel per-file read requests for large files
- `--sftp-read-chunk-size`: read request size for SFTP range pulls

### Notes on Windows metadata behavior

- `-A`, `-X`: warn and continue (unsupported)
- `-o`, `-g`: warn and continue (unsupported)
- `-p`: best-effort (readonly mapping), then continue
- `-l`: attempts symlink creation; if OS/privilege disallows it, symlink is skipped with warning

Enable strict mode to hard-fail on unsupported behavior:

```bash
parsync --strict-windows-metadata -vrPlu user@host:/src C:\\dst
```

## Windows symlink troubleshooting

Windows symlink creation usually requires one of:

- Administrator privileges
- Developer Mode enabled

If not available, `-l` may skip symlinks (or fail with `--strict-windows-metadata`).
