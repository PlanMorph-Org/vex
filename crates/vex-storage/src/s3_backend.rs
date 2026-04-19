//! S3-compatible object backend (AWS S3, `DigitalOcean` Spaces, `MinIO`, R2).
//!
//! Available with the `s3-backend` feature. Uses `aws-sdk-s3` under a small
//! Tokio runtime so the backend trait stays synchronous.
//!
//! Object keys are sharded by the first 4 hex characters of the Blake3 hash:
//! `{prefix}objects/aa/bb/<rest_of_hash>.bin`. The `.bin` payload is the
//! exact framed bytes from [`crate::object::encode`] (already zstd-compressed
//! internally; we do not double-compress).

use std::sync::Arc;

use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::Delete;
use aws_sdk_s3::Client;
use tokio::runtime::Runtime;
use vex_utils::{Hash256, VexError, VexResult};

use crate::backend::ObjectBackend;

/// Configuration for an S3-compatible endpoint.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Bucket name. Required.
    pub bucket: String,
    /// Custom endpoint URL (e.g. `https://fra1.digitaloceanspaces.com`).
    /// `None` means "default AWS S3".
    pub endpoint: Option<String>,
    /// Region. For non-AWS providers any non-empty value works
    /// (e.g. `"us-east-1"` for `DigitalOcean` Spaces).
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// Required for `MinIO` and other path-style services. Default `false`
    /// (virtual-hosted style) which is what AWS and DO Spaces use.
    pub path_style: bool,
    /// Key prefix prepended to every object. Use it for tenant isolation,
    /// e.g. `"tenants/<orgId>/repos/<repoId>/"`. Must end with `/` or be empty.
    pub key_prefix: String,
}

impl S3Config {
    fn validate(&self) -> VexResult<()> {
        if self.bucket.is_empty() {
            return Err(VexError::Config("S3Config.bucket is empty".into()));
        }
        if !self.key_prefix.is_empty() && !self.key_prefix.ends_with('/') {
            return Err(VexError::Config(
                "S3Config.key_prefix must end with '/'".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct S3ObjectBackend {
    client: Client,
    cfg: S3Config,
    rt: Arc<Runtime>,
}

impl S3ObjectBackend {
    /// Build a backend from an explicit configuration. Spawns a private
    /// multi-threaded Tokio runtime sized for I/O.
    pub fn new(cfg: S3Config) -> VexResult<Self> {
        cfg.validate()?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("vex-s3")
            .build()
            .map_err(|e| VexError::Storage(format!("tokio: {e}")))?;
        let rt = Arc::new(rt);
        let client = rt.block_on(build_client(&cfg))?;
        Ok(Self { client, cfg, rt })
    }

    /// Construct from an existing runtime handle (preferred when the host
    /// already drives Tokio, e.g. inside `vex-serve`).
    pub fn with_runtime(cfg: S3Config, rt: Arc<Runtime>) -> VexResult<Self> {
        cfg.validate()?;
        let client = rt.block_on(build_client(&cfg))?;
        Ok(Self { client, cfg, rt })
    }

    fn key_for(&self, hash: Hash256) -> String {
        let hex = hash.to_hex();
        // Defensive: a 32-byte Blake3 is always 64 hex chars.
        debug_assert!(hex.len() >= 4);
        let aa = &hex[0..2];
        let bb = &hex[2..4];
        let rest = &hex[4..];
        format!("{}objects/{}/{}/{}.bin", self.cfg.key_prefix, aa, bb, rest)
    }
}

async fn build_client(cfg: &S3Config) -> VexResult<Client> {
    let creds = Credentials::new(&cfg.access_key, &cfg.secret_key, None, None, "vex");
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new(cfg.region.clone()))
        .credentials_provider(creds);
    if let Some(ep) = &cfg.endpoint {
        loader = loader.endpoint_url(ep);
    }
    let shared = loader.load().await;
    let mut s3 = aws_sdk_s3::config::Builder::from(&shared);
    if cfg.path_style {
        s3 = s3.force_path_style(true);
    }
    Ok(Client::from_conf(s3.build()))
}

impl ObjectBackend for S3ObjectBackend {
    fn put(&self, hash: Hash256, framed: &[u8]) -> VexResult<()> {
        let key = self.key_for(hash);
        let body = ByteStream::from(framed.to_vec());
        let bucket = self.cfg.bucket.clone();
        let client = self.client.clone();
        self.rt.block_on(async move {
            client
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .send()
                .await
                .map_err(|e| VexError::Storage(format!("s3 put: {e}")))?;
            Ok::<_, VexError>(())
        })
    }

    fn get(&self, hash: Hash256) -> VexResult<Option<Vec<u8>>> {
        let key = self.key_for(hash);
        let bucket = self.cfg.bucket.clone();
        let client = self.client.clone();
        self.rt.block_on(async move {
            match client.get_object().bucket(bucket).key(key).send().await {
                Ok(out) => {
                    let bytes = out
                        .body
                        .collect()
                        .await
                        .map_err(|e| VexError::Storage(format!("s3 read: {e}")))?
                        .into_bytes()
                        .to_vec();
                    Ok(Some(bytes))
                }
                Err(e) => {
                    let svc = e.into_service_error();
                    if svc.is_no_such_key() {
                        Ok(None)
                    } else {
                        Err(VexError::Storage(format!("s3 get: {svc}")))
                    }
                }
            }
        })
    }

    fn has(&self, hash: Hash256) -> VexResult<bool> {
        let key = self.key_for(hash);
        let bucket = self.cfg.bucket.clone();
        let client = self.client.clone();
        self.rt.block_on(async move {
            match client.head_object().bucket(bucket).key(key).send().await {
                Ok(_) => Ok(true),
                Err(e) => {
                    let svc = e.into_service_error();
                    if svc.is_not_found() {
                        Ok(false)
                    } else {
                        Err(VexError::Storage(format!("s3 head: {svc}")))
                    }
                }
            }
        })
    }

    fn delete(&self, hash: Hash256) -> VexResult<bool> {
        // S3 DeleteObject is idempotent (returns success even if missing).
        // We do a HEAD first to report the boolean honestly.
        let existed = self.has(hash)?;
        if !existed {
            return Ok(false);
        }
        let key = self.key_for(hash);
        let bucket = self.cfg.bucket.clone();
        let client = self.client.clone();
        self.rt.block_on(async move {
            client
                .delete_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| VexError::Storage(format!("s3 delete: {e}")))?;
            Ok::<_, VexError>(())
        })?;
        Ok(true)
    }

    fn list_hashes(&self) -> VexResult<Vec<Hash256>> {
        let prefix = format!("{}objects/", self.cfg.key_prefix);
        let bucket = self.cfg.bucket.clone();
        let client = self.client.clone();
        self.rt.block_on(async move {
            let mut out = Vec::new();
            let mut continuation: Option<String> = None;
            loop {
                let mut req = client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(&prefix)
                    .max_keys(1000);
                if let Some(c) = &continuation {
                    req = req.continuation_token(c);
                }
                let page = req
                    .send()
                    .await
                    .map_err(|e| VexError::Storage(format!("s3 list: {e}")))?;
                for obj in page.contents() {
                    if let Some(key) = obj.key() {
                        if let Some(h) = parse_hash_from_key(key, &prefix) {
                            out.push(h);
                        }
                    }
                }
                if page.is_truncated().unwrap_or(false) {
                    continuation = page.next_continuation_token().map(str::to_owned);
                    if continuation.is_none() {
                        break;
                    }
                } else {
                    break;
                }
            }
            Ok(out)
        })
    }
}

impl S3ObjectBackend {
    /// Bulk delete for `gc`. Best-effort; falls back to per-object deletes
    /// if the multi-object endpoint is unsupported by the provider.
    pub fn delete_many(&self, hashes: &[Hash256]) -> VexResult<usize> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let bucket = self.cfg.bucket.clone();
        let client = self.client.clone();
        let keys: Vec<String> = hashes.iter().map(|h| self.key_for(*h)).collect();
        self.rt.block_on(async move {
            let mut deleted = 0usize;
            for chunk in keys.chunks(1000) {
                let objs: Vec<_> = chunk
                    .iter()
                    .map(|k| {
                        #[allow(clippy::expect_used)]
                        aws_sdk_s3::types::ObjectIdentifier::builder()
                            .key(k)
                            .build()
                            .expect("static key")
                    })
                    .collect();
                let del = Delete::builder()
                    .set_objects(Some(objs))
                    .build()
                    .map_err(|e| VexError::Storage(format!("s3 delete build: {e}")))?;
                let res = client
                    .delete_objects()
                    .bucket(&bucket)
                    .delete(del)
                    .send()
                    .await
                    .map_err(|e| VexError::Storage(format!("s3 delete_many: {e}")))?;
                deleted += res.deleted().len();
            }
            Ok(deleted)
        })
    }
}

fn parse_hash_from_key(key: &str, prefix: &str) -> Option<Hash256> {
    // <prefix>aa/bb/<60 hex chars>.bin
    let suffix = key.strip_prefix(prefix)?;
    let parts: Vec<&str> = suffix.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let last = parts[2].strip_suffix(".bin")?;
    let hex: String = format!("{}{}{}", parts[0], parts[1], last);
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let byte_str = hex.get(i * 2..i * 2 + 2)?;
        *byte = u8::from_str_radix(byte_str, 16).ok()?;
    }
    Some(Hash256::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::parse_hash_from_key;

    #[test]
    fn key_parser_roundtrips() {
        let hex = "ab".repeat(32);
        let key = format!("tenants/x/objects/ab/ab/{}.bin", &hex[4..]);
        let h = parse_hash_from_key(&key, "tenants/x/objects/").expect("parse");
        assert_eq!(h.to_hex(), hex);
    }

    #[test]
    fn key_parser_rejects_bad_shape() {
        assert!(
            parse_hash_from_key("tenants/x/objects/ab/ab/short.bin", "tenants/x/objects/")
                .is_none()
        );
        assert!(parse_hash_from_key("not/the/prefix/ab/ab/x.bin", "tenants/x/objects/").is_none());
    }
}
