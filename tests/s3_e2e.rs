//! MinIO-based end-to-end tests for S3 backend.
//!
//! All tests require Docker and are `#[ignore]` by default.
//! Run with: `cargo test --features s3 -- --ignored`

use std::{
    fs,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use tempfile::TempDir;

// ── Docker helpers (same pattern as e2e_sshd.rs) ──────────────────────

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

// ── MinIO setup helpers ───────────────────────────────────────────────

const MINIO_USER: &str = "testuser";
const MINIO_PASS: &str = "testpass123";
const TEST_BUCKET: &str = "test-bucket";

struct MinioInstance {
    _container: DockerContainer,
    endpoint: String,
}

fn start_minio() -> Result<MinioInstance> {
    let cid = docker_run(&[
        "run",
        "-d",
        "-P",
        "-e",
        &format!("MINIO_ROOT_USER={MINIO_USER}"),
        "-e",
        &format!("MINIO_ROOT_PASSWORD={MINIO_PASS}"),
        "minio/minio",
        "server",
        "/data",
    ])?;
    let container = DockerContainer { id: cid.clone() };

    let port_out = docker_run(&["port", &cid, "9000/tcp"])?;
    let port = parse_mapped_port(&port_out)?;
    let endpoint = format!("http://127.0.0.1:{port}");

    // Wait for MinIO to be ready.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let check = Command::new("docker")
            .args(["exec", &cid, "curl", "-sf", "http://localhost:9000/minio/health/live"])
            .output();
        if let Ok(output) = check {
            if output.status.success() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(500));
    }

    Ok(MinioInstance {
        _container: container,
        endpoint,
    })
}

/// Create a bucket using the AWS CLI pointed at MinIO.
fn create_bucket(endpoint: &str) -> Result<()> {
    let status = Command::new("aws")
        .args([
            "s3api",
            "create-bucket",
            "--bucket",
            TEST_BUCKET,
            "--endpoint-url",
            endpoint,
        ])
        .env("AWS_ACCESS_KEY_ID", MINIO_USER)
        .env("AWS_SECRET_ACCESS_KEY", MINIO_PASS)
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .output()
        .context("run aws s3api create-bucket")?;
    if !status.status.success() {
        return Err(anyhow!(
            "create-bucket failed: {}",
            String::from_utf8_lossy(&status.stderr)
        ));
    }
    Ok(())
}

/// Upload a local file to S3 via the AWS CLI.
fn upload_file(endpoint: &str, local_path: &Path, s3_key: &str) -> Result<()> {
    let s3_url = format!("s3://{TEST_BUCKET}/{s3_key}");
    let status = Command::new("aws")
        .args([
            "s3",
            "cp",
            &local_path.display().to_string(),
            &s3_url,
            "--endpoint-url",
            endpoint,
        ])
        .env("AWS_ACCESS_KEY_ID", MINIO_USER)
        .env("AWS_SECRET_ACCESS_KEY", MINIO_PASS)
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .output()
        .context("run aws s3 cp")?;
    if !status.status.success() {
        return Err(anyhow!(
            "s3 cp failed: {}",
            String::from_utf8_lossy(&status.stderr)
        ));
    }
    Ok(())
}

/// Run parsync to sync from S3 to a local destination.
fn run_parsync_s3(
    remote: &str,
    destination: &Path,
    endpoint: &str,
    extra_args: &[&str],
) -> Result<()> {
    let dest_str = destination.display().to_string();
    let mut args = vec![
        "-vrPu",
        remote,
        &dest_str,
        "--s3-endpoint-url",
        endpoint,
    ];
    args.extend_from_slice(extra_args);

    let output = Command::new(assert_cmd::cargo::cargo_bin!("parsync"))
        .args(&args)
        .env("AWS_ACCESS_KEY_ID", MINIO_USER)
        .env("AWS_SECRET_ACCESS_KEY", MINIO_PASS)
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .output()
        .context("run parsync")?;

    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "parsync failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn aws_available() -> bool {
    Command::new("aws")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[test]
#[ignore = "requires docker"]
fn e2e_pull_from_s3_basic() -> Result<()> {
    if !docker_available() || !aws_available() {
        return Ok(());
    }

    let minio = start_minio()?;

    // Create bucket and upload test files.
    create_bucket(&minio.endpoint)?;

    let fixture = TempDir::new()?;
    fs::write(fixture.path().join("hello.txt"), b"hello world")?;
    fs::write(fixture.path().join("data.bin"), b"binary data\x00\x01\x02")?;

    upload_file(&minio.endpoint, &fixture.path().join("hello.txt"), "hello.txt")?;
    upload_file(&minio.endpoint, &fixture.path().join("data.bin"), "data.bin")?;

    // Sync from S3 to local destination.
    let destination = TempDir::new()?;
    let remote = format!("s3://{TEST_BUCKET}/");

    run_parsync_s3(&remote, destination.path(), &minio.endpoint, &[])?;

    assert_eq!(
        fs::read(destination.path().join("hello.txt"))?,
        b"hello world"
    );
    assert_eq!(
        fs::read(destination.path().join("data.bin"))?,
        b"binary data\x00\x01\x02"
    );
    assert!(!destination.path().join(".parsync").exists());

    Ok(())
}

#[test]
#[ignore = "requires docker"]
fn e2e_pull_from_s3_recursive() -> Result<()> {
    if !docker_available() || !aws_available() {
        return Ok(());
    }

    let minio = start_minio()?;
    create_bucket(&minio.endpoint)?;

    let fixture = TempDir::new()?;
    fs::write(fixture.path().join("root.txt"), b"root")?;
    fs::create_dir_all(fixture.path().join("sub"))?;
    fs::write(fixture.path().join("sub/nested.txt"), b"nested")?;
    fs::create_dir_all(fixture.path().join("sub/deep"))?;
    fs::write(fixture.path().join("sub/deep/leaf.txt"), b"leaf")?;

    upload_file(&minio.endpoint, &fixture.path().join("root.txt"), "data/root.txt")?;
    upload_file(
        &minio.endpoint,
        &fixture.path().join("sub/nested.txt"),
        "data/sub/nested.txt",
    )?;
    upload_file(
        &minio.endpoint,
        &fixture.path().join("sub/deep/leaf.txt"),
        "data/sub/deep/leaf.txt",
    )?;

    let destination = TempDir::new()?;
    run_parsync_s3(
        &format!("s3://{TEST_BUCKET}/data"),
        destination.path(),
        &minio.endpoint,
        &[],
    )?;

    assert_eq!(
        fs::read(destination.path().join("root.txt"))?,
        b"root"
    );
    assert_eq!(
        fs::read(destination.path().join("sub/nested.txt"))?,
        b"nested"
    );
    assert_eq!(
        fs::read(destination.path().join("sub/deep/leaf.txt"))?,
        b"leaf"
    );

    Ok(())
}

#[test]
#[ignore = "requires docker"]
fn e2e_pull_from_s3_concurrent_small_files() -> Result<()> {
    if !docker_available() || !aws_available() {
        return Ok(());
    }

    let minio = start_minio()?;
    create_bucket(&minio.endpoint)?;

    // Upload 50 small files (keeping it manageable for test speed).
    let fixture = TempDir::new()?;
    for i in 0..50 {
        let name = format!("file_{:04}.txt", i);
        let content = format!("content of file {i}");
        let path = fixture.path().join(&name);
        fs::write(&path, content.as_bytes())?;
        upload_file(&minio.endpoint, &path, &name)?;
    }

    let destination = TempDir::new()?;
    run_parsync_s3(
        &format!("s3://{TEST_BUCKET}/"),
        destination.path(),
        &minio.endpoint,
        &["--jobs", "8"],
    )?;

    // Verify all files arrived correctly.
    for i in 0..50 {
        let name = format!("file_{:04}.txt", i);
        let expected = format!("content of file {i}");
        let actual = fs::read_to_string(destination.path().join(&name))?;
        assert_eq!(actual, expected, "mismatch for {name}");
    }

    Ok(())
}

#[test]
#[ignore = "requires docker"]
fn e2e_pull_from_s3_dry_run() -> Result<()> {
    if !docker_available() || !aws_available() {
        return Ok(());
    }

    let minio = start_minio()?;
    create_bucket(&minio.endpoint)?;

    let fixture = TempDir::new()?;
    fs::write(fixture.path().join("file.txt"), b"data")?;
    upload_file(&minio.endpoint, &fixture.path().join("file.txt"), "file.txt")?;

    let destination = TempDir::new()?;
    run_parsync_s3(
        &format!("s3://{TEST_BUCKET}/"),
        destination.path(),
        &minio.endpoint,
        &["--dry-run"],
    )?;

    // Dry run should not write any files.
    assert!(!destination.path().join("file.txt").exists());

    Ok(())
}

#[test]
#[ignore = "requires docker"]
fn e2e_pull_from_s3_update_flag() -> Result<()> {
    if !docker_available() || !aws_available() {
        return Ok(());
    }

    let minio = start_minio()?;
    create_bucket(&minio.endpoint)?;

    let fixture = TempDir::new()?;
    fs::write(fixture.path().join("file.txt"), b"original")?;
    upload_file(&minio.endpoint, &fixture.path().join("file.txt"), "file.txt")?;

    // First sync.
    let destination = TempDir::new()?;
    run_parsync_s3(
        &format!("s3://{TEST_BUCKET}/"),
        destination.path(),
        &minio.endpoint,
        &[],
    )?;
    assert_eq!(fs::read(destination.path().join("file.txt"))?, b"original");

    // Upload newer version.
    fs::write(fixture.path().join("file.txt"), b"updated")?;
    upload_file(&minio.endpoint, &fixture.path().join("file.txt"), "file.txt")?;

    // Re-sync with -u flag (already set in run_parsync_s3).
    run_parsync_s3(
        &format!("s3://{TEST_BUCKET}/"),
        destination.path(),
        &minio.endpoint,
        &[],
    )?;

    // The updated file should be downloaded since remote is newer.
    assert_eq!(fs::read(destination.path().join("file.txt"))?, b"updated");

    Ok(())
}

#[test]
#[ignore = "requires docker"]
fn e2e_pull_from_s3_trailing_star() -> Result<()> {
    if !docker_available() || !aws_available() {
        return Ok(());
    }

    let minio = start_minio()?;
    create_bucket(&minio.endpoint)?;

    let fixture = TempDir::new()?;
    fs::write(fixture.path().join("a.txt"), b"aaa")?;
    fs::write(fixture.path().join("b.txt"), b"bbb")?;
    upload_file(&minio.endpoint, &fixture.path().join("a.txt"), "prefix/a.txt")?;
    upload_file(&minio.endpoint, &fixture.path().join("b.txt"), "prefix/b.txt")?;

    let destination = TempDir::new()?;
    run_parsync_s3(
        &format!("s3://{TEST_BUCKET}/prefix/*"),
        destination.path(),
        &minio.endpoint,
        &[],
    )?;

    assert_eq!(fs::read(destination.path().join("a.txt"))?, b"aaa");
    assert_eq!(fs::read(destination.path().join("b.txt"))?, b"bbb");

    Ok(())
}

#[test]
#[ignore = "requires docker"]
fn e2e_pull_from_s3_no_such_bucket() -> Result<()> {
    if !docker_available() || !aws_available() {
        return Ok(());
    }

    let minio = start_minio()?;
    // Don't create the bucket — expect an error.

    let destination = TempDir::new()?;
    let result = run_parsync_s3(
        "s3://nonexistent-bucket/",
        destination.path(),
        &minio.endpoint,
        &[],
    );

    assert!(result.is_err());

    Ok(())
}

#[test]
#[ignore = "requires docker"]
fn e2e_pull_from_s3_empty_prefix() -> Result<()> {
    if !docker_available() || !aws_available() {
        return Ok(());
    }

    let minio = start_minio()?;
    create_bucket(&minio.endpoint)?;

    let fixture = TempDir::new()?;
    fs::write(fixture.path().join("top.txt"), b"top-level")?;
    upload_file(&minio.endpoint, &fixture.path().join("top.txt"), "top.txt")?;

    let destination = TempDir::new()?;
    run_parsync_s3(
        &format!("s3://{TEST_BUCKET}"),
        destination.path(),
        &minio.endpoint,
        &[],
    )?;

    assert_eq!(fs::read(destination.path().join("top.txt"))?, b"top-level");

    Ok(())
}
