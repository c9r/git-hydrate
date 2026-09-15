//! `git hydrate drop` and `git dehydrate`, which replace files with their pointers.
//!
//! A file is dropped only when git agrees it matches the pointer in the index
//! and the remote holds the object, so nothing that exists only in this
//! working tree can be removed.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Result, bail};

use crate::commands::head_all;
use crate::config;
use crate::git::Repo;
use crate::remote::Remote;
use crate::tracked::{self, WorkState, human_bytes, tracked_pointers, write_pointer};

pub struct Options {
    pub pathspecs: Vec<String>,
    pub all: bool,
    pub quiet: bool,
}

pub async fn run(repo: &Repo, cwd: &Path, options: Options) -> Result<()> {
    if !options.all && options.pathspecs.is_empty() {
        bail!("name the paths to dehydrate, or pass --all");
    }
    if options.all && !options.pathspecs.is_empty() {
        bail!("--all dehydrates every tracked file, so it takes no paths");
    }
    let (settings, _) = config::resolve(repo)?;
    let files = if options.all {
        tracked_pointers(repo, &repo.toplevel, &[])?
    } else {
        tracked_pointers(repo, cwd, &options.pathspecs)?
    };

    let mut candidates = Vec::new();
    let mut refused: Vec<(String, String)> = Vec::new();
    for file in files {
        match tracked::work_state(&repo.toplevel, &file)? {
            WorkState::Hydrated => candidates.push(file),
            WorkState::Dehydrated | WorkState::Missing => {}
            WorkState::PointerMismatch => {
                refused.push((file.path, "the file is a pointer for a different object".to_string()))
            }
        }
    }
    if candidates.is_empty() && refused.is_empty() {
        if !options.quiet {
            eprintln!("nothing to dehydrate");
        }
        return Ok(());
    }

    let modified: HashSet<String> =
        repo.modified_paths(&candidates.iter().map(|f| f.path.clone()).collect::<Vec<_>>())?.into_iter().collect();
    let (clean, dirty): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|f| !modified.contains(&f.path));
    for file in dirty {
        refused.push((file.path, "the file has changes that are not committed".to_string()));
    }

    let remote = Remote::open(&settings).await?;
    let oids: Vec<String> = clean.iter().map(|f| f.pointer.oid.clone()).collect::<HashSet<_>>().into_iter().collect();
    let present = head_all(&remote, &oids).await?;

    let mut dropped = 0usize;
    let mut bytes = 0u64;
    let mut written = Vec::new();
    for file in clean {
        match present.get(&file.pointer.oid) {
            Some(Some(size)) if *size == file.pointer.size => {}
            Some(Some(size)) => {
                refused.push((
                    file.path,
                    format!("the remote holds the object at {size} bytes, not {}", file.pointer.size),
                ));
                continue;
            }
            _ => {
                refused.push((file.path, "the object is not in the remote; run git hydrate push first".to_string()));
                continue;
            }
        }
        write_pointer(&repo.toplevel.join(&file.path), &file.pointer, file.executable)?;
        dropped += 1;
        bytes += file.pointer.size;
        written.push(file.path);
    }
    if !written.is_empty() {
        repo.refresh_index(&written)?;
    }
    if !options.quiet {
        eprintln!("dehydrated {dropped} files, freed {}", human_bytes(bytes));
    }
    for (path, reason) in &refused {
        eprintln!("git-hydrate: {path}: not dehydrated, {reason}");
    }
    if !refused.is_empty() {
        bail!("{} files were not dehydrated", refused.len());
    }
    Ok(())
}
