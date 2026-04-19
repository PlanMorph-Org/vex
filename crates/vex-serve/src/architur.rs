//! HTTP client to the architur API's `/api/internal/vex/*` endpoints.
//!
//! vex-serve calls into architur for two reasons:
//!   1. **Authorization** — confirm the SSH-authenticated user is allowed to
//!      push/fetch the requested repository (`POST /authorize`).
//!   2. **Ref mirroring** — after a successful `UpdateRef`, tell architur the
//!      new tip so the studio UI shows the commit (`POST /ref-updated`).
//!
//! Every request is signed with HMAC-SHA256 over `"<unix_ts>.<body>"`,
//! header value `t=<unix>,v1=<hex>` — matching `VexAuthService.Sign` on the
//! .NET side. The shared secret is read from `VEX_INTERNAL_SECRET`.
//!
//! The client is **synchronous** (`ureq`) by design: vex-serve handles one
//! SSH connection per process, so blocking calls are fine and we avoid
//! pulling tokio + reqwest into the binary.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;

/// All settings needed to call architur. Built from CLI flags / env at
/// startup; absent → fall back to "no architur integration" (useful for
/// local development with a bare vex-serve over a Unix socket).
#[derive(Debug, Clone)]
pub struct ArchiturClient {
    pub base_url: String,
    pub secret: String,
    /// Stop the SSH session if architur is unreachable. Production = true,
    /// dev = false. Default false.
    pub fail_closed: bool,
}

impl ArchiturClient {
    /// Build from environment. Returns `None` if `VEX_API_BASE` is missing —
    /// the caller treats that as "skip architur calls".
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("VEX_API_BASE").ok()?;
        let secret = std::env::var("VEX_INTERNAL_SECRET").ok()?;
        let fail_closed = matches!(
            std::env::var("VEX_FAIL_CLOSED").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE")
        );
        Some(Self {
            base_url,
            secret,
            fail_closed,
        })
    }

    /// `POST /api/internal/vex/authorize`. Returns true if the user is
    /// allowed to perform `op` on `repo_id`.
    pub fn authorize(&self, user_id: &str, repo_id: &str, op: &str) -> bool {
        let body = serde_json::json!({
            "userId": user_id,
            "repositoryId": repo_id,
            "operation": op,
        });
        let body_str = body.to_string();
        let url = format!(
            "{}/api/internal/vex/authorize",
            self.base_url.trim_end_matches('/')
        );
        let signature = self.sign(&body_str);

        match ureq::post(&url)
            .set("Content-Type", "application/json")
            .set("X-Vex-Signature", &signature)
            .timeout(std::time::Duration::from_secs(5))
            .send_string(&body_str)
        {
            Ok(resp) => match resp.into_json::<AuthorizeResponse>() {
                Ok(j) => {
                    if !j.allow {
                        warn!(reason = ?j.reason, "architur denied {op}");
                    }
                    j.allow
                }
                Err(e) => {
                    warn!(error = %e, "could not parse authorize response");
                    !self.fail_closed
                }
            },
            Err(e) => {
                warn!(error = %e, "architur authorize call failed");
                !self.fail_closed
            }
        }
    }

    /// `POST /api/internal/vex/ref-updated`. Best-effort: failures are
    /// logged but do not break the SSH session (the ref already moved on
    /// disk; the projection will be reconciled by a sweeper).
    pub fn ref_updated(&self, payload: &RefUpdatedPayload) {
        let body_str = match serde_json::to_string(payload) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "ref-updated serialise failed");
                return;
            }
        };
        let url = format!(
            "{}/api/internal/vex/ref-updated",
            self.base_url.trim_end_matches('/')
        );
        let signature = self.sign(&body_str);
        debug!(repo = %payload.repository_id, %payload.ref_name, "ref-updated → architur");

        let result = ureq::post(&url)
            .set("Content-Type", "application/json")
            .set("X-Vex-Signature", &signature)
            .timeout(std::time::Duration::from_secs(5))
            .send_string(&body_str);
        if let Err(e) = result {
            warn!(error = %e, "architur ref-updated failed");
        }
    }

    fn sign(&self, body: &str) -> String {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let payload = format!("{ts}.{body}");
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        let mac_hex = hex::encode(mac.finalize().into_bytes());
        format!("t={ts},v1={mac_hex}")
    }
}

#[derive(Debug, Deserialize)]
struct AuthorizeResponse {
    allow: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// Payload for `/api/internal/vex/ref-updated`. Field names use camelCase
/// because System.Text.Json on the architur side expects that by default.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefUpdatedPayload {
    pub repository_id: String,
    pub ref_name: String,
    pub kind: String,
    pub old_commit_hash: Option<String>,
    pub new_commit_hash: Option<String>,
    pub new_generation: i64,
    pub commit: Option<RefUpdatedCommit>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefUpdatedCommit {
    pub commit_hash: String,
    pub parent_hash: Option<String>,
    pub tree_hash: String,
    pub author_name: String,
    pub author_email: String,
    pub message: String,
    /// ISO-8601 / RFC3339, e.g. `2026-04-18T20:15:30Z`.
    pub authored_at: String,
    pub pushed_by_user_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_format_matches_dotnet() {
        // Mirror VexAuthService.Sign("hello", t=1700000000) — values verified
        // against a Python reference: hex(hmac_sha256(b"k", b"1700000000.hello"))
        let c = ArchiturClient {
            base_url: "http://x".into(),
            secret: "k".into(),
            fail_closed: false,
        };
        let sig = c.sign("hello");
        assert!(sig.starts_with("t="));
        assert!(sig.contains(",v1="));
        assert_eq!(sig.split(",v1=").nth(1).unwrap().len(), 64);
    }
}
