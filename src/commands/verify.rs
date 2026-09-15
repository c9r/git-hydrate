//! `git hydrate verify`, which checks that a tree's objects are all in the remote.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use crate::commands::head_all;
use crate::config;
use crate::git::Repo;
use crate::pointer::{MAX_LEN, Pointer};
use crate::remote::Remote;

/// The pointers in a tree, with the paths that carry each one.
///
/// Any blob shaped like a pointer counts, whatever the attributes said when
/// it was committed, because attributes at a past commit are not something
/// git will answer cheaply and a pointer is a pointer.
pub fn tree_pointers(repo: &Repo, tree: &str) -> Result<HashMap<Pointer, Vec<String>>> {
    let entries = repo.ls_tree(tree)?;
    let blobs: Vec<_> = entries.into_iter().filter(|e| e.kind == "blob").collect();
    let sizes = repo.object_sizes(&blobs.iter().map(|e| e.object.clone()).collect::<Vec<_>>())?;
    let small: Vec<_> = blobs
        .into_iter()
        .zip(sizes)
        .filter(|(_, size)| matches!(size, Some((_, size)) if *size > 0 && (*size as usize) < MAX_LEN))
        .map(|(entry, _)| entry)
        .collect();
    let contents = repo.read_objects(&small.iter().map(|e| e.object.clone()).collect::<Vec<_>>())?;
    let mut pointers: HashMap<Pointer, Vec<String>> = HashMap::new();
    for (entry, content) in small.into_iter().zip(contents) {
        if let Some(pointer) = content.as_deref().and_then(Pointer::parse) {
            pointers.entry(pointer).or_default().push(entry.path);
        }
    }
    Ok(pointers)
}

pub async fn run(repo: &Repo, treeish: Option<String>) -> Result<()> {
    let treeish = treeish.unwrap_or_else(|| "HEAD".to_string());
    let tree =
        repo.resolve(&format!("{treeish}^{{tree}}"))?.with_context(|| format!("{treeish} does not name a tree"))?;
    let pointers = tree_pointers(repo, &tree)?;
    let (settings, _) = config::resolve(repo)?;
    let remote = Remote::open(&settings).await?;
    let present = head_all(&remote, &pointers.keys().map(|p| p.oid.clone()).collect::<Vec<_>>()).await?;

    let mut missing: Vec<(&Pointer, &Vec<String>, &str)> = Vec::new();
    for (pointer, paths) in &pointers {
        match present.get(&pointer.oid) {
            Some(Some(size)) if *size == pointer.size => {}
            Some(Some(_)) => missing.push((pointer, paths, "the remote holds it at a different size")),
            _ => missing.push((pointer, paths, "not in the remote")),
        }
    }
    missing.sort_by(|a, b| a.1.cmp(b.1));
    println!(
        "{} objects behind {} pointers in {treeish}, {} missing",
        pointers.len(),
        pointers.values().map(Vec::len).sum::<usize>(),
        missing.len()
    );
    for (pointer, paths, reason) in &missing {
        println!("{}  {}  {reason}", pointer.oid, paths.join(", "));
    }
    if !missing.is_empty() {
        bail!("{} objects are missing from {}", missing.len(), remote.describe());
    }
    Ok(())
}
