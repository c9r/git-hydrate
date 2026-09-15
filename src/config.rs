//! Configuration, which is git config under the `hydrate` section.
//!
//! A value is looked up in the repository's own config, then in the committed
//! `.hydrateconfig` at the root of the working tree, then in the global and
//! system config, so a repository commits its remote once and any machine
//! can override it locally.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git::{ConfigScope, ConfigType, Repo};

/// The committed config file's name.
pub const CONFIG_FILE: &str = ".hydrateconfig";

/// The default size in bytes of a transfer part, which is also the multipart upload part size.
pub const DEFAULT_PARTSIZE: u64 = 16 << 20;

/// The default number of files transferred at once.
pub const DEFAULT_CONCURRENCY: usize = 4;

/// The smallest part S3 accepts in a multipart upload, in bytes, except for the last part.
const MIN_PARTSIZE: u64 = 5 << 20;

/// The largest part S3 accepts in a multipart upload, in bytes.
const MAX_PARTSIZE: u64 = 5 << 30;

/// Where objects live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSpec {
    /// An S3 bucket, with a key prefix that is empty or ends with a slash.
    S3 { bucket: String, prefix: String },
    /// A directory whose files are named by oid.
    Local { root: PathBuf },
}

/// Everything the tool needs to reach the remote.
#[derive(Clone, Debug)]
pub struct Settings {
    pub remote: RemoteSpec,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub pathstyle: bool,
    /// Bytes per transfer part.
    pub partsize: u64,
    /// Files transferred at once.
    pub concurrency: usize,
}

/// One resolved value and where it came from, for `git hydrate env`.
#[derive(Clone, Debug)]
pub struct Origin {
    pub key: String,
    pub value: String,
    pub source: String,
}

/// Reads one `hydrate.*` key through the lookup order.
pub fn lookup(repo: &Repo, key: &str, kind: ConfigType) -> Result<Option<(String, String)>> {
    let full = format!("hydrate.{key}");
    if let Some(value) = repo.config_get(Some(ConfigScope::Local), None, kind, &full)? {
        return Ok(Some((value, "repository config".to_string())));
    }
    let file = repo.toplevel.join(CONFIG_FILE);
    if file.is_file()
        && let Some(value) = repo.config_get(None, Some(&file), kind, &full)?
    {
        return Ok(Some((value, CONFIG_FILE.to_string())));
    }
    if let Some(value) = repo.config_get(Some(ConfigScope::Global), None, kind, &full)? {
        return Ok(Some((value, "global config".to_string())));
    }
    if let Some(value) = repo.config_get(Some(ConfigScope::System), None, kind, &full)? {
        return Ok(Some((value, "system config".to_string())));
    }
    Ok(None)
}

/// Resolves the settings for a repository, recording where each value came from.
pub fn resolve(repo: &Repo) -> Result<(Settings, Vec<Origin>)> {
    let mut origins = Vec::new();
    let mut take = |key: &str, kind: ConfigType| -> Result<Option<String>> {
        Ok(lookup(repo, key, kind)?.map(|(value, source)| {
            origins.push(Origin { key: key.to_string(), value: value.clone(), source });
            value
        }))
    };

    let remote_url = take("remote", ConfigType::Text)?
        .with_context(|| format!("hydrate.remote is not set; put it in {CONFIG_FILE} or git config"))?;
    let remote = parse_remote(&remote_url)?;
    let endpoint = take("endpoint", ConfigType::Text)?;
    let region = take("region", ConfigType::Text)?;
    let profile = take("profile", ConfigType::Text)?;
    let pathstyle = take("pathstyle", ConfigType::Bool)?.map(|v| v == "true").unwrap_or(false);
    let partsize = match take("partsize", ConfigType::Int)? {
        Some(text) => text.parse::<u64>().context("hydrate.partsize is not a size")?,
        None => DEFAULT_PARTSIZE,
    };
    if !(MIN_PARTSIZE..=MAX_PARTSIZE).contains(&partsize) {
        bail!("hydrate.partsize must be between {MIN_PARTSIZE} and {MAX_PARTSIZE} bytes");
    }
    let concurrency = match take("concurrency", ConfigType::Int)? {
        Some(text) => text.parse::<usize>().context("hydrate.concurrency is not a count")?,
        None => DEFAULT_CONCURRENCY,
    };
    if concurrency == 0 {
        bail!("hydrate.concurrency must be at least one");
    }
    Ok((Settings { remote, endpoint, region, profile, pathstyle, partsize, concurrency }, origins))
}

/// Parses `s3://bucket/prefix` or `file:///path`.
pub fn parse_remote(url: &str) -> Result<RemoteSpec> {
    if let Some(rest) = url.strip_prefix("s3://") {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() {
            bail!("hydrate.remote {url} names no bucket");
        }
        let prefix = prefix.trim_matches('/');
        let prefix = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
        return Ok(RemoteSpec::S3 { bucket: bucket.to_string(), prefix });
    }
    if let Some(rest) = url.strip_prefix("file://") {
        let path = file_url_path(rest);
        if path.as_os_str().is_empty() || !path.is_absolute() {
            bail!("hydrate.remote {url} must name an absolute path");
        }
        return Ok(RemoteSpec::Local { root: path });
    }
    bail!("hydrate.remote {url} is neither s3:// nor file://")
}

#[cfg(not(windows))]
fn file_url_path(rest: &str) -> PathBuf {
    PathBuf::from(rest)
}

#[cfg(windows)]
fn file_url_path(rest: &str) -> PathBuf {
    let bytes = rest.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        PathBuf::from(&rest[1..])
    } else {
        PathBuf::from(rest)
    }
}

/// The path of the committed config file in a working tree.
pub fn config_file(toplevel: &Path) -> PathBuf {
    toplevel.join(CONFIG_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remotes() {
        assert_eq!(
            parse_remote("s3://bucket/some/prefix/").unwrap(),
            RemoteSpec::S3 { bucket: "bucket".into(), prefix: "some/prefix/".into() }
        );
        assert_eq!(
            parse_remote("s3://bucket").unwrap(),
            RemoteSpec::S3 { bucket: "bucket".into(), prefix: String::new() }
        );
        assert_eq!(
            parse_remote("s3://bucket/").unwrap(),
            RemoteSpec::S3 { bucket: "bucket".into(), prefix: String::new() }
        );
        assert!(parse_remote("s3://").is_err());
        assert!(parse_remote("https://example.com").is_err());
        assert!(parse_remote("file://relative").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn parses_file_remotes() {
        assert_eq!(parse_remote("file:///srv/objects").unwrap(), RemoteSpec::Local { root: "/srv/objects".into() });
    }
}
