//! `git hydrate env`, which prints the resolved configuration.

use anyhow::Result;

use crate::commands::install::{HOOK_MARKER, HOOKS, PROCESS_KEY, REQUIRED_KEY};
use crate::config::{self, RemoteSpec};
use crate::git::{ConfigType, Repo};

pub fn run(repo: &Repo) -> Result<()> {
    println!("toplevel = {}", repo.toplevel.display());
    println!("git-dir = {}", repo.git_dir.display());
    for key in [PROCESS_KEY, REQUIRED_KEY] {
        match repo.config_get(None, None, ConfigType::Text, key)? {
            Some(value) => println!("{key} = {value}"),
            None => println!("{key} is not set; run git hydrate install"),
        }
    }
    let redirected = repo.hooks_redirected()?;
    let hooks_dir = repo.hooks_dir()?;
    for name in HOOKS {
        let hook = hooks_dir.join(name);
        let state = if redirected {
            "redirected by core.hooksPath"
        } else if std::fs::read_to_string(&hook).map(|text| text.contains(HOOK_MARKER)).unwrap_or(false) {
            "present"
        } else if hook.is_file() {
            "occupied by another hook"
        } else {
            "absent"
        };
        println!("{name} hook = {} ({state})", hook.display());
    }

    match config::resolve(repo) {
        Ok((settings, origins)) => {
            for origin in &origins {
                println!("hydrate.{} = {} ({})", origin.key, origin.value, origin.source);
            }
            let remote = match &settings.remote {
                RemoteSpec::S3 { bucket, prefix } => format!("s3://{bucket}/{prefix}"),
                RemoteSpec::Local { root } => format!("file://{}", root.display()),
            };
            println!("remote = {remote}");
            println!("partsize = {} bytes", settings.partsize);
            println!("concurrency = {}", settings.concurrency);
        }
        Err(err) => println!("configuration is incomplete: {err:#}"),
    }
    Ok(())
}
