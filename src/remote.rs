use std::{
    collections::HashSet,
    ffi::OsString,
    io::{Read, Seek, SeekFrom, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ssh2::{File, FileStat, Session, Sftp};

use crate::delta::protocol::{BlockSigWire, DeltaPlan, HelperRequest, HelperResponse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSpec {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
    pub port_explicit: bool,
    pub path: String,
    pub path_trailing_star: bool,
}

impl RemoteSpec {
    pub fn parse(input: &str) -> Result<Self> {
        let (host_part, path_part) = input
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("remote must be in format [user@]host:/path"))?;
        if host_part.trim().is_empty() || path_part.trim().is_empty() {
            bail!("remote must include non-empty host and path");
        }

        let (user, host_port) = if let Some((user, host)) = host_part.split_once('@') {
            if user.is_empty() || host.is_empty() {
                bail!("invalid user@host format");
            }
            (Some(user.to_string()), host.to_string())
        } else {
            (None, host_part.to_string())
        };

        let (host, port, port_explicit) = parse_host_port(&host_port)?;

        Ok(Self {
            user,
            host,
            port,
            port_explicit,
            path: normalize_source_path(path_part)?,
            path_trailing_star: path_part.ends_with("/*"),
        })
    }

    pub fn display_host(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

fn parse_host_port(host_port: &str) -> Result<(String, u16, bool)> {
    if let Some((host, port_str)) = host_port.rsplit_once(':') {
        if host.is_empty() {
            bail!("host cannot be empty");
        }
        if !port_str.is_empty() && port_str.chars().all(|c| c.is_ascii_digit()) {
            let port = port_str
                .parse::<u16>()
                .with_context(|| format!("invalid port: {port_str}"))?;
            return Ok((host.to_string(), port, true));
        }
    }
    Ok((host_port.to_string(), 22, false))
}

fn normalize_source_path(raw: &str) -> Result<String> {
    if raw.ends_with("/*") {
        let trimmed = raw.trim_end_matches('*').trim_end_matches('/');
        if trimmed.is_empty() {
            bail!("invalid remote source path: {raw}");
        }
        return Ok(trimmed.to_string());
    }

    if raw.contains('*') || raw.contains('?') || raw.contains('[') || raw.contains(']') {
        bail!(
            "unsupported remote glob pattern: {raw}. Use a concrete directory path or trailing /*"
        );
    }

    Ok(raw.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub relative_path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_secs: i64,
    pub mode: u32,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub link_target: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFileStat {
    pub size: u64,
    pub mtime_secs: i64,
}

pub trait RemoteClient {
    fn list_entries(&self, recursive: bool) -> Result<Vec<RemoteEntry>>;
    fn list_entries_with_progress(
        &self,
        recursive: bool,
        _progress: Option<&dyn Fn(usize)>,
    ) -> Result<Vec<RemoteEntry>> {
        self.list_entries(recursive)
    }
    fn read_range(&self, relative_path: &Path, offset: u64, len: u64) -> Result<Vec<u8>>;
    fn stat_file(&self, relative_path: &Path) -> Result<RemoteFileStat>;
    fn generate_delta_plan(
        &self,
        _relative_path: &Path,
        _source_size: u64,
        _source_mtime_secs: i64,
        _block_size: u32,
        _blocks: &[BlockSigWire],
        _helper_command: &str,
    ) -> Result<DeltaPlan> {
        bail!("delta helper protocol unsupported by remote implementation")
    }

    fn get_acl_text(&self, _relative_path: &Path) -> Result<Option<String>> {
        Ok(None)
    }

    fn get_xattrs(&self, _relative_path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(Vec::new())
    }
}

#[derive(Clone)]
pub struct SshRemote {
    spec: RemoteSpec,
    root_path: PathBuf,
    root_kind: EntryKind,
    root_basename: PathBuf,
    star_children_mode: bool,
    pool: Arc<ConnectionPool>,
}

impl SshRemote {
    pub fn connect(spec: RemoteSpec, pool_size: usize) -> Result<Self> {
        let star_children_mode = spec.path_trailing_star;
        let target = resolve_connect_target(&spec)?;
        let pool = Arc::new(ConnectionPool::new(target, pool_size.max(1))?);
        let root_path = PathBuf::from(spec.path.clone());

        let root_stat = {
            let mut conn = pool.checkout()?;
            conn.lstat(&root_path)
                .with_context(|| format!("remote path not found: {}", root_path.display()))?
        };
        let root_kind = entry_kind_from_stat(&root_stat)?;
        let root_basename = root_path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        Ok(Self {
            spec,
            root_path,
            root_kind,
            root_basename,
            star_children_mode,
            pool,
        })
    }

    fn remote_path_for(&self, relative_path: &Path) -> PathBuf {
        if self.root_kind == EntryKind::Dir {
            self.root_path.join(relative_path)
        } else {
            self.root_path.clone()
        }
    }

    fn collect_single_root_entry(&self) -> Result<RemoteEntry> {
        let mut conn = self.pool.checkout()?;
        let stat = conn.lstat(&self.root_path)?;
        let kind = entry_kind_from_stat(&stat)?;
        let link_target = if kind == EntryKind::Symlink {
            conn.readlink(&self.root_path).ok()
        } else {
            None
        };

        Ok(RemoteEntry {
            relative_path: self.root_basename.clone(),
            kind,
            size: stat.size.unwrap_or(0),
            mtime_secs: stat.mtime.unwrap_or(0) as i64,
            mode: stat.perm.unwrap_or(0o644) & 0o7777,
            uid: stat.uid,
            gid: stat.gid,
            link_target,
        })
    }

    fn walk_dir(
        &self,
        recursive: bool,
        progress: Option<&dyn Fn(usize)>,
    ) -> Result<Vec<RemoteEntry>> {
        let mut out = Vec::new();
        let mut stack = vec![(self.root_path.clone(), PathBuf::new())];
        let mut found = 0_usize;

        while let Some((remote_dir, rel_dir)) = stack.pop() {
            let mut conn = self.pool.checkout()?;
            let entries = conn
                .readdir(&remote_dir)
                .with_context(|| format!("failed listing remote dir: {}", remote_dir.display()))?;
            drop(conn);

            for (full_path, readdir_stat) in entries {
                let Some(name) = full_path.file_name() else {
                    continue;
                };
                if name == "." || name == ".." {
                    continue;
                }

                let child_rel = if rel_dir.as_os_str().is_empty() {
                    PathBuf::from(name)
                } else {
                    rel_dir.join(name)
                };

                // Use attributes returned by readdir to avoid per-file lstat RTTs.
                let mut stat = readdir_stat;
                let mut kind = stat.perm.map(entry_kind_from_perm);
                if kind.is_none() {
                    let mut conn = self.pool.checkout()?;
                    stat = conn.lstat(&full_path)?;
                    kind = Some(entry_kind_from_stat(&stat)?);
                }
                let kind = kind.expect("kind set");

                let link_target = if kind == EntryKind::Symlink {
                    let mut conn = self.pool.checkout()?;
                    conn.readlink(&full_path).ok()
                } else {
                    None
                };

                let entry = RemoteEntry {
                    relative_path: child_rel.clone(),
                    kind: kind.clone(),
                    size: stat.size.unwrap_or(0),
                    mtime_secs: stat.mtime.unwrap_or(0) as i64,
                    mode: stat.perm.unwrap_or(0o644) & 0o7777,
                    uid: stat.uid,
                    gid: stat.gid,
                    link_target,
                };

                if recursive && kind == EntryKind::Dir {
                    stack.push((full_path.clone(), child_rel));
                }

                out.push(entry);
                found += 1;
                if found.is_multiple_of(200) {
                    if let Some(cb) = progress {
                        cb(found);
                    }
                }
            }

            if !recursive {
                break;
            }
        }

        out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        if let Some(cb) = progress {
            cb(found);
        }
        Ok(out)
    }

    fn list_entries_via_find(
        &self,
        recursive: bool,
        progress: Option<&dyn Fn(usize)>,
    ) -> Result<Vec<RemoteEntry>> {
        if self.root_kind != EntryKind::Dir {
            return Ok(vec![self.collect_single_root_entry()?]);
        }

        let mut conn = self.pool.checkout()?;
        let mut out = conn.stream_find_entries(
            &self.root_path,
            recursive,
            self.star_children_mode,
            progress,
        )?;

        out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        if let Some(cb) = progress {
            cb(out.len());
        }
        Ok(out)
    }

    fn run_exec_output(&self, command: &str) -> Result<ExecOutput> {
        let mut conn = self.pool.checkout()?;
        conn.exec(command)
    }

    fn run_exec_with_input_output(&self, command: &str, input: &[u8]) -> Result<ExecOutput> {
        let mut conn = self.pool.checkout()?;
        conn.exec_with_input(command, input)
    }

    fn run_shell_delta_fallback(&self, req: &HelperRequest) -> Result<ExecOutput> {
        let req_json = serde_json::to_vec(req).context("serialize fallback request")?;
        let req_b64 = STANDARD.encode(req_json);
        let py = r#"python3 - <<'PY'
import base64, json, os, pathlib, sys, hashlib
def weak_hash(buf):
    MOD=1<<16
    a=0
    b=0
    n=len(buf)
    for i,x in enumerate(buf):
        a=(a+x)%MOD
        b=(b+((n-i)*x))%MOD
    return ((b<<16)|a) & 0xffffffff
def strong_hash(buf):
    return hashlib.md5(buf).hexdigest()
req=json.loads(base64.b64decode(os.environ['PRSYNC_DELTA_REQ_B64']))
p=pathlib.Path(req['source_path'])
data=p.read_bytes()
bs=int(req['block_size'])
byweak={}
for b in req['blocks']:
    byweak.setdefault(int(b['weak']), []).append(b)
ops=[]
i=0
lit_start=0
copy_bytes=0
while bs>0 and i+bs<=len(data):
    w=weak_hash(data[i:i+bs])
    m=None
    for cand in byweak.get(w,[]):
        if int(cand['len'])==bs and cand['strong_hex'].lower()==strong_hash(data[i:i+bs]):
            m=cand
            break
    if m is not None:
        if lit_start<i:
            ops.append({'kind':'Literal','data_b64':base64.b64encode(data[lit_start:i]).decode()})
        ops.append({'kind':'Copy','block_index':int(m['index']),'len':int(m['len'])})
        copy_bytes += int(m['len'])
        i += bs
        lit_start = i
        continue
    i += 1
if lit_start < len(data):
    ops.append({'kind':'Literal','data_b64':base64.b64encode(data[lit_start:]).decode()})
out={
  'protocol_version':1,
  'source_size':len(data),
  'source_mtime_secs':int(req.get('mtime_secs',0)),
  'ops':ops,
  'final_digest_hex':strong_hash(data),
  'literal_bytes':len(data)-copy_bytes,
  'copy_bytes':copy_bytes
}
sys.stdout.write(json.dumps(out))
PY"#;
        let cmd = format!(
            "PRSYNC_DELTA_REQ_B64={} sh -lc {}",
            shell_quote_word(&req_b64),
            shell_quote_word(py)
        );
        self.run_exec_output(&cmd)
    }
}

impl RemoteClient for SshRemote {
    fn list_entries(&self, recursive: bool) -> Result<Vec<RemoteEntry>> {
        self.list_entries_with_progress(recursive, None)
    }

    fn list_entries_with_progress(
        &self,
        recursive: bool,
        progress: Option<&dyn Fn(usize)>,
    ) -> Result<Vec<RemoteEntry>> {
        match self.list_entries_via_find(recursive, progress) {
            Ok(entries) => Ok(entries),
            Err(_) => {
                if self.root_kind == EntryKind::Dir {
                    self.walk_dir(recursive, progress)
                } else {
                    Ok(vec![self.collect_single_root_entry()?])
                }
            }
        }
    }

    fn read_range(&self, relative_path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
        let full_path = self.remote_path_for(relative_path);
        let mut last_err: Option<anyhow::Error> = None;

        for _ in 0..2 {
            let mut conn = self.pool.checkout()?;
            match conn.read_range(&full_path, offset, len) {
                Ok(buf) => return Ok(buf),
                Err(err) => {
                    last_err = Some(err.context(format!(
                        "read remote range {} @{}+{} via {}",
                        full_path.display(),
                        offset,
                        len,
                        self.spec.display_host()
                    )));
                    if let Ok(new_conn) = self.pool.connect_one() {
                        conn.replace(new_conn);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("unable to read range")))
    }

    fn stat_file(&self, relative_path: &Path) -> Result<RemoteFileStat> {
        let full_path = self.remote_path_for(relative_path);
        let mut conn = self.pool.checkout()?;
        let stat = conn.lstat(&full_path)?;
        Ok(RemoteFileStat {
            size: stat.size.unwrap_or(0),
            mtime_secs: stat.mtime.unwrap_or(0) as i64,
        })
    }

    fn get_acl_text(&self, relative_path: &Path) -> Result<Option<String>> {
        let full_path = self.remote_path_for(relative_path);
        let cmd = format!("getfacl --absolute-names -p -- {}", shell_quote(&full_path));
        let output = self.run_exec_output(&cmd)?;
        if output.exit_status == 0 {
            return String::from_utf8(output.stdout)
                .context("remote command output was not valid utf-8")
                .map(Some);
        }
        bail!(
            "remote ACL query failed for {} ({}): {}. Install/support getfacl on the remote host or disable -A/--acls",
            full_path.display(),
            output.exit_status,
            output.stderr.trim()
        )
    }

    fn get_xattrs(&self, relative_path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
        let full_path = self.remote_path_for(relative_path);
        let getfattr_cmd = format!(
            "getfattr --absolute-names -d --encoding=base64 -m- -- {}",
            shell_quote(&full_path)
        );
        let output = self.run_exec_output(&getfattr_cmd)?;
        if output.exit_status == 0 {
            return parse_xattrs(
                &String::from_utf8(output.stdout)
                    .context("remote command output was not valid utf-8")?,
            );
        }

        let stderr = output.stderr.to_ascii_lowercase();
        if stderr.contains("no such attribute") || stderr.contains("no attributes") {
            return Ok(Vec::new());
        }

        if is_missing_remote_command(output.exit_status, &output.stderr) {
            let mac_cmd = format!("xattr -l -x -- {}", shell_quote(&full_path));
            let mac_out = self.run_exec_output(&mac_cmd)?;
            if mac_out.exit_status == 0 {
                return parse_macos_xattrs(
                    &String::from_utf8(mac_out.stdout)
                        .context("remote command output was not valid utf-8")?,
                );
            }
            let mac_stderr = mac_out.stderr.to_ascii_lowercase();
            if mac_stderr.contains("no such xattr") || mac_stderr.contains("no such attribute") {
                return Ok(Vec::new());
            }
            bail!(
                "remote xattr query failed for {}: neither getfattr nor xattr succeeded. getfattr stderr='{}' xattr stderr='{}'",
                full_path.display(),
                output.stderr.trim(),
                mac_out.stderr.trim()
            );
        }
        bail!(
            "remote xattr query failed for {} ({}): {}. Install/support getfattr on remote or disable -X/--xattrs",
            full_path.display(),
            output.exit_status,
            output.stderr.trim()
        )
    }

    fn generate_delta_plan(
        &self,
        relative_path: &Path,
        source_size: u64,
        source_mtime_secs: i64,
        block_size: u32,
        blocks: &[BlockSigWire],
        helper_command: &str,
    ) -> Result<DeltaPlan> {
        let full_path = self.remote_path_for(relative_path);
        let req = HelperRequest {
            protocol_version: 1,
            source_path: full_path.to_string_lossy().to_string(),
            file_size: source_size,
            mtime_secs: source_mtime_secs,
            block_size,
            blocks: blocks.to_vec(),
            max_literals: u64::MAX,
        };
        let payload = serde_json::to_vec(&req).context("serialize delta helper request")?;
        let helper_cmd = format!("{helper_command} --stdio");
        let output = self
            .run_exec_with_input_output(&helper_cmd, &payload)
            .or_else(|primary_err| {
                self.run_shell_delta_fallback(&req).map_err(|fallback_err| {
                    primary_err.context(format!(
                        "delta helper unavailable and shell fallback failed: {fallback_err:#}"
                    ))
                })
            })?;
        if output.exit_status != 0 {
            bail!(
                "delta helper failed ({}): {}",
                output.exit_status,
                output.stderr.trim()
            );
        }
        let resp: HelperResponse =
            serde_json::from_slice(&output.stdout).context("parse delta helper response")?;
        Ok(DeltaPlan {
            ops: resp.ops,
            final_digest_hex: resp.final_digest_hex,
            literal_bytes: resp.literal_bytes,
            copy_bytes: resp.copy_bytes,
            source_size: resp.source_size,
            source_mtime_secs: resp.source_mtime_secs,
        })
    }
}

#[derive(Debug, Clone)]
struct ConnectTarget {
    host: String,
    port: u16,
    user: String,
    identity_files: Vec<PathBuf>,
}

fn resolve_connect_target(spec: &RemoteSpec) -> Result<ConnectTarget> {
    let config = load_user_ssh_config().unwrap_or_default();
    let cfg = config.resolve_for_host(&spec.host);

    let host = cfg.hostname.unwrap_or_else(|| spec.host.clone());
    let port = if spec.port_explicit {
        spec.port
    } else {
        cfg.port.unwrap_or(spec.port)
    };

    let user = spec
        .user
        .clone()
        .or(cfg.user)
        .or_else(|| std::env::var("USER").ok())
        .ok_or_else(|| anyhow!("missing ssh username; use user@host:/path"))?;

    let identity_files = cfg
        .identity_files
        .iter()
        .map(|p| expand_identity_file(p, &host, &user, port))
        .collect();

    Ok(ConnectTarget {
        host,
        port,
        user,
        identity_files,
    })
}

struct Connection {
    session: Session,
    sftp: Sftp,
    open_read_path: Option<PathBuf>,
    open_read_file: Option<File>,
}

struct ExecOutput {
    stdout: Vec<u8>,
    stderr: String,
    exit_status: i32,
}

impl Connection {
    fn connect(target: &ConnectTarget) -> Result<Self> {
        let tcp = TcpStream::connect((target.host.as_str(), target.port))
            .with_context(|| format!("connect ssh tcp {}:{}", target.host.as_str(), target.port))?;
        tcp.set_nodelay(true).context("configure ssh tcp nodelay")?;

        let mut session = Session::new().context("create ssh session")?;
        session.set_tcp_stream(tcp);
        // Bound blocking SSH/SFTP operations so Ctrl+C can be observed in outer loops.
        session.set_timeout(5_000);
        session.handshake().context("ssh handshake failed")?;

        authenticate_session(&session, target)?;
        let sftp = session.sftp().context("create sftp session")?;
        Ok(Self {
            session,
            sftp,
            open_read_path: None,
            open_read_file: None,
        })
    }

    fn readdir(&mut self, path: &Path) -> Result<Vec<(PathBuf, FileStat)>> {
        self.sftp
            .readdir(path)
            .with_context(|| format!("sftp readdir failed: {}", path.display()))
    }

    fn lstat(&mut self, path: &Path) -> Result<FileStat> {
        self.sftp
            .lstat(path)
            .with_context(|| format!("sftp lstat failed: {}", path.display()))
    }

    fn readlink(&mut self, path: &Path) -> Result<PathBuf> {
        self.sftp
            .readlink(path)
            .with_context(|| format!("sftp readlink failed: {}", path.display()))
    }

    fn read_range(&mut self, path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
        let needs_open = self
            .open_read_path
            .as_ref()
            .map(|p| p != path)
            .unwrap_or(true)
            || self.open_read_file.is_none();
        if needs_open {
            self.open_read_file = Some(
                self.sftp
                    .open(path)
                    .with_context(|| format!("sftp open failed: {}", path.display()))?,
            );
            self.open_read_path = Some(path.to_path_buf());
        }

        let file = self
            .open_read_file
            .as_mut()
            .ok_or_else(|| anyhow!("missing open sftp file handle"))?;
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("sftp seek failed: {}", path.display()))?;

        let mut buf = vec![0_u8; len as usize];
        let mut read = 0_usize;
        while read < buf.len() {
            let n = file.read(&mut buf[read..]).with_context(|| {
                format!("sftp read failed {} at offset {}", path.display(), offset)
            })?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(buf)
    }

    fn exec(&mut self, cmd: &str) -> Result<ExecOutput> {
        let mut channel = self.session.channel_session().context("open ssh channel")?;
        channel
            .exec(cmd)
            .with_context(|| format!("exec remote command: {cmd}"))?;

        let mut stdout = Vec::new();
        channel
            .read_to_end(&mut stdout)
            .context("read command stdout")?;
        let mut stderr = String::new();
        channel
            .stderr()
            .read_to_string(&mut stderr)
            .context("read command stderr")?;
        channel.wait_close().context("wait for command close")?;
        let exit_status = channel.exit_status().context("read command exit status")?;
        Ok(ExecOutput {
            stdout,
            stderr,
            exit_status,
        })
    }

    fn exec_with_input(&mut self, cmd: &str, input: &[u8]) -> Result<ExecOutput> {
        let mut channel = self.session.channel_session().context("open ssh channel")?;
        channel
            .exec(cmd)
            .with_context(|| format!("exec remote command: {cmd}"))?;
        channel.write_all(input).context("write command stdin")?;
        channel.send_eof().context("send eof to remote command")?;
        let mut stdout = Vec::new();
        channel
            .read_to_end(&mut stdout)
            .context("read command stdout")?;
        let mut stderr = String::new();
        channel
            .stderr()
            .read_to_string(&mut stderr)
            .context("read command stderr")?;
        channel.wait_close().context("wait for command close")?;
        let exit_status = channel.exit_status().context("read command exit status")?;
        Ok(ExecOutput {
            stdout,
            stderr,
            exit_status,
        })
    }

    fn stream_find_entries(
        &mut self,
        root: &Path,
        recursive: bool,
        star_children_mode: bool,
        progress: Option<&dyn Fn(usize)>,
    ) -> Result<Vec<RemoteEntry>> {
        let depth_expr = if recursive { "" } else { "-maxdepth 1" };
        let cmd = format!(
            "cd {} && find . {depth_expr} -mindepth 1 -printf '%y\\037%P\\037%s\\037%T@\\037%m\\037%U\\037%G\\037%l\\0'",
            shell_quote(root)
        );

        let mut channel = self.session.channel_session().context("open ssh channel")?;
        channel
            .exec(&cmd)
            .with_context(|| format!("exec remote command: {cmd}"))?;

        let mut out = Vec::new();
        let mut pending = Vec::<u8>::new();
        let mut read_buf = [0_u8; 64 * 1024];

        loop {
            let n = channel.read(&mut read_buf).context("read find stream")?;
            if n == 0 {
                break;
            }
            pending.extend_from_slice(&read_buf[..n]);

            while let Some(pos) = pending.iter().position(|b| *b == 0) {
                let rec: Vec<u8> = pending.drain(..pos).collect();
                pending.drain(..1);
                if let Some(entry) = parse_find_record(&rec, star_children_mode)? {
                    out.push(entry);
                    if out.len().is_multiple_of(100) {
                        if let Some(cb) = progress {
                            cb(out.len());
                        }
                    }
                }
            }
        }

        if !pending.is_empty() {
            if let Some(entry) = parse_find_record(&pending, star_children_mode)? {
                out.push(entry);
            }
        }

        let mut stderr = String::new();
        channel
            .stderr()
            .read_to_string(&mut stderr)
            .context("read command stderr")?;
        channel.wait_close().context("wait for command close")?;
        let exit_status = channel.exit_status().context("read command exit status")?;
        if exit_status != 0 {
            bail!("remote command failed ({exit_status}): {}", stderr.trim());
        }

        Ok(out)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Keep shutdown snappy; transfer-time timeout remains higher.
        self.session.set_timeout(150);
        let _ = self.session.disconnect(None, "prsync shutdown", None);
    }
}

fn authenticate_session(session: &Session, target: &ConnectTarget) -> Result<()> {
    if session.userauth_agent(&target.user).is_ok() && session.authenticated() {
        return Ok(());
    }

    if try_pubkey_files(session, &target.user, &target.identity_files)? && session.authenticated() {
        return Ok(());
    }

    if let Ok(password) = std::env::var("PRSYNC_SSH_PASSWORD") {
        if session.userauth_password(&target.user, &password).is_ok() && session.authenticated() {
            return Ok(());
        }
    }

    bail!(
        "ssh authentication failed for {}@{}:{} (tried agent, configured/default keys, PRSYNC_SSH_PASSWORD)",
        target.user,
        target.host,
        target.port
    )
}

fn try_pubkey_files(session: &Session, user: &str, configured: &[PathBuf]) -> Result<bool> {
    for private in configured {
        if !private.exists() {
            continue;
        }
        let public = private.with_extension("pub");
        let public_ref = if public.exists() {
            Some(public.as_path())
        } else {
            None
        };
        if session
            .userauth_pubkey_file(user, public_ref, private, None)
            .is_ok()
            && session.authenticated()
        {
            return Ok(true);
        }
    }

    let home = match std::env::var("HOME") {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    let defaults = ["id_ed25519", "id_rsa"];
    for key in defaults {
        let private = PathBuf::from(&home).join(".ssh").join(key);
        if !private.exists() {
            continue;
        }
        let public = private.with_extension("pub");
        let public_ref = if public.exists() {
            Some(public.as_path())
        } else {
            None
        };
        if session
            .userauth_pubkey_file(user, public_ref, &private, None)
            .is_ok()
            && session.authenticated()
        {
            return Ok(true);
        }
    }

    Ok(false)
}

struct ConnectionPool {
    target: ConnectTarget,
    inner: Mutex<PoolState>,
    condvar: Condvar,
}

struct PoolState {
    idle: Vec<Connection>,
    created: usize,
    max_size: usize,
}

impl ConnectionPool {
    fn new(target: ConnectTarget, pool_size: usize) -> Result<Self> {
        let mut first = Vec::new();
        // Eagerly open one connection for fast-fail auth/host errors,
        // then grow lazily as workers request more connections.
        first.push(Connection::connect(&target)?);
        let pool = Self {
            target,
            inner: Mutex::new(PoolState {
                idle: first,
                created: 1,
                max_size: pool_size.max(1),
            }),
            condvar: Condvar::new(),
        };
        Ok(pool)
    }

    fn connect_one(&self) -> Result<Connection> {
        Connection::connect(&self.target)
    }

    fn checkout(&self) -> Result<PooledConnection<'_>> {
        let mut locked = self
            .inner
            .lock()
            .map_err(|_| anyhow!("pool lock poisoned"))?;
        loop {
            if let Some(conn) = locked.idle.pop() {
                return Ok(PooledConnection {
                    pool: self,
                    conn: Some(conn),
                });
            }

            if locked.created < locked.max_size {
                locked.created += 1;
                drop(locked);
                let conn = match self.connect_one() {
                    Ok(conn) => conn,
                    Err(err) => {
                        let mut state = self
                            .inner
                            .lock()
                            .map_err(|_| anyhow!("pool lock poisoned"))?;
                        state.created = state.created.saturating_sub(1);
                        self.condvar.notify_one();
                        return Err(err);
                    }
                };
                return Ok(PooledConnection {
                    pool: self,
                    conn: Some(conn),
                });
            }

            locked = self
                .condvar
                .wait(locked)
                .map_err(|_| anyhow!("pool lock poisoned while waiting"))?;
        }
    }

    fn checkin(&self, conn: Connection) -> Result<()> {
        let mut locked = self
            .inner
            .lock()
            .map_err(|_| anyhow!("pool lock poisoned"))?;
        locked.idle.push(conn);
        self.condvar.notify_one();
        Ok(())
    }
}

struct PooledConnection<'a> {
    pool: &'a ConnectionPool,
    conn: Option<Connection>,
}

impl<'a> PooledConnection<'a> {
    fn replace(&mut self, conn: Connection) {
        self.conn = Some(conn);
    }

    fn readdir(&mut self, path: &Path) -> Result<Vec<(PathBuf, FileStat)>> {
        self.conn
            .as_mut()
            .ok_or_else(|| anyhow!("missing pooled connection"))?
            .readdir(path)
    }

    fn lstat(&mut self, path: &Path) -> Result<FileStat> {
        self.conn
            .as_mut()
            .ok_or_else(|| anyhow!("missing pooled connection"))?
            .lstat(path)
    }

    fn readlink(&mut self, path: &Path) -> Result<PathBuf> {
        self.conn
            .as_mut()
            .ok_or_else(|| anyhow!("missing pooled connection"))?
            .readlink(path)
    }

    fn read_range(&mut self, path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.conn
            .as_mut()
            .ok_or_else(|| anyhow!("missing pooled connection"))?
            .read_range(path, offset, len)
    }

    fn exec(&mut self, cmd: &str) -> Result<ExecOutput> {
        self.conn
            .as_mut()
            .ok_or_else(|| anyhow!("missing pooled connection"))?
            .exec(cmd)
    }

    fn exec_with_input(&mut self, cmd: &str, input: &[u8]) -> Result<ExecOutput> {
        self.conn
            .as_mut()
            .ok_or_else(|| anyhow!("missing pooled connection"))?
            .exec_with_input(cmd, input)
    }

    fn stream_find_entries(
        &mut self,
        root: &Path,
        recursive: bool,
        star_children_mode: bool,
        progress: Option<&dyn Fn(usize)>,
    ) -> Result<Vec<RemoteEntry>> {
        self.conn
            .as_mut()
            .ok_or_else(|| anyhow!("missing pooled connection"))?
            .stream_find_entries(root, recursive, star_children_mode, progress)
    }
}

impl Drop for PooledConnection<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = self.pool.checkin(conn);
        }
    }
}

fn entry_kind_from_stat(stat: &FileStat) -> Result<EntryKind> {
    let perm = stat
        .perm
        .ok_or_else(|| anyhow!("remote stat missing mode bits"))?;

    Ok(entry_kind_from_perm(perm))
}

fn entry_kind_from_perm(perm: u32) -> EntryKind {
    const S_IFMT: u32 = 0o170000;
    const S_IFREG: u32 = 0o100000;
    const S_IFDIR: u32 = 0o040000;
    const S_IFLNK: u32 = 0o120000;

    match perm & S_IFMT {
        S_IFDIR => EntryKind::Dir,
        S_IFLNK => EntryKind::Symlink,
        S_IFREG => EntryKind::File,
        _ => EntryKind::File,
    }
}

fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn shell_quote_word(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn parse_xattrs(output: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let mut attrs = Vec::new();
    for line in output.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let raw = value.trim();
        if let Some(b64) = raw.strip_prefix("0s") {
            let decoded = STANDARD
                .decode(b64)
                .with_context(|| format!("decode xattr value for {name}"))?;
            attrs.push((name.trim().to_string(), decoded));
        }
    }
    Ok(attrs)
}

fn parse_macos_xattrs(output: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let mut attrs = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_hex = String::new();

    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') && line.ends_with(':') {
            if let Some(name) = current_name.take() {
                let decoded = decode_hex_lossy(&current_hex)
                    .with_context(|| format!("decode macOS xattr value for {name}"))?;
                attrs.push((name, decoded));
                current_hex.clear();
            }
            current_name = Some(line.trim_end_matches(':').to_string());
            continue;
        }

        if current_name.is_some() {
            for (idx, token) in line.split_whitespace().enumerate() {
                // Skip the leading 8-hex offset column, keep only payload groups.
                if idx == 0 && token.len() == 8 && token.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                if token.chars().all(|c| c.is_ascii_hexdigit()) {
                    current_hex.push_str(token);
                }
            }
        }
    }

    if let Some(name) = current_name.take() {
        let decoded = decode_hex_lossy(&current_hex)
            .with_context(|| format!("decode macOS xattr value for {name}"))?;
        attrs.push((name, decoded));
    }

    Ok(attrs)
}

fn decode_hex_lossy(hex: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chars = hex.chars();
    while let (Some(a), Some(b)) = (chars.next(), chars.next()) {
        let hi = a
            .to_digit(16)
            .ok_or_else(|| anyhow!("invalid hex digit '{a}'"))?;
        let lo = b
            .to_digit(16)
            .ok_or_else(|| anyhow!("invalid hex digit '{b}'"))?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

fn is_missing_remote_command(exit_status: i32, stderr: &str) -> bool {
    if exit_status == 127 {
        return true;
    }
    let s = stderr.to_ascii_lowercase();
    s.contains("command not found") || s.contains("not found")
}

fn parse_find_record(rec: &[u8], star_children_mode: bool) -> Result<Option<RemoteEntry>> {
    if rec.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&[u8]> = rec.split(|b| *b == 0x1f).collect();
    let kind = match parts.first().copied().unwrap_or_default() {
        b"f" => EntryKind::File,
        b"d" => EntryKind::Dir,
        b"l" => EntryKind::Symlink,
        _ => return Ok(None),
    };
    let rel_raw = parts.get(1).copied().unwrap_or_default();
    if rel_raw.is_empty() {
        return Ok(None);
    }
    let rel_path = bytes_to_pathbuf(rel_raw);

    if star_children_mode {
        let first = rel_path
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        if first.starts_with('.') {
            return Ok(None);
        }
    }

    let size = parse_ascii_u64(parts.get(2).copied().unwrap_or(b"0")).unwrap_or(0);
    let mtime_secs = std::str::from_utf8(parts.get(3).copied().unwrap_or(b"0"))
        .ok()
        .unwrap_or("0")
        .parse::<f64>()
        .unwrap_or(0.0)
        .floor() as i64;
    let mode = u32::from_str_radix(
        std::str::from_utf8(parts.get(4).copied().unwrap_or(b"644"))
            .ok()
            .unwrap_or("644"),
        8,
    )
    .unwrap_or(0o644);
    let uid = parse_ascii_u32(parts.get(5).copied().unwrap_or_default());
    let gid = parse_ascii_u32(parts.get(6).copied().unwrap_or_default());
    let link_target = parts
        .get(7)
        .copied()
        .filter(|v| !v.is_empty())
        .map(bytes_to_pathbuf);

    Ok(Some(RemoteEntry {
        relative_path: rel_path,
        kind,
        size,
        mtime_secs,
        mode,
        uid,
        gid,
        link_target,
    }))
}

fn parse_ascii_u64(raw: &[u8]) -> Option<u64> {
    std::str::from_utf8(raw).ok()?.parse::<u64>().ok()
}

fn parse_ascii_u32(raw: &[u8]) -> Option<u32> {
    std::str::from_utf8(raw).ok()?.parse::<u32>().ok()
}

#[cfg(unix)]
fn bytes_to_pathbuf(raw: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(raw.to_vec()))
}

#[cfg(not(unix))]
fn bytes_to_pathbuf(raw: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(raw).to_string())
}

#[derive(Debug, Clone, Default)]
struct SshConfig {
    stanzas: Vec<HostStanza>,
}

#[derive(Debug, Clone)]
struct HostStanza {
    patterns: Vec<String>,
    options: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
struct ResolvedHost {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_files: Vec<PathBuf>,
}

impl SshConfig {
    fn resolve_for_host(&self, host_alias: &str) -> ResolvedHost {
        let mut resolved = ResolvedHost::default();
        for stanza in &self.stanzas {
            if !matches_host_patterns(host_alias, &stanza.patterns) {
                continue;
            }

            for (key, value) in &stanza.options {
                match key.as_str() {
                    "hostname" => {
                        resolved.hostname = Some(value.to_string());
                    }
                    "user" => {
                        resolved.user = Some(value.to_string());
                    }
                    "port" => {
                        if let Ok(p) = value.parse::<u16>() {
                            resolved.port = Some(p);
                        }
                    }
                    "identityfile" => {
                        resolved.identity_files.push(PathBuf::from(value));
                    }
                    _ => {}
                }
            }
        }

        resolved
    }
}

fn load_user_ssh_config() -> Result<SshConfig> {
    let home = std::env::var("HOME").context("HOME missing for ssh config")?;
    let path = PathBuf::from(home).join(".ssh/config");
    if !path.exists() {
        return Ok(SshConfig::default());
    }
    let mut visited = HashSet::new();
    load_ssh_config_path(&path, &mut visited)
}

fn load_ssh_config_path(path: &Path, visited: &mut HashSet<PathBuf>) -> Result<SshConfig> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Ok(SshConfig::default());
    }

    let raw = std::fs::read_to_string(&canonical)
        .with_context(|| format!("read ssh config: {}", canonical.display()))?;
    let mut main = parse_ssh_config(&raw);

    // Handle Include patterns (commonly used for ~/.ssh/config.d/*).
    let base_dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let includes = extract_include_patterns(&raw);
    for pattern in includes {
        let include_paths = expand_include_pattern(&pattern, &base_dir)?;
        for include_path in include_paths {
            if !include_path.exists() {
                continue;
            }
            let cfg = load_ssh_config_path(&include_path, visited)?;
            main.stanzas.extend(cfg.stanzas);
        }
    }

    Ok(main)
}

fn extract_include_patterns(text: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    for raw_line in text.lines() {
        let line = strip_comments(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = parse_key_value(&line) else {
            continue;
        };
        if k.eq_ignore_ascii_case("include") {
            patterns.extend(v.split_whitespace().map(normalize_value));
        }
    }
    patterns
}

fn expand_include_pattern(pattern: &str, base_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut p = pattern.to_string();
    if p.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            p = p.replacen('~', &home, 1);
        }
    } else if !Path::new(&p).is_absolute() {
        p = base_dir.join(p).to_string_lossy().to_string();
    }

    let mut out = Vec::new();
    let paths = glob::glob(&p).with_context(|| format!("invalid include glob: {p}"))?;
    for path in paths.flatten() {
        out.push(path);
    }
    out.sort();
    Ok(out)
}

fn parse_ssh_config(text: &str) -> SshConfig {
    let mut stanzas = Vec::new();
    let mut current = HostStanza {
        patterns: vec!["*".to_string()],
        options: Vec::new(),
    };

    for raw_line in text.lines() {
        let line = strip_comments(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }

        let Some((k, v)) = parse_key_value(&line) else {
            continue;
        };
        let key = k.to_ascii_lowercase();

        if key == "host" {
            if !current.options.is_empty() || current.patterns != vec!["*".to_string()] {
                stanzas.push(current);
            }
            let patterns = v
                .split_whitespace()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            current = HostStanza {
                patterns: if patterns.is_empty() {
                    vec!["*".to_string()]
                } else {
                    patterns
                },
                options: Vec::new(),
            };
            continue;
        }

        current.options.push((key, normalize_value(v)));
    }

    stanzas.push(current);
    SshConfig { stanzas }
}

fn strip_comments(line: &str) -> String {
    let mut out = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in line.chars() {
        if c == '\'' && !in_double {
            in_single = !in_single;
        } else if c == '"' && !in_single {
            in_double = !in_double;
        } else if c == '#' && !in_single && !in_double {
            break;
        }
        out.push(c);
    }
    out
}

fn parse_key_value(line: &str) -> Option<(&str, &str)> {
    if let Some((k, v)) = line.split_once('=') {
        return Some((k.trim(), v.trim()));
    }

    let mut split = line.splitn(2, char::is_whitespace);
    let key = split.next()?.trim();
    let value = split.next()?.trim();
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some((key, value))
}

fn normalize_value(value: &str) -> String {
    let v = value.trim();
    if (v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')) {
        return v[1..v.len() - 1].to_string();
    }
    v.to_string()
}

fn matches_host_patterns(host: &str, patterns: &[String]) -> bool {
    let mut matched_positive = false;
    for pattern in patterns {
        if let Some(neg) = pattern.strip_prefix('!') {
            if glob_match(neg, host) {
                return false;
            }
            continue;
        }

        if glob_match(pattern, host) {
            matched_positive = true;
        }
    }
    matched_positive
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_match = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            star_match = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            star_match += 1;
            ti = star_match;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

fn expand_identity_file(path: &Path, host: &str, user: &str, port: u16) -> PathBuf {
    let mut raw = path.to_string_lossy().to_string();
    if let Some(home) = std::env::var("HOME").ok().filter(|_| raw.starts_with('~')) {
        raw = raw.replacen('~', &home, 1);
    }
    raw = raw
        .replace("%h", host)
        .replace("%r", user)
        .replace("%p", &port.to_string());
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use tempfile::TempDir;

    use super::{
        glob_match, is_missing_remote_command, load_ssh_config_path, parse_find_record,
        parse_macos_xattrs, parse_ssh_config, parse_xattrs, EntryKind, RemoteSpec,
    };

    #[test]
    fn parse_remote_spec_user_host() {
        let spec = RemoteSpec::parse("alice@example.com:/srv/data").expect("parse");
        assert_eq!(spec.user.as_deref(), Some("alice"));
        assert_eq!(spec.host, "example.com");
        assert_eq!(spec.path, "/srv/data");
        assert_eq!(spec.port, 22);
        assert!(!spec.port_explicit);
    }

    #[test]
    fn parse_remote_with_port() {
        let spec = RemoteSpec::parse("alice@example.com:2222:/srv/data").expect("parse");
        assert_eq!(spec.user.as_deref(), Some("alice"));
        assert_eq!(spec.host, "example.com");
        assert_eq!(spec.port, 2222);
        assert!(spec.port_explicit);
    }

    #[test]
    fn parse_remote_with_trailing_star() {
        let spec = RemoteSpec::parse("alice@example.com:/srv/data/*").expect("parse");
        assert_eq!(spec.path, "/srv/data");
    }

    #[test]
    fn parse_remote_spec_without_user() {
        let spec = RemoteSpec::parse("example.com:/srv/data").expect("parse");
        assert!(spec.user.is_none());
        assert_eq!(spec.host, "example.com");
    }

    #[test]
    fn rejects_bad_remote_spec() {
        assert!(RemoteSpec::parse("example.com").is_err());
        assert!(RemoteSpec::parse("@example.com:/a").is_err());
        assert!(RemoteSpec::parse("a@h:/srv/*/bad").is_err());
    }

    #[test]
    fn parses_base64_xattrs() {
        let parsed = parse_xattrs("# file: /tmp/a\nuser.flag=0sYmluYXJ5\n").expect("parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "user.flag");
        assert_eq!(parsed[0].1, b"binary");
    }

    #[test]
    fn missing_remote_command_is_detected() {
        assert!(is_missing_remote_command(127, ""));
        assert!(is_missing_remote_command(
            1,
            "/bin/sh: getfattr: command not found"
        ));
        assert!(!is_missing_remote_command(1, "permission denied"));
    }

    #[test]
    fn parses_macos_hex_xattrs() {
        let input = "com.example.flag:\n00000000 41424344 0a\n";
        let attrs = parse_macos_xattrs(input).expect("parse");
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].0, "com.example.flag");
        assert_eq!(attrs[0].1, b"ABCD\n");
    }

    #[cfg(unix)]
    #[test]
    fn parse_find_record_supports_non_utf8_paths() {
        let rec = b"f\x1ffoo\xffbar.epub\x1f12\x1f1710000000.0\x1f644\x1f1000\x1f1000\x1f";
        let entry = parse_find_record(rec, false)
            .expect("parse")
            .expect("entry");
        assert_eq!(entry.kind, EntryKind::File);
        assert_eq!(entry.size, 12);
        assert_eq!(
            entry.relative_path.as_os_str().to_string_lossy(),
            "foo\u{fffd}bar.epub"
        );
    }

    #[test]
    fn parses_ssh_config_and_resolves_alias() {
        let cfg = parse_ssh_config(
            r#"
            Host *
                User defaultuser
                Port 22

            Host lnbox
                HostName 192.168.1.9
                User alice
                Port 2222
                IdentityFile ~/.ssh/id_lnbox
            "#,
        );
        let resolved = cfg.resolve_for_host("lnbox");
        assert_eq!(resolved.hostname.as_deref(), Some("192.168.1.9"));
        assert_eq!(resolved.user.as_deref(), Some("alice"));
        assert_eq!(resolved.port, Some(2222));
        assert_eq!(resolved.identity_files.len(), 1);
    }

    #[test]
    fn glob_match_works() {
        assert!(glob_match("ln*", "lnbox"));
        assert!(glob_match("l?box", "lnbox"));
        assert!(!glob_match("a*", "lnbox"));
    }

    #[test]
    fn loads_include_configs() {
        let tmp = TempDir::new().expect("tmp");
        let main = tmp.path().join("config");
        let dir = tmp.path().join("config.d");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            &main,
            format!("Include {}/config.d/*\n", tmp.path().display()),
        )
        .expect("write main");
        std::fs::write(
            dir.join("lnbox"),
            "Host lnbox\n  HostName example.org\n  User alice\n  Port 2222\n",
        )
        .expect("write include");

        let mut visited = HashSet::new();
        let cfg = load_ssh_config_path(&main, &mut visited).expect("load");
        let resolved = cfg.resolve_for_host("lnbox");
        assert_eq!(resolved.hostname.as_deref(), Some("example.org"));
        assert_eq!(resolved.user.as_deref(), Some("alice"));
        assert_eq!(resolved.port, Some(2222));
    }
}
