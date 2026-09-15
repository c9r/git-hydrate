//! `git hydrate install` and `uninstall`, and the hooks they and the filter place.
//!
//! Two hooks make commits durable. The pre-commit hook uploads the objects
//! behind staged pointers, so a commit exists in the remote the moment it
//! exists at all. The pre-push hook uploads whatever a commit made without
//! hooks left behind and refuses the push if an object exists nowhere.

use std::path::Path;

use anyhow::{Context, Result};

use crate::git::{self, ConfigScope, Repo};

/// The config keys that register the filter with git.
pub const PROCESS_KEY: &str = "filter.hydrate.process";
pub const REQUIRED_KEY: &str = "filter.hydrate.required";

/// What git runs for every tracked path.
pub const PROCESS_COMMAND: &str = "git-hydrate filter-process";

/// The hooks the tool places, each running the verb of the same name.
pub const HOOKS: [&str; 2] = ["pre-commit", "pre-push"];

/// The line that identifies a hook as this tool's.
pub const HOOK_MARKER: &str = "# git-hydrate hook";

fn hook_script(name: &str) -> String {
    format!(
        "#!/bin/sh
{HOOK_MARKER}
if ! command -v git-hydrate >/dev/null 2>&1; then
  echo >&2 'git-hydrate: the git-hydrate binary is not on PATH, so the objects behind pointers cannot be uploaded or verified'
  exit 2
fi
exec git-hydrate {name} \"$@\"
"
    )
}

/// What happened when a hook was looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookOutcome {
    Installed,
    Present,
    /// Another hook occupies the file, and it was left alone.
    Foreign,
    /// `core.hooksPath` sends hooks elsewhere, and nothing was written.
    Redirected,
}

pub fn install(local: bool) -> Result<()> {
    if local {
        let repo = Repo::discover()?;
        repo.config_set(ConfigScope::Local, PROCESS_KEY, PROCESS_COMMAND)?;
        repo.config_set(ConfigScope::Local, REQUIRED_KEY, "true")?;
        report_hooks(&ensure_hooks(&repo)?);
        println!("registered the hydrate filter in this repository's git config");
    } else {
        git::config_set_global(PROCESS_KEY, PROCESS_COMMAND)?;
        git::config_set_global(REQUIRED_KEY, "true")?;
        if let Ok(repo) = Repo::discover() {
            report_hooks(&ensure_hooks(&repo)?);
        }
        println!("registered the hydrate filter in the global git config");
    }
    Ok(())
}

pub fn uninstall(local: bool) -> Result<()> {
    if local {
        let repo = Repo::discover()?;
        repo.config_unset(ConfigScope::Local, PROCESS_KEY)?;
        repo.config_unset(ConfigScope::Local, REQUIRED_KEY)?;
        remove_hooks(&repo)?;
        println!("removed the hydrate filter from this repository's git config");
    } else {
        git::config_unset_global(PROCESS_KEY)?;
        git::config_unset_global(REQUIRED_KEY)?;
        if let Ok(repo) = Repo::discover() {
            remove_hooks(&repo)?;
        }
        println!("removed the hydrate filter from the global git config");
    }
    Ok(())
}

/// Places each hook the repository lacks, reporting what happened to each.
pub fn ensure_hooks(repo: &Repo) -> Result<Vec<(&'static str, HookOutcome)>> {
    if repo.hooks_redirected()? {
        return Ok(HOOKS.iter().map(|name| (*name, HookOutcome::Redirected)).collect());
    }
    let dir = repo.hooks_dir()?;
    let mut outcomes = Vec::new();
    for name in HOOKS {
        let path = dir.join(name);
        let outcome = match std::fs::read_to_string(&path) {
            Ok(text) if text.contains(HOOK_MARKER) => HookOutcome::Present,
            Ok(_) => HookOutcome::Foreign,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
                std::fs::write(&path, hook_script(name)).with_context(|| format!("writing {}", path.display()))?;
                make_executable(&path)?;
                HookOutcome::Installed
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        outcomes.push((name, outcome));
    }
    Ok(outcomes)
}

/// Removes each hook that is this tool's.
pub fn remove_hooks(repo: &Repo) -> Result<()> {
    if repo.hooks_redirected()? {
        return Ok(());
    }
    let dir = repo.hooks_dir()?;
    for name in HOOKS {
        let path = dir.join(name);
        if let Ok(text) = std::fs::read_to_string(&path)
            && text.contains(HOOK_MARKER)
        {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed the {name} hook");
        }
    }
    Ok(())
}

fn report_hooks(outcomes: &[(&str, HookOutcome)]) {
    for (name, outcome) in outcomes {
        match outcome {
            HookOutcome::Installed => println!("installed the {name} hook"),
            HookOutcome::Present => {}
            HookOutcome::Foreign => {
                eprintln!(
                    "git-hydrate: a {name} hook already exists and is not git-hydrate's; have it run `git-hydrate {name} \"$@\"`"
                )
            }
            HookOutcome::Redirected => {
                eprintln!(
                    "git-hydrate: core.hooksPath is set, so no {name} hook was installed; have your {name} hook run `git-hydrate {name} \"$@\"`"
                )
            }
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("setting the mode of {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}
