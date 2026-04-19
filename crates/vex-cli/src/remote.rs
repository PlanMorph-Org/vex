//! Remote configuration: parsing SSH URLs and persisting `[remote.*]` entries
//! in the on-disk repo config (`.vex/remotes.toml`).
//!
//! The format is intentionally toml-of-tables so adding fields later
//! (e.g. signing requirements, alternate transports) is forward-compatible.
//!
//! ```toml
//! [remote.origin]
//! url = "ssh://git@vex.planmorph.app:2222/acme/tower"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

pub(crate) const REMOTES_FILE: &str = "remotes.toml";

/// Parsed `ssh://[user@]host[:port]/<org>/<project>` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshUrl {
    pub user: String,
    pub host: String,
    pub port: u16,
    /// Path passed to the remote `vex-serve`, e.g. `acme/tower`.
    pub repo: String,
}

impl SshUrl {
    pub(crate) fn parse(s: &str) -> Result<Self> {
        let rest = s
            .strip_prefix("ssh://")
            .ok_or_else(|| anyhow!("remote URL must start with ssh://"))?;
        // Split host-part from path-part at the first `/`.
        let (hostpart, path) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("URL is missing a repository path"))?;
        let (user, hostport) = match hostpart.split_once('@') {
            Some((u, h)) => (u.to_string(), h),
            None => ("git".to_string(), hostpart),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>().context("invalid port in URL")?,
            ),
            None => (hostport.to_string(), 22u16),
        };
        if host.is_empty() {
            bail!("URL is missing a host");
        }
        let repo = path.trim_end_matches('/').to_string();
        validate_repo_path(&repo)?;
        Ok(Self { user, host, port, repo })
    }
}

fn validate_repo_path(p: &str) -> Result<()> {
    if p.is_empty() || p.contains("..") || p.starts_with('/') {
        bail!("invalid repo path: {p}");
    }
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|c| c.is_empty()) {
        bail!("repo path must be <org>/<project>");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RemoteEntry {
    pub url: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct RemotesFile {
    #[serde(default)]
    remote: BTreeMap<String, RemoteEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteStore {
    path: PathBuf,
    entries: BTreeMap<String, RemoteEntry>,
}

impl RemoteStore {
    /// Open the remotes file inside `<repo>/.vex/`. Creates the file lazily.
    pub(crate) fn open(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(".vex").join(REMOTES_FILE);
        let entries = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let parsed: RemotesFile = toml::from_str(&raw)
                .with_context(|| format!("parse {}", path.display()))?;
            parsed.remote
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, entries })
    }

    pub(crate) fn list(&self) -> impl Iterator<Item = (&String, &RemoteEntry)> {
        self.entries.iter()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&RemoteEntry> {
        self.entries.get(name)
    }

    pub(crate) fn add(&mut self, name: &str, url: &str) -> Result<()> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("invalid remote name: {name}");
        }
        if self.entries.contains_key(name) {
            bail!("remote `{name}` already exists");
        }
        // Validate the URL up-front so the file never holds garbage.
        let _ = SshUrl::parse(url)?;
        self.entries.insert(name.to_string(), RemoteEntry { url: url.to_string() });
        self.persist()
    }

    pub(crate) fn remove(&mut self, name: &str) -> Result<()> {
        if self.entries.remove(name).is_none() {
            bail!("no such remote: {name}");
        }
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let body = toml::to_string_pretty(&RemotesFile { remote: self.entries.clone() })
            .context("serialize remotes.toml")?;
        std::fs::write(&self.path, body)
            .with_context(|| format!("write {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_ssh_url() {
        let u = SshUrl::parse("ssh://alice@vex.example.com:2222/acme/tower").unwrap();
        assert_eq!(u.user, "alice");
        assert_eq!(u.host, "vex.example.com");
        assert_eq!(u.port, 2222);
        assert_eq!(u.repo, "acme/tower");
    }

    #[test]
    fn parse_defaults_user_and_port() {
        let u = SshUrl::parse("ssh://vex.example.com/acme/tower").unwrap();
        assert_eq!(u.user, "git");
        assert_eq!(u.port, 22);
    }

    #[test]
    fn rejects_bad_repo_paths() {
        assert!(SshUrl::parse("ssh://h/only-one").is_err());
        assert!(SshUrl::parse("ssh://h/a/b/c").is_err());
        assert!(SshUrl::parse("ssh://h//acme/tower").is_err());
        assert!(SshUrl::parse("ssh://h/../escape").is_err());
    }

    #[test]
    fn rejects_non_ssh_scheme() {
        assert!(SshUrl::parse("https://h/a/b").is_err());
    }

    #[test]
    fn add_list_remove_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".vex")).unwrap();
        let mut s = RemoteStore::open(tmp.path()).unwrap();
        s.add("origin", "ssh://git@example.com/acme/tower").unwrap();
        assert!(s.add("origin", "ssh://git@example.com/acme/tower").is_err());
        let s2 = RemoteStore::open(tmp.path()).unwrap();
        assert_eq!(s2.list().count(), 1);
        let mut s3 = RemoteStore::open(tmp.path()).unwrap();
        s3.remove("origin").unwrap();
        assert_eq!(RemoteStore::open(tmp.path()).unwrap().list().count(), 0);
    }
}
