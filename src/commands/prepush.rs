//! `git hydrate pre-push`, the hook that verifies objects before commits leave.
//!
//! Git hands the hook the refs being pushed on standard input. The hook finds
//! every pointer in the commits the remote does not have and uploads each
//! object the remote lacks from the working tree, which only a commit made
//! without the pre-commit hook leaves behind. It aborts the push naming any
//! object it can find nowhere.

use std::collections::{HashMap, HashSet};
use std::io::Read;

use anyhow::{Context, Result, bail};

use crate::commands::head_all;
use crate::commands::push::{self, Wanted};
use crate::config;
use crate::git::Repo;
use crate::pointer::{MAX_LEN, Pointer};
use crate::progress::Progress;
use crate::remote::Remote;
use crate::tracked::{human_bytes, tracked_pointers};

pub async fn run(repo: &Repo, remote_name: &str, quiet: bool) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("reading the ref updates git passes a pre-push hook")?;

    let mut objects: HashMap<String, Option<String>> = HashMap::new();
    for line in input.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() != 4 {
            bail!("unexpected pre-push input line: {line}");
        }
        let (local_sha, remote_sha) = (fields[1], fields[3]);
        if is_zero(local_sha) {
            continue;
        }
        let not_remote = format!("--remotes={remote_name}");
        let exclude = format!("^{remote_sha}");
        let args: Vec<&str> =
            if is_zero(remote_sha) { vec![local_sha, "--not", &not_remote] } else { vec![local_sha, &exclude] };
        for (object, path) in repo.list_objects(&args)? {
            objects.entry(object).or_insert(path);
        }
    }
    if objects.is_empty() {
        return Ok(());
    }

    let ids: Vec<String> = objects.keys().cloned().collect();
    let sizes = repo.object_sizes(&ids)?;
    let small: Vec<String> = ids
        .into_iter()
        .zip(sizes)
        .filter(
            |(_, size)| matches!(size, Some((kind, size)) if kind == "blob" && *size > 0 && (*size as usize) < MAX_LEN),
        )
        .map(|(id, _)| id)
        .collect();
    let contents = repo.read_objects(&small)?;
    let mut wanted: HashMap<String, Wanted> = HashMap::new();
    for (id, content) in small.into_iter().zip(contents) {
        let Some(pointer) = content.as_deref().and_then(Pointer::parse) else { continue };
        let entry = wanted.entry(pointer.oid.clone()).or_insert_with(|| Wanted { pointer, paths: Vec::new() });
        if let Some(Some(path)) = objects.get(&id) {
            entry.paths.push(path.clone());
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }

    for file in tracked_pointers(repo, &repo.toplevel, &[])? {
        if let Some(entry) = wanted.get_mut(&file.pointer.oid)
            && !entry.paths.contains(&file.path)
        {
            entry.paths.push(file.path);
        }
    }
    for entry in wanted.values_mut() {
        let mut seen = HashSet::new();
        entry.paths.retain(|p| seen.insert(p.clone()));
    }

    let (settings, _) = config::resolve(repo)?;
    let remote = Remote::open(&settings).await?;
    let present = head_all(&remote, &wanted.keys().cloned().collect::<Vec<_>>()).await?;
    let mut missing: Vec<Wanted> = wanted
        .into_values()
        .filter(|w| !matches!(present.get(&w.pointer.oid), Some(Some(size)) if *size == w.pointer.size))
        .collect();
    missing.sort_by(|a, b| a.paths.cmp(&b.paths));
    if missing.is_empty() {
        return Ok(());
    }

    let progress = Progress::new(quiet);
    let outcome = push::upload(repo, &remote, &settings, missing, &progress).await?;
    if !quiet && outcome.uploaded > 0 {
        progress.println(&format!(
            "git-hydrate: uploaded {} objects, {}",
            outcome.uploaded,
            human_bytes(outcome.bytes)
        ));
    }
    for failure in &outcome.failures {
        let paths = if failure.paths.is_empty() { failure.pointer.oid.clone() } else { failure.paths.join(", ") };
        eprintln!("git-hydrate: {paths}: {}", failure.reason);
    }
    if !outcome.failures.is_empty() {
        bail!(
            "{} objects behind the pushed commits are not in the remote, so the push was refused",
            outcome.failures.len()
        );
    }
    Ok(())
}

fn is_zero(sha: &str) -> bool {
    !sha.is_empty() && sha.bytes().all(|b| b == b'0')
}
