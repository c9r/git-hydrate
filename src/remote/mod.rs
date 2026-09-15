//! The object store, which holds every object under its oid and never modifies one.

pub mod local;
pub mod s3;

use std::path::Path;

use anyhow::Result;
use bytes::Bytes;
use indicatif::ProgressBar;

use crate::config::{RemoteSpec, Settings};

/// An open connection to the object store named by the settings.
pub enum Remote {
    S3(s3::S3Remote),
    Local(local::LocalRemote),
}

impl Remote {
    /// Opens the remote the settings name.
    pub async fn open(settings: &Settings) -> Result<Remote> {
        match &settings.remote {
            RemoteSpec::S3 { bucket, prefix } => Ok(Remote::S3(s3::S3Remote::open(settings, bucket, prefix).await?)),
            RemoteSpec::Local { root } => Ok(Remote::Local(local::LocalRemote::open(root)?)),
        }
    }

    /// The remote as a person would name it.
    pub fn describe(&self) -> String {
        match self {
            Remote::S3(remote) => remote.describe(),
            Remote::Local(remote) => remote.describe(),
        }
    }

    /// The size of an object if it exists.
    pub async fn head(&self, oid: &str) -> Result<Option<u64>> {
        match self {
            Remote::S3(remote) => remote.head(oid).await,
            Remote::Local(remote) => remote.head(oid).await,
        }
    }

    /// A byte range of an object. The range must lie inside the object.
    pub async fn get_range(&self, oid: &str, start: u64, len: u64) -> Result<Bytes> {
        match self {
            Remote::S3(remote) => remote.get_range(oid, start, len).await,
            Remote::Local(remote) => remote.get_range(oid, start, len).await,
        }
    }

    /// Stores a file as the object `oid`, verifying that its bytes hash to `oid` as they are read.
    ///
    /// An object that already exists is left as it is. The bar advances as bytes leave.
    pub async fn put_file(&self, oid: &str, size: u64, path: &Path, bar: &ProgressBar) -> Result<()> {
        match self {
            Remote::S3(remote) => remote.put_file(oid, size, path, bar).await,
            Remote::Local(remote) => remote.put_file(oid, size, path, bar).await,
        }
    }
}
