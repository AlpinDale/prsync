use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use aws_types::region::Region;
use tokio::runtime::Runtime;

use crate::remote::{EntryKind, RemoteClient, RemoteEntry, RemoteFileStat, S3Spec};

/// S3 backend implementing `RemoteClient`.
///
/// Bridges the async AWS SDK into parsync's synchronous rayon pipeline via an
/// owned tokio `Runtime`. Each trait method calls `self.runtime.block_on(...)`,
/// which is safe because rayon worker threads are not tokio contexts.
pub struct S3Remote {
    client: Client,
    bucket: String,
    prefix: String,
    prefix_trailing_star: bool,
    runtime: Arc<Runtime>,
}

impl S3Remote {
    pub fn connect(spec: S3Spec) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to create tokio runtime for S3")?;

        let client = runtime.block_on(async {
            let mut loader = aws_config::defaults(BehaviorVersion::latest());
            if let Some(ref region) = spec.region {
                loader = loader.region(Region::new(region.clone()));
            }
            if let Some(ref profile) = spec.profile {
                loader = loader.profile_name(profile);
            }
            if let Some(ref endpoint) = spec.endpoint_url {
                loader = loader.endpoint_url(endpoint);
            }
            let config = loader.load().await;

            let mut s3_config_builder = aws_sdk_s3::config::Builder::from(&config);
            // Use path-style addressing for MinIO/LocalStack compatibility.
            if spec.endpoint_url.is_some() {
                s3_config_builder = s3_config_builder.force_path_style(true);
            }
            Client::from_conf(s3_config_builder.build())
        });

        // Verify bucket is accessible.
        runtime
            .block_on(client.head_bucket().bucket(&spec.bucket).send())
            .with_context(|| format!("S3 bucket '{}' is not accessible", spec.bucket))?;

        Ok(Self {
            client,
            bucket: spec.bucket,
            prefix: spec.prefix,
            prefix_trailing_star: spec.prefix_trailing_star,
            runtime: Arc::new(runtime),
        })
    }

    /// Build the full S3 key prefix for listing, with trailing slash for
    /// directory-like semantics.
    fn listing_prefix(&self) -> String {
        if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        }
    }

    /// Strip the listing prefix from an S3 key to produce a relative path.
    fn relative_path(&self, key: &str) -> PathBuf {
        let prefix = self.listing_prefix();
        let stripped = key.strip_prefix(&prefix).unwrap_or(key);
        PathBuf::from(stripped)
    }

    /// Produce the full S3 key for a relative path.
    fn full_key(&self, relative_path: &Path) -> String {
        let prefix = self.listing_prefix();
        format!("{}{}", prefix, relative_path.display())
    }
}

impl RemoteClient for S3Remote {
    fn supports_block_delta(&self) -> bool {
        true
    }

    fn list_entries(&self, recursive: bool) -> Result<Vec<RemoteEntry>> {
        self.list_entries_with_progress(recursive, None)
    }

    fn list_entries_with_progress(
        &self,
        recursive: bool,
        progress: Option<&dyn Fn(usize)>,
    ) -> Result<Vec<RemoteEntry>> {
        let prefix = self.listing_prefix();

        self.runtime.block_on(async {
            let mut entries = Vec::new();
            let mut continuation_token: Option<String> = None;

            loop {
                let mut req = self
                    .client
                    .list_objects_v2()
                    .bucket(&self.bucket)
                    .prefix(&prefix);

                if !recursive {
                    req = req.delimiter("/");
                }

                if let Some(ref token) = continuation_token {
                    req = req.continuation_token(token);
                }

                let resp = req
                    .send()
                    .await
                    .with_context(|| format!("ListObjectsV2 bucket={} prefix={}", self.bucket, prefix))?;

                // Objects (files)
                for obj in resp.contents() {
                    let key = obj.key().unwrap_or("");
                    // Skip "directory marker" keys (keys ending with /)
                    if key.ends_with('/') {
                        continue;
                    }

                    let relative = self.relative_path(key);
                    if relative.as_os_str().is_empty() {
                        continue;
                    }

                    let mtime_secs = obj
                        .last_modified()
                        .map(|t| t.secs())
                        .unwrap_or(0);

                    entries.push(RemoteEntry {
                        relative_path: relative,
                        kind: EntryKind::File,
                        size: obj.size().unwrap_or(0) as u64,
                        mtime_secs,
                        mode: 0o644,
                        uid: None,
                        gid: None,
                        link_target: None,
                    });
                }

                // Common prefixes (directories) — only in non-recursive mode
                if !recursive {
                    for cp in resp.common_prefixes() {
                        if let Some(p) = cp.prefix() {
                            let dir_key = p.trim_end_matches('/');
                            let relative = self.relative_path(dir_key);
                            if relative.as_os_str().is_empty() {
                                continue;
                            }
                            entries.push(RemoteEntry {
                                relative_path: relative,
                                kind: EntryKind::Dir,
                                size: 0,
                                mtime_secs: 0,
                                mode: 0o755,
                                uid: None,
                                gid: None,
                                link_target: None,
                            });
                        }
                    }
                }

                if let Some(cb) = &progress {
                    cb(entries.len());
                }

                if resp.is_truncated() == Some(true) {
                    continuation_token = resp.next_continuation_token().map(String::from);
                } else {
                    break;
                }
            }

            Ok(entries)
        })
    }

    fn read_range(&self, relative_path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
        let key = self.full_key(relative_path);
        let range = format!("bytes={}-{}", offset, offset + len - 1);

        self.runtime.block_on(async {
            let resp = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .range(&range)
                .send()
                .await
                .with_context(|| {
                    format!("GetObject bucket={} key={} range={}", self.bucket, key, range)
                })?;

            let body = resp
                .body
                .collect()
                .await
                .context("reading S3 GetObject response body")?;

            Ok(body.into_bytes().to_vec())
        })
    }

    fn stat_file(&self, relative_path: &Path) -> Result<RemoteFileStat> {
        let key = self.full_key(relative_path);

        self.runtime.block_on(async {
            let resp = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .with_context(|| format!("HeadObject bucket={} key={}", self.bucket, key))?;

            let mtime_secs = resp
                .last_modified()
                .map(|t| t.secs())
                .unwrap_or(0);

            Ok(RemoteFileStat {
                size: resp.content_length().unwrap_or(0).max(0) as u64,
                mtime_secs,
            })
        })
    }
}

// S3Remote is safe to share across rayon threads: the AWS Client is Clone+Send+Sync,
// and the tokio Runtime is behind an Arc.
unsafe impl Sync for S3Remote {}

#[cfg(test)]
mod tests {
    use crate::remote::SourceSpec;

    #[test]
    fn parse_s3_basic() {
        let spec = SourceSpec::parse("s3://my-bucket/data/subdir", None, None, None).unwrap();
        match spec {
            SourceSpec::S3(s) => {
                assert_eq!(s.bucket, "my-bucket");
                assert_eq!(s.prefix, "data/subdir");
                assert!(!s.prefix_trailing_star);
            }
            _ => panic!("expected S3 spec"),
        }
    }

    #[test]
    fn parse_s3_trailing_star() {
        let spec = SourceSpec::parse("s3://bucket/prefix/*", None, None, None).unwrap();
        match spec {
            SourceSpec::S3(s) => {
                assert_eq!(s.bucket, "bucket");
                assert_eq!(s.prefix, "prefix");
                assert!(s.prefix_trailing_star);
            }
            _ => panic!("expected S3 spec"),
        }
    }

    #[test]
    fn parse_s3_bucket_only() {
        let spec = SourceSpec::parse("s3://my-bucket/", None, None, None).unwrap();
        match spec {
            SourceSpec::S3(s) => {
                assert_eq!(s.bucket, "my-bucket");
                assert_eq!(s.prefix, "");
                assert!(!s.prefix_trailing_star);
            }
            _ => panic!("expected S3 spec"),
        }
    }

    #[test]
    fn parse_s3_bucket_no_slash() {
        let spec = SourceSpec::parse("s3://my-bucket", None, None, None).unwrap();
        match spec {
            SourceSpec::S3(s) => {
                assert_eq!(s.bucket, "my-bucket");
                assert_eq!(s.prefix, "");
            }
            _ => panic!("expected S3 spec"),
        }
    }

    #[test]
    fn parse_s3_with_options() {
        let spec = SourceSpec::parse(
            "s3://bucket/key",
            Some("us-west-2"),
            Some("http://localhost:9000"),
            Some("myprofile"),
        )
        .unwrap();
        match spec {
            SourceSpec::S3(s) => {
                assert_eq!(s.region.as_deref(), Some("us-west-2"));
                assert_eq!(s.endpoint_url.as_deref(), Some("http://localhost:9000"));
                assert_eq!(s.profile.as_deref(), Some("myprofile"));
            }
            _ => panic!("expected S3 spec"),
        }
    }

    #[test]
    fn parse_s3_empty_bucket_fails() {
        assert!(SourceSpec::parse("s3://", None, None, None).is_err());
        assert!(SourceSpec::parse("s3:///prefix", None, None, None).is_err());
    }

    #[test]
    fn parse_ssh_unchanged() {
        let spec = SourceSpec::parse("user@host:/path", None, None, None).unwrap();
        match spec {
            SourceSpec::Ssh(s) => {
                assert_eq!(s.user.as_deref(), Some("user"));
                assert_eq!(s.host, "host");
                assert_eq!(s.path, "/path");
            }
            _ => panic!("expected SSH spec"),
        }
    }
}
