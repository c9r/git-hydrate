//! `git hydrate pull`, which downloads objects into the working tree.

use std::path::Path;

use anyhow::{Result, bail};
use futures::{StreamExt, stream};

use crate::config;
use crate::git::Repo;
use crate::progress::Progress;
use crate::remote::Remote;
use crate::tracked::{self, WorkState, human_bytes, tracked_pointers};
use crate::transfer;

pub struct Options {
    pub pathspecs: Vec<String>,
    pub all: bool,
    pub quiet: bool,
}

pub async fn run(repo: &Repo, cwd: &Path, options: Options) -> Result<()> {
    if !options.all && options.pathspecs.is_empty() {
        bail!("name the paths to hydrate, or pass --all");
    }
    if options.all && !options.pathspecs.is_empty() {
        bail!("--all hydrates every tracked file, so it takes no paths");
    }
    let (settings, _) = config::resolve(repo)?;
    let files = if options.all {
        tracked_pointers(repo, &repo.toplevel, &[])?
    } else {
        tracked_pointers(repo, cwd, &options.pathspecs)?
    };
    let progress = Progress::new(options.quiet);

    let mut todo = Vec::new();
    let mut already = 0usize;
    for file in files {
        match tracked::work_state(&repo.toplevel, &file)? {
            WorkState::Dehydrated | WorkState::Missing => todo.push(file),
            WorkState::Hydrated => already += 1,
            WorkState::PointerMismatch => {
                progress.println(&format!("{}: skipped, the file is a pointer for a different object", file.path))
            }
        }
    }
    if todo.is_empty() {
        if !options.quiet {
            progress.println(&format!("nothing to hydrate, {already} files already hydrated"));
        }
        return Ok(());
    }

    let remote = Remote::open(&settings).await?;
    let results: Vec<(String, Result<u64>)> = stream::iter(todo)
        .map(|file| {
            let remote = &remote;
            let progress = &progress;
            let repo = &repo;
            let partsize = settings.partsize;
            async move {
                let bar = progress.bytes(&file.path, file.pointer.size);
                let dest = repo.toplevel.join(&file.path);
                let result = transfer::download(remote, &file.pointer, &dest, file.executable, partsize, &bar).await;
                bar.finish_and_clear();
                (file.path, result.map(|()| file.pointer.size))
            }
        })
        .buffer_unordered(settings.concurrency)
        .collect()
        .await;

    let mut hydrated = Vec::new();
    let mut bytes = 0u64;
    let mut failures = Vec::new();
    for (path, result) in results {
        match result {
            Ok(size) => {
                hydrated.push(path);
                bytes += size;
            }
            Err(err) => failures.push((path, err)),
        }
    }
    if !hydrated.is_empty() {
        repo.refresh_index(&hydrated)?;
    }
    if !options.quiet {
        progress.println(&format!("hydrated {} files, {}", hydrated.len(), human_bytes(bytes)));
    }
    for (path, err) in &failures {
        eprintln!("git-hydrate: {path}: {err:#}");
    }
    if !failures.is_empty() {
        bail!("{} files could not be hydrated", failures.len());
    }
    Ok(())
}
