//! `git hydrate track`, which adds patterns to `.gitattributes`.

use std::io::Write;

use anyhow::{Context, Result, bail};

use crate::git::Repo;
use crate::tracked::FILTER_NAME;

pub fn run(repo: &Repo, patterns: Vec<String>) -> Result<()> {
    if patterns.is_empty() {
        bail!("name the patterns to track");
    }
    let path = repo.toplevel.join(".gitattributes");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut additions = String::new();
    for pattern in &patterns {
        if pattern.is_empty() || pattern.chars().any(char::is_whitespace) {
            bail!("{pattern:?} is not a pattern .gitattributes can carry; patterns cannot contain whitespace");
        }
        let tracked = existing.lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next() == Some(pattern) && fields.any(|f| f == format!("filter={FILTER_NAME}"))
        });
        if tracked {
            println!("{pattern} is already tracked");
            continue;
        }
        additions.push_str(&format!("{pattern} filter={FILTER_NAME} -text\n"));
        println!("tracking {pattern}");
    }
    if additions.is_empty() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n").context("writing .gitattributes")?;
    }
    file.write_all(additions.as_bytes()).context("writing .gitattributes")?;
    Ok(())
}
