//! `git hydrate push`, which uploads the objects the index points to.
//!
//! The pre-push hook uses the same upload, over the pointers in the commits
//! being pushed rather than the index.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use futures::{StreamExt, stream};

use crate::commands::head_all;
use crate::config::{self, Settings};
use crate::git::Repo;
use crate::pointer::Pointer;
use crate::progress::Progress;
use crate::remote::Remote;
use crate::tracked::{self, Tracked, WorkState, human_bytes, tracked_pointers};

pub struct Options {
    pub pathspecs: Vec<String>,
    pub quiet: bool,
}

/// An object the remote lacks, with the paths that could supply it.
pub struct Wanted {
    pub pointer: Pointer,
    pub paths: Vec<String>,
}

/// Why an object could not be uploaded.
pub struct Failure {
    pub pointer: Pointer,
    pub paths: Vec<String>,
    pub reason: String,
}

pub async fn run(repo: &Repo, cwd: &Path, options: Options) -> Result<()> {
    let (settings, _) = config::resolve(repo)?;
    let files = if options.pathspecs.is_empty() {
        tracked_pointers(repo, &repo.toplevel, &[])?
    } else {
        tracked_pointers(repo, cwd, &options.pathspecs)?
    };
    let mut wanted = group_by_object(files);
    let remote = Remote::open(&settings).await?;
    let present = head_all(&remote, &wanted.iter().map(|w| w.pointer.oid.clone()).collect::<Vec<_>>()).await?;
    wanted.retain(|w| !matches!(present.get(&w.pointer.oid), Some(Some(size)) if *size == w.pointer.size));
    let progress = Progress::new(options.quiet);
    let outcome = upload(repo, &remote, &settings, wanted, &progress).await?;
    if !options.quiet {
        progress.println(&format!("uploaded {} objects, {}", outcome.uploaded, human_bytes(outcome.bytes)));
    }
    for failure in &outcome.failures {
        eprintln!("git-hydrate: {}: {}", failure.paths.join(", "), failure.reason);
    }
    if !outcome.failures.is_empty() {
        bail!("{} objects could not be uploaded", outcome.failures.len());
    }
    Ok(())
}

/// Collapses tracked files onto their objects, keeping every path that names each one.
pub fn group_by_object(files: Vec<Tracked>) -> Vec<Wanted> {
    let mut by_oid: HashMap<String, Wanted> = HashMap::new();
    for file in files {
        by_oid
            .entry(file.pointer.oid.clone())
            .or_insert_with(|| Wanted { pointer: file.pointer.clone(), paths: Vec::new() })
            .paths
            .push(file.path);
    }
    let mut wanted: Vec<Wanted> = by_oid.into_values().collect();
    wanted.sort_by(|a, b| a.paths.cmp(&b.paths));
    wanted
}

pub struct Outcome {
    pub uploaded: usize,
    pub bytes: u64,
    pub failures: Vec<Failure>,
}

/// Uploads each wanted object from the first path in the working tree that holds it.
pub async fn upload(
    repo: &Repo,
    remote: &Remote,
    settings: &Settings,
    wanted: Vec<Wanted>,
    progress: &Progress,
) -> Result<Outcome> {
    let results: Vec<Result<u64, Failure>> = stream::iter(wanted)
        .map(|want| async move {
            let mut reasons = Vec::new();
            for path in &want.paths {
                let file = Tracked { path: path.clone(), executable: false, pointer: want.pointer.clone() };
                match tracked::work_state(&repo.toplevel, &file) {
                    Ok(WorkState::Hydrated) => {}
                    Ok(_) => continue,
                    Err(err) => {
                        reasons.push(format!("{path}: {err}"));
                        continue;
                    }
                }
                let bar = progress.bytes(path, want.pointer.size);
                let result =
                    remote.put_file(&want.pointer.oid, want.pointer.size, &repo.toplevel.join(path), &bar).await;
                bar.finish_and_clear();
                match result {
                    Ok(()) => return Ok(want.pointer.size),
                    Err(err) => reasons.push(format!("{path}: {err:#}")),
                }
            }
            let reason = if reasons.is_empty() {
                format!("object {} is in neither the remote nor the working tree", want.pointer.oid)
            } else {
                format!("object {} could not be uploaded: {}", want.pointer.oid, reasons.join("; "))
            };
            Err(Failure { pointer: want.pointer, paths: want.paths, reason })
        })
        .buffer_unordered(settings.concurrency)
        .collect()
        .await;

    let mut outcome = Outcome { uploaded: 0, bytes: 0, failures: Vec::new() };
    for result in results {
        match result {
            Ok(size) => {
                outcome.uploaded += 1;
                outcome.bytes += size;
            }
            Err(failure) => outcome.failures.push(failure),
        }
    }
    outcome.failures.sort_by(|a, b| a.paths.cmp(&b.paths));
    Ok(outcome)
}
