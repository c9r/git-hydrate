//! `git hydrate export`, which writes a tree into a plain directory, hydrated.
//!
//! The directory is not a repository. It records the commit it holds in a
//! marker file, and running the export again against a newer commit changes
//! only what the commit changed. Every pass walks the whole tree and makes
//! each path right, so an interrupted export is finished by running it again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config;
use crate::git::{Repo, TreeEntry};
use crate::hash;
use crate::pointer::{MAX_LEN, Pointer};
use crate::progress::Progress;
use crate::remote::Remote;
use crate::tracked::{human_bytes, set_executable, write_pointer};
use crate::transfer;

/// The file at the root of an exported directory that names the commit it holds.
pub const MARKER: &str = ".hydrate-commit";

pub struct Options {
    pub treeish: String,
    pub dir: PathBuf,
    pub only: Vec<String>,
    pub verify: bool,
    pub quiet: bool,
}

pub async fn run(repo: &Repo, options: Options) -> Result<()> {
    let record = match repo.resolve(&format!("{}^{{commit}}", options.treeish))? {
        Some(commit) => commit,
        None => repo
            .resolve(&format!("{}^{{tree}}", options.treeish))?
            .with_context(|| format!("{} does not name a commit or a tree", options.treeish))?,
    };
    let tree = repo.resolve(&format!("{record}^{{tree}}"))?.context("the commit has no tree")?;
    let (settings, _) = config::resolve(repo)?;
    let only = matcher(&options.only)?;
    let progress = Progress::new(options.quiet);

    let dir = &options.dir;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let marker = dir.join(MARKER);
    let previous = match std::fs::read_to_string(&marker) {
        Ok(text) => Some(text.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", marker.display())),
    };

    let mut removed = 0usize;
    if let Some(previous) = &previous
        && previous != &record
    {
        if repo.resolve(&format!("{previous}^{{tree}}"))?.is_none() {
            bail!(
                "{} records {previous}, which this repository no longer has, so the paths it dropped cannot be found; remove the marker to export without pruning them",
                marker.display()
            );
        }
        for path in repo.deleted_between(previous, &record)? {
            let target = dir.join(&path);
            match std::fs::remove_file(&target) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", target.display())),
            }
            prune_empty_parents(dir, &target);
        }
    }

    let entries = repo.ls_tree(&tree)?;
    let pointers = pointers_in(repo, &entries)?;

    let mut existing_to_hash: Vec<(usize, PathBuf)> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.kind == "blob" && !pointers.contains_key(&entry.object) && dir.join(&entry.path).is_file() {
            existing_to_hash.push((index, dir.join(&entry.path)));
        }
    }
    let existing_hashes =
        hash_files_as_git(repo, &existing_to_hash.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>())?;
    let matches_tree: HashMap<usize, bool> = existing_to_hash
        .iter()
        .zip(existing_hashes)
        .map(|((index, _), hash)| (*index, hash == entries[*index].object))
        .collect();

    let mut downloads: Vec<(TreeEntry, Pointer)> = Vec::new();
    let mut written = 0usize;
    let mut verified = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        let target = dir.join(&entry.path);
        let executable = entry.mode == "100755";
        match entry.mode.as_str() {
            "160000" => {
                progress.println(&format!("{}: skipped, a submodule cannot be exported", entry.path));
                continue;
            }
            "120000" => {
                let mut link = Vec::new();
                repo.read_blob_to(&entry.object, &mut link)?;
                let link = String::from_utf8(link)
                    .with_context(|| format!("{} has a link target that is not UTF-8", entry.path))?;
                if place_symlink(&target, &link)? {
                    written += 1;
                }
                continue;
            }
            _ => {}
        }
        match pointers.get(&entry.object) {
            Some(pointer) if only.as_ref().is_none_or(|set| set.is_match(&entry.path)) => {
                let present = match std::fs::symlink_metadata(&target) {
                    Ok(meta) if meta.is_file() && meta.len() == pointer.size && !pointer_at(&target, pointer) => {
                        if options.verify {
                            verified += 1;
                            hash::hash_file(&target)?.0 == pointer.oid
                        } else {
                            true
                        }
                    }
                    _ => false,
                };
                if !present {
                    downloads.push((entry.clone(), pointer.clone()));
                }
            }
            Some(pointer) => {
                if !pointer_at(&target, pointer) {
                    write_pointer(&target, pointer, executable)?;
                    written += 1;
                }
            }
            None => {
                if matches_tree.get(&index).copied() != Some(true) {
                    write_blob(repo, entry, &target, executable)?;
                    written += 1;
                }
            }
        }
    }

    let mut hydrated = 0usize;
    let mut bytes = 0u64;
    let mut failures = Vec::new();
    if !downloads.is_empty() {
        let remote = Remote::open(&settings).await?;
        let results: Vec<(String, Result<u64>)> = stream::iter(downloads)
            .map(|(entry, pointer)| {
                let remote = &remote;
                let progress = &progress;
                let partsize = settings.partsize;
                async move {
                    let bar = progress.bytes(&entry.path, pointer.size);
                    let result = transfer::download(
                        remote,
                        &pointer,
                        &dir.join(&entry.path),
                        entry.mode == "100755",
                        partsize,
                        &bar,
                    )
                    .await;
                    bar.finish_and_clear();
                    (entry.path, result.map(|()| pointer.size))
                }
            })
            .buffer_unordered(settings.concurrency)
            .collect()
            .await;
        for (path, result) in results {
            match result {
                Ok(size) => {
                    hydrated += 1;
                    bytes += size;
                }
                Err(err) => failures.push((path, err)),
            }
        }
    }

    for (path, err) in &failures {
        eprintln!("git-hydrate: {path}: {err:#}");
    }
    if !failures.is_empty() {
        bail!(
            "{} objects could not be hydrated, so {} still records {}",
            failures.len(),
            marker.display(),
            previous.as_deref().unwrap_or("nothing")
        );
    }
    write_marker(&marker, &record)?;
    if !options.quiet {
        let verified = if options.verify { format!(", verified {verified}") } else { String::new() };
        progress.println(&format!(
            "exported {} at {record}: hydrated {hydrated} objects ({}), wrote {written} files, removed {removed}{verified}",
            dir.display(),
            human_bytes(bytes)
        ));
    }
    Ok(())
}

/// The pointers among a tree's blobs, keyed by blob id.
fn pointers_in(repo: &Repo, entries: &[TreeEntry]) -> Result<HashMap<String, Pointer>> {
    let blobs: Vec<String> = entries.iter().filter(|e| e.kind == "blob").map(|e| e.object.clone()).collect();
    let sizes = repo.object_sizes(&blobs)?;
    let small: Vec<String> = blobs
        .into_iter()
        .zip(sizes)
        .filter(|(_, size)| matches!(size, Some((_, size)) if *size > 0 && (*size as usize) < MAX_LEN))
        .map(|(blob, _)| blob)
        .collect();
    let contents = repo.read_objects(&small)?;
    Ok(small
        .into_iter()
        .zip(contents)
        .filter_map(|(blob, content)| Pointer::parse(&content?).map(|pointer| (blob, pointer)))
        .collect())
}

/// Builds the matcher for `--only`. A pattern without glob characters names a
/// path and everything under it.
fn matcher(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.trim_end_matches('/');
        let mut add = |text: &str| -> Result<()> {
            builder.add(Glob::new(text).with_context(|| format!("--only pattern {text} is not a glob"))?);
            Ok(())
        };
        add(pattern)?;
        if !pattern.contains(['*', '?', '[', '{']) {
            add(&format!("{pattern}/**"))?;
        }
    }
    Ok(Some(builder.build().context("building the --only matcher")?))
}

/// Whether the file at a path is exactly this pointer.
fn pointer_at(path: &Path, pointer: &Pointer) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && (meta.len() as usize) < MAX_LEN => {
            std::fs::read(path).ok().and_then(|content| Pointer::parse(&content)).as_ref() == Some(pointer)
        }
        _ => false,
    }
}

/// Git's blob id for each file, so an exported file can be compared with the tree without reading it twice.
fn hash_files_as_git(repo: &Repo, paths: &[PathBuf]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.to_string_lossy().as_bytes());
        input.push(b'\n');
    }
    let output = repo.output_with_stdin(&["hash-object", "--no-filters", "--stdin-paths"], input)?;
    let hashes: Vec<String> = String::from_utf8(output)
        .context("hash-object printed something that is not UTF-8")?
        .lines()
        .map(str::to_string)
        .collect();
    if hashes.len() != paths.len() {
        bail!("hash-object answered {} of {} files", hashes.len(), paths.len());
    }
    Ok(hashes)
}

/// Writes a blob's content to a path, atomically.
fn write_blob(repo: &Repo, entry: &TreeEntry, target: &Path, executable: bool) -> Result<()> {
    let dir = target.parent().context("the path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    if target.is_dir() {
        bail!("{} is a directory where the tree has a file", target.display());
    }
    let mut temp = tempfile::Builder::new()
        .prefix(".hydrate-")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    repo.read_blob_to(&entry.object, temp.as_file_mut())?;
    set_executable(temp.path(), executable)?;
    temp.persist(target).with_context(|| format!("placing {}", target.display()))?;
    Ok(())
}

/// Makes a symlink at a path point at a target, returning whether anything changed.
#[cfg(unix)]
fn place_symlink(path: &Path, target: &str) -> Result<bool> {
    if let Ok(existing) = std::fs::read_link(path)
        && existing.to_string_lossy() == target
    {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("replacing {}", path.display())),
    }
    std::os::unix::fs::symlink(target, path).with_context(|| format!("linking {}", path.display()))?;
    Ok(true)
}

#[cfg(not(unix))]
fn place_symlink(path: &Path, _target: &str) -> Result<bool> {
    eprintln!("git-hydrate: {}: skipped, symbolic links are not exported on this platform", path.display());
    Ok(false)
}

/// Removes the now-empty directories between a removed file and the export root.
fn prune_empty_parents(root: &Path, removed: &Path) {
    let mut current = removed.parent();
    while let Some(dir) = current {
        if dir == root || std::fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

fn write_marker(marker: &Path, record: &str) -> Result<()> {
    let dir = marker.parent().context("the marker has no parent directory")?;
    let mut temp = tempfile::Builder::new().prefix(".hydrate-").tempfile_in(dir).context("creating the marker")?;
    std::io::Write::write_all(&mut temp, format!("{record}\n").as_bytes()).context("writing the marker")?;
    temp.persist(marker).with_context(|| format!("placing {}", marker.display()))?;
    Ok(())
}
