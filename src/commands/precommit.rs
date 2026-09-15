//! `git hydrate pre-commit`, the hook that uploads objects before a commit exists.
//!
//! Every staged pointer's object is uploaded from the working tree before
//! the commit is made, so a commit's objects are in the remote the moment
//! the commit is. A staged file that no longer matches its pointer, because
//! it was changed after `git add`, refuses the commit rather than letting a
//! pointer to nothing be committed.

use std::collections::HashSet;

use anyhow::{Result, bail};

use crate::commands::head_all;
use crate::commands::push::{self, group_by_object};
use crate::config;
use crate::git::Repo;
use crate::progress::Progress;
use crate::remote::Remote;
use crate::tracked::{human_bytes, tracked_pointers};

pub async fn run(repo: &Repo, quiet: bool) -> Result<()> {
    let staged: HashSet<String> = repo.staged_paths()?.into_iter().collect();
    if staged.is_empty() {
        return Ok(());
    }
    let files: Vec<_> =
        tracked_pointers(repo, &repo.toplevel, &[])?.into_iter().filter(|f| staged.contains(&f.path)).collect();
    if files.is_empty() {
        return Ok(());
    }
    let mut wanted = group_by_object(files);
    let (settings, _) = config::resolve(repo)?;
    let remote = Remote::open(&settings).await?;
    let present = head_all(&remote, &wanted.iter().map(|w| w.pointer.oid.clone()).collect::<Vec<_>>()).await?;
    wanted.retain(|w| !matches!(present.get(&w.pointer.oid), Some(Some(size)) if *size == w.pointer.size));
    if wanted.is_empty() {
        return Ok(());
    }

    let progress = Progress::new(quiet);
    let outcome = push::upload(repo, &remote, &settings, wanted, &progress).await?;
    if !quiet && outcome.uploaded > 0 {
        progress.println(&format!(
            "git-hydrate: uploaded {} objects, {}",
            outcome.uploaded,
            human_bytes(outcome.bytes)
        ));
    }
    for failure in &outcome.failures {
        eprintln!("git-hydrate: {}: {}", failure.paths.join(", "), failure.reason);
    }
    if !outcome.failures.is_empty() {
        bail!(
            "{} staged objects could not be uploaded, so the commit was refused; stage the files again if they changed, or commit with --no-verify to upload at push",
            outcome.failures.len()
        );
    }
    Ok(())
}
