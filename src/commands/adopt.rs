//! `git hydrate adopt`, which writes a pointer for an object already in the remote.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config;
use crate::git::Repo;
use crate::pointer::{Pointer, is_oid};
use crate::remote::Remote;
use crate::tracked::{FILTER_NAME, write_pointer};

pub struct Options {
    pub path: String,
    pub oid: String,
    pub size: u64,
    pub force: bool,
}

pub async fn run(repo: &Repo, cwd: &Path, options: Options) -> Result<()> {
    if !is_oid(&options.oid) {
        bail!("{} is not a lowercase hex sha256", options.oid);
    }
    let absolute = normalize(&cwd.join(&options.path));
    let relative = absolute
        .strip_prefix(&repo.toplevel)
        .with_context(|| format!("{} is outside the working tree", options.path))?
        .to_string_lossy()
        .replace('\\', "/");
    if relative.is_empty() {
        bail!("{} names the working tree itself", options.path);
    }

    let (settings, _) = config::resolve(repo)?;
    let remote = Remote::open(&settings).await?;
    match remote.head(&options.oid).await? {
        Some(size) if size == options.size => {}
        Some(size) => bail!("the remote holds {} at {size} bytes, not {}", options.oid, options.size),
        None => bail!("the remote does not hold {}", options.oid),
    }

    let mut executable = false;
    match std::fs::symlink_metadata(&absolute) {
        Ok(meta) if meta.is_file() => {
            executable = is_executable(&meta);
            let content = std::fs::read(&absolute).with_context(|| format!("reading {}", absolute.display()))?;
            if !Pointer::is_pointer(&content) && !options.force {
                bail!("{relative} exists and is not a pointer; pass --force to replace it");
            }
        }
        Ok(_) => bail!("{relative} exists and is not a regular file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", absolute.display())),
    }
    let pointer = Pointer { oid: options.oid.clone(), size: options.size };
    write_pointer(&absolute, &pointer, executable)?;

    let attribute = repo.filter_attribute(std::slice::from_ref(&relative))?;
    if attribute.first().and_then(|a| a.as_deref()) != Some(FILTER_NAME) {
        eprintln!(
            "git-hydrate: {relative} is not covered by a filter={FILTER_NAME} rule in .gitattributes, so git will not treat it as tracked"
        );
    }
    println!("{relative}: pointer written for {} ({} bytes)", options.oid, options.size);
    Ok(())
}

/// Resolves `.` and `..` lexically, which is enough to place a path under the top level.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}
