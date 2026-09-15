//! A directory as the object store, one file per oid.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use indicatif::ProgressBar;

use crate::hash::Hasher;

/// The size of the buffer copies go through.
const BUFFER: usize = 1 << 20;

pub struct LocalRemote {
    root: PathBuf,
}

impl LocalRemote {
    pub fn open(root: &Path) -> Result<LocalRemote> {
        Ok(LocalRemote { root: root.to_path_buf() })
    }

    pub fn describe(&self) -> String {
        format!("file://{}", self.root.display())
    }

    fn object_path(&self, oid: &str) -> PathBuf {
        self.root.join(oid)
    }

    pub async fn head(&self, oid: &str) -> Result<Option<u64>> {
        match tokio::fs::metadata(self.object_path(oid)).await {
            Ok(meta) if meta.is_file() => Ok(Some(meta.len())),
            Ok(_) => bail!("{} exists but is not a file", self.object_path(oid).display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.object_path(oid).display())),
        }
    }

    pub async fn get_range(&self, oid: &str, start: u64, len: u64) -> Result<Bytes> {
        let path = self.object_path(oid);
        tokio::task::spawn_blocking(move || -> Result<Bytes> {
            let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
            let mut buf = vec![0u8; len as usize];
            read_exact_at(&file, &mut buf, start).with_context(|| format!("reading {}", path.display()))?;
            Ok(Bytes::from(buf))
        })
        .await
        .context("the read task failed")?
    }

    pub async fn put_file(&self, oid: &str, size: u64, path: &Path, bar: &ProgressBar) -> Result<()> {
        if let Some(existing) = self.head(oid).await? {
            if existing != size {
                bail!("the remote already holds {oid} at {existing} bytes, not {size}");
            }
            bar.inc(size);
            return Ok(());
        }
        let root = self.root.clone();
        let target = self.object_path(oid);
        let source = path.to_path_buf();
        let oid = oid.to_string();
        let bar = bar.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
            let mut file = File::open(&source).with_context(|| format!("opening {}", source.display()))?;
            let mut temp = tempfile::Builder::new()
                .prefix(".upload-")
                .tempfile_in(&root)
                .with_context(|| format!("creating a temporary file in {}", root.display()))?;
            let mut hasher = Hasher::new();
            let mut buf = vec![0u8; BUFFER];
            let mut copied = 0u64;
            loop {
                let n = file.read(&mut buf).with_context(|| format!("reading {}", source.display()))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                temp.write_all(&buf[..n]).context("writing the object")?;
                copied += n as u64;
                bar.inc(n as u64);
            }
            let actual = hasher.finish();
            if actual != oid || copied != size {
                bail!("{} hashes to {actual} at {copied} bytes, not {oid} at {size}", source.display());
            }
            temp.as_file().sync_all().context("syncing the object")?;
            temp.persist(&target).with_context(|| format!("placing {}", target.display()))?;
            Ok(())
        })
        .await
        .context("the copy task failed")?
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "object is shorter than expected"));
        }
        done += n;
    }
    Ok(())
}
