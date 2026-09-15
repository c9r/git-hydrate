//! `git hydrate status`, which lists tracked files and their state.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::commands::head_all;
use crate::config;
use crate::git::Repo;
use crate::remote::Remote;
use crate::tracked::{self, WorkState, human_bytes, tracked_pointers};

pub struct Options {
    pub pathspecs: Vec<String>,
    pub check_remote: bool,
    pub porcelain: bool,
}

pub async fn run(repo: &Repo, cwd: &Path, options: Options) -> Result<()> {
    let files = if options.pathspecs.is_empty() {
        tracked_pointers(repo, &repo.toplevel, &[])?
    } else {
        tracked_pointers(repo, cwd, &options.pathspecs)?
    };
    let mut states = Vec::with_capacity(files.len());
    for file in &files {
        states.push(tracked::work_state(&repo.toplevel, file)?);
    }
    let hydrated: Vec<String> =
        files.iter().zip(&states).filter(|(_, s)| **s == WorkState::Hydrated).map(|(f, _)| f.path.clone()).collect();
    let modified: HashSet<String> = repo.modified_paths(&hydrated)?.into_iter().collect();

    let present: Option<HashMap<String, Option<u64>>> = if options.check_remote {
        match config::resolve(repo) {
            Ok((settings, _)) => {
                let remote = Remote::open(&settings).await?;
                let oids: Vec<String> =
                    files.iter().map(|f| f.pointer.oid.clone()).collect::<HashSet<_>>().into_iter().collect();
                Some(head_all(&remote, &oids).await?)
            }
            Err(err) => {
                eprintln!("git-hydrate: not asking the remote: {err:#}");
                None
            }
        }
    } else {
        None
    };

    for (file, state) in files.iter().zip(&states) {
        let state = match state {
            WorkState::Hydrated if modified.contains(&file.path) => "modified",
            WorkState::Hydrated => "hydrated",
            WorkState::Dehydrated => "dehydrated",
            WorkState::Missing => "missing",
            WorkState::PointerMismatch => "mismatch",
        };
        let remote = match &present {
            None => "-",
            Some(present) => match present.get(&file.pointer.oid) {
                Some(Some(size)) if *size == file.pointer.size => "remote",
                Some(Some(_)) => "corrupt",
                _ => "absent",
            },
        };
        if options.porcelain {
            println!("{state}\t{remote}\t{}\t{}\t{}", file.pointer.oid, file.pointer.size, file.path);
        } else {
            println!("{state:<10} {remote:<8} {:>10}  {}", human_bytes(file.pointer.size), file.path);
        }
    }
    Ok(())
}
