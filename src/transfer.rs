//! Downloading an object into the working tree.
//!
//! Ranges are fetched in parallel and consumed in order, so the bytes are
//! hashed as they arrive and the file is written sequentially. The download
//! lands in a temporary file beside its destination and is renamed into place
//! only after the hash matches, so a destination path never holds a partial
//! or corrupt object.

use std::path::Path;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use indicatif::ProgressBar;
use tokio::io::AsyncWriteExt;

use crate::hash::Hasher;
use crate::pointer::Pointer;
use crate::remote::Remote;

/// Ranges in flight for one download.
const RANGES_IN_FLIGHT: usize = 4;

/// Downloads the object a pointer names to `dest`, replacing whatever is there.
pub async fn download(
    remote: &Remote,
    pointer: &Pointer,
    dest: &Path,
    executable: bool,
    partsize: u64,
    bar: &ProgressBar,
) -> Result<()> {
    let dir = dest.parent().context("the destination has no parent directory")?;
    tokio::fs::create_dir_all(dir).await.with_context(|| format!("creating {}", dir.display()))?;
    let temp = tempfile::Builder::new()
        .prefix(".hydrate-")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    let mut file = tokio::fs::File::from_std(temp.as_file().try_clone().context("opening the temporary file")?);

    let ranges: Vec<(u64, u64)> =
        (0..pointer.size).step_by(partsize as usize).map(|start| (start, partsize.min(pointer.size - start))).collect();
    let mut chunks = futures::stream::iter(ranges)
        .map(|(start, len)| remote.get_range(&pointer.oid, start, len))
        .buffered(RANGES_IN_FLIGHT);
    let mut hasher = Hasher::new();
    let mut received = 0u64;
    while let Some(bytes) = chunks.try_next().await? {
        hasher.update(&bytes);
        file.write_all(&bytes).await.context("writing the download")?;
        received += bytes.len() as u64;
        bar.inc(bytes.len() as u64);
    }
    file.flush().await.context("writing the download")?;
    file.sync_all().await.context("syncing the download")?;
    drop(file);

    let actual = hasher.finish();
    if actual != pointer.oid || received != pointer.size {
        bail!("the remote's object hashes to {actual} at {received} bytes, not {} at {}", pointer.oid, pointer.size);
    }
    set_executable(temp.path(), executable)?;
    temp.persist(dest).with_context(|| format!("placing {}", dest.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode of {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}
