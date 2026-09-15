//! The command line, and the dispatch from it to the verbs.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::commands::{adopt, drop, env, export, install, precommit, prepush, pull, push, status, track, verify};
use crate::filter;
use crate::git::Repo;

/// Keep large files in an object store and pointers in git, with one copy on disk.
#[derive(Parser, Debug)]
#[command(name = "git-hydrate", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Paths to hydrate, as `git hydrate pull` would take them.
    #[arg(value_name = "PATHSPEC")]
    paths: Vec<String>,

    /// Hydrate every tracked file in the repository.
    #[arg(long)]
    all: bool,

    /// Print nothing but errors.
    #[arg(long, short, global = true)]
    quiet: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Download the objects behind tracked files into the working tree.
    Pull {
        #[arg(value_name = "PATHSPEC")]
        paths: Vec<String>,
        /// Hydrate every tracked file in the repository.
        #[arg(long)]
        all: bool,
    },
    /// Replace hydrated files with their pointers, refusing anything not committed and pushed.
    Drop {
        #[arg(value_name = "PATHSPEC")]
        paths: Vec<String>,
        /// Dehydrate every tracked file in the repository.
        #[arg(long)]
        all: bool,
    },
    /// Upload the objects the index points to that the remote lacks.
    Push {
        #[arg(value_name = "PATHSPEC")]
        paths: Vec<String>,
    },
    /// List tracked files with their state in the working tree and the remote.
    Status {
        #[arg(value_name = "PATHSPEC")]
        paths: Vec<String>,
        /// Do not ask the remote which objects it holds.
        #[arg(long)]
        no_remote: bool,
        /// Print tab-separated fields for scripts.
        #[arg(long)]
        porcelain: bool,
    },
    /// Check that every pointer in a tree has its object in the remote.
    Verify {
        /// The tree to check, `HEAD` by default.
        #[arg(value_name = "TREE-ISH")]
        tree: Option<String>,
    },
    /// Write a tree into a directory that is not a repository, hydrated.
    Export {
        #[arg(value_name = "TREE-ISH")]
        tree: String,
        #[arg(value_name = "DIR")]
        dir: PathBuf,
        /// Hydrate only the pointers whose paths match these globs. The rest stay pointers.
        #[arg(long, value_name = "GLOB")]
        only: Vec<String>,
        /// Hash every object already in the directory instead of trusting its size.
        #[arg(long)]
        verify: bool,
    },
    /// Write a pointer for an object that is already in the remote.
    Adopt {
        #[arg(value_name = "PATH")]
        path: String,
        #[arg(value_name = "OID")]
        oid: String,
        #[arg(value_name = "SIZE")]
        size: u64,
        /// Replace a file that is not a pointer.
        #[arg(long)]
        force: bool,
    },
    /// Add patterns to .gitattributes so their files are tracked.
    Track {
        #[arg(value_name = "PATTERN")]
        patterns: Vec<String>,
    },
    /// Print the resolved configuration.
    Env,
    /// Register the filter with git, globally unless --local.
    Install {
        #[arg(long)]
        local: bool,
    },
    /// Unregister the filter, globally unless --local.
    Uninstall {
        #[arg(long)]
        local: bool,
    },
    /// The filter process git runs. Not for people.
    #[command(name = "filter-process", hide = true)]
    FilterProcess,
    /// The pre-commit hook git runs. Not for people.
    #[command(name = "pre-commit", hide = true)]
    PreCommit,
    /// The pre-push hook git runs. Not for people.
    #[command(name = "pre-push", hide = true)]
    PrePush { remote: String, url: Option<String> },
}

/// Runs `git-hydrate` with the given arguments and returns the process's exit code.
pub fn main<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::parse_from(args);
    let quiet = cli.quiet;
    let command = match cli.command {
        Some(command) => command,
        None if cli.paths.is_empty() && !cli.all => {
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            return ExitCode::from(2);
        }
        None => Command::Pull { paths: cli.paths, all: cli.all },
    };
    report(run(command, quiet))
}

/// Runs `git-dehydrate`, which is `git hydrate drop`.
pub fn dehydrate_main<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    #[derive(Parser, Debug)]
    #[command(
        name = "git-dehydrate",
        version,
        about = "Replace hydrated files with their pointers, refusing anything not committed and pushed."
    )]
    struct Dehydrate {
        #[arg(value_name = "PATHSPEC")]
        paths: Vec<String>,
        /// Dehydrate every tracked file in the repository.
        #[arg(long)]
        all: bool,
        /// Print nothing but errors.
        #[arg(long, short)]
        quiet: bool,
    }
    let cli = Dehydrate::parse_from(args);
    report(run(Command::Drop { paths: cli.paths, all: cli.all }, cli.quiet))
}

fn report(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("git-hydrate: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, quiet: bool) -> Result<()> {
    match command {
        Command::Install { local } => return install::install(local),
        Command::Uninstall { local } => return install::uninstall(local),
        Command::FilterProcess => {
            let stdin = std::io::stdin().lock();
            let stdout = std::io::stdout().lock();
            return filter::run(stdin, stdout, || {
                if let Ok(repo) = Repo::discover() {
                    let _ = install::ensure_hooks(&repo);
                }
            });
        }
        _ => {}
    }
    let repo = Repo::discover()?;
    let cwd = std::env::current_dir()?;
    match command {
        Command::Track { patterns } => track::run(&repo, patterns),
        Command::Env => env::run(&repo),
        Command::Pull { paths, all } => {
            block_on(pull::run(&repo, &cwd, pull::Options { pathspecs: paths, all, quiet }))
        }
        Command::Drop { paths, all } => {
            block_on(drop::run(&repo, &cwd, drop::Options { pathspecs: paths, all, quiet }))
        }
        Command::Push { paths } => block_on(push::run(&repo, &cwd, push::Options { pathspecs: paths, quiet })),
        Command::Status { paths, no_remote, porcelain } => block_on(status::run(
            &repo,
            &cwd,
            status::Options { pathspecs: paths, check_remote: !no_remote, porcelain },
        )),
        Command::Verify { tree } => block_on(verify::run(&repo, tree)),
        Command::Export { tree, dir, only, verify } => {
            block_on(export::run(&repo, export::Options { treeish: tree, dir, only, verify, quiet }))
        }
        Command::Adopt { path, oid, size, force } => {
            block_on(adopt::run(&repo, &cwd, adopt::Options { path, oid, size, force }))
        }
        Command::PreCommit => block_on(precommit::run(&repo, quiet)),
        Command::PrePush { remote, url: _ } => block_on(prepush::run(&repo, &remote, quiet)),
        Command::Install { .. } | Command::Uninstall { .. } | Command::FilterProcess => {
            unreachable!("handled before the repository is needed")
        }
    }
}

fn block_on<F: Future<Output = Result<()>>>(future: F) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(future)
}
