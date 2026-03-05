use std::{
    fs,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use tempfile::TempDir;

struct DockerContainer {
    id: String,
}

impl Drop for DockerContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).status();
    }
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn docker_run(args: &[&str]) -> Result<String> {
    let output = Command::new("docker").args(args).output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "docker command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn parse_mapped_port(port_output: &str) -> Result<u16> {
    let mapped = port_output
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("missing docker port output"))?;
    let port = mapped
        .rsplit(':')
        .next()
        .ok_or_else(|| anyhow!("invalid docker port mapping"))?
        .parse::<u16>()
        .context("parse mapped port")?;
    Ok(port)
}

fn run_prsync(remote: &str, destination: &Path) -> Result<()> {
    let output = Command::new(assert_cmd::cargo::cargo_bin!("prsync"))
        .args(["-vrPlu", remote, &destination.display().to_string()])
        .env("PRSYNC_SSH_PASSWORD", "pass")
        .output()
        .context("run prsync")?;
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "prsync failed: {}",
        String::from_utf8_lossy(&output.stderr)
    ))
}

#[test]
#[ignore = "requires docker"]
fn e2e_pull_over_sftp_with_resume_state() -> Result<()> {
    if !docker_available() {
        return Ok(());
    }

    let fixture = TempDir::new()?;
    fs::create_dir_all(fixture.path().join("sub"))?;
    fs::write(fixture.path().join("hello.txt"), b"hello world")?;
    fs::write(fixture.path().join("sub/nested.txt"), b"nested")?;

    let cid = docker_run(&[
        "run",
        "-d",
        "-P",
        "-v",
        &format!("{}:/home/foo/upload", fixture.path().display()),
        "atmoz/sftp",
        "foo:pass:::upload",
    ])?;
    let _container = DockerContainer { id: cid.clone() };

    let port_out = docker_run(&["port", &cid, "22/tcp"])?;
    let port = parse_mapped_port(&port_out)?;
    let remote = format!("foo@127.0.0.1:{port}:/upload");

    let destination = TempDir::new()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_err: Option<anyhow::Error> = None;
    while Instant::now() < deadline {
        match run_prsync(&remote, destination.path()) {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(err) => {
                last_err = Some(err);
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
    if let Some(err) = last_err {
        return Err(err);
    }

    assert_eq!(
        fs::read(destination.path().join("hello.txt"))?,
        b"hello world"
    );
    assert_eq!(
        fs::read(destination.path().join("sub/nested.txt"))?,
        b"nested"
    );
    assert!(!destination.path().join(".prsync").exists());

    Ok(())
}
