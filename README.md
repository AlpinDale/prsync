# prsync

`prsync` is a Rust CLI for high-throughput, resumable pull syncs from SSH remotes.

## Goals

- Parallel transfers across files
- Chunked resume for large files
- Automatic resume on rerun (aria2-style)
- Practical compatibility with common `rsync -vrPlu` workflows

## Usage

```bash
prsync -vrPlu user@example.com:/remote/path /local/destination
```

To specify a non-default SSH port:

```bash
prsync -vrPlu user@example.com:2222:/remote/path /local/destination
```

## Key behavior

- `-r`: recursive directory sync
- `-v`: verbose summary
- `-P`: partial + progress mode
- `-l`: preserve symlinks
- `-u`: skip if destination file is newer than source
- `--resume` / `--no-resume`: explicit resume policy override
- Resume state is stored in `/local/destination/.prsync/state.db`
- Active run lock is `/local/destination/.prsync/lock`

## Performance knobs

```bash
prsync -vrPlu --jobs 16 --chunk-size 16777216 --chunk-threshold 134217728 user@host:/src /dst
```

## Config and precedence

- Optional config file: `~/.config/prsync/config.toml`
- Supported keys: `jobs`, `chunk_size`, `chunk_threshold`, `retries`, `resume`, `state_dir`
- Environment overrides: `PRSYNC_JOBS`, `PRSYNC_CHUNK_SIZE`, `PRSYNC_CHUNK_THRESHOLD`, `PRSYNC_RETRIES`, `PRSYNC_RESUME`, `PRSYNC_STATE_DIR`
- Precedence: CLI > env > config file > built-in defaults

You can override state location explicitly:

```bash
prsync -vrPlu --state-dir /var/tmp/prsync-state user@host:/src /dst
```

## Metadata flags

Optional metadata preservation beyond `-vrPlu`:

- `-p`: permissions
- `-o`: owner
- `-g`: group
- `-A`: ACLs (`getfacl` on remote, `setfacl` on local)
- `-X`: xattrs (`getfattr` on remote, local xattr apply)

## Notes

- Transfer backend is `ssh2` (libssh2) over SFTP with a connection pool.
- `~/.ssh/config` host aliases are supported (`Host`, `HostName`, `User`, `Port`, `IdentityFile`).
- SSH auth attempts agent, default key files (`~/.ssh/id_ed25519`, `~/.ssh/id_rsa`), then `PRSYNC_SSH_PASSWORD`.
- Path safety checks reject absolute and `..` traversal remote entries.
- Remote file mutation is re-checked before finalize; changed files are retried from scratch.

## Test suites

- Fast suite: `cargo test`
- Docker e2e: `cargo test --test e2e_sshd -- --ignored`
- Performance smoke: `cargo test --test perf_smoke -- --ignored`
