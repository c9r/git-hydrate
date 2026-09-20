//! The repository, driven through git's plumbing as subprocesses.
//!
//! Every question about the index, the trees, the attributes, and the config
//! is asked of git itself, so the tool never reads or writes a git file
//! directly and never disagrees with git about what a path is.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

/// A working tree and the git directory behind it.
#[derive(Clone, Debug)]
pub struct Repo {
    /// The root of the working tree, absolute.
    pub toplevel: PathBuf,
    /// The repository's git directory, absolute. In a linked worktree this is the worktree's own.
    pub git_dir: PathBuf,
}

/// One index entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// The file mode as git prints it, such as `100644`.
    pub mode: String,
    /// The blob's object id.
    pub blob: String,
    /// The merge stage. Anything but zero is a conflict.
    pub stage: u8,
    /// The path relative to the top level.
    pub path: String,
}

/// One entry of a recursive tree listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    /// `blob`, `commit` for a submodule, or `tree` when listing without recursion.
    pub kind: String,
    pub object: String,
    pub path: String,
}

/// Where a config value is looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigScope {
    Local,
    Global,
    System,
}

/// The type git parses a config value as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigType {
    Text,
    Int,
    Bool,
}

impl Repo {
    /// Finds the repository containing the current directory.
    pub fn discover() -> Result<Repo> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel", "--absolute-git-dir"])
            .stderr(Stdio::piped())
            .output()
            .context("running git")?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        let text = String::from_utf8(output.stdout).context("git printed a path that is not UTF-8")?;
        let mut lines = text.lines();
        let toplevel = lines.next().filter(|l| !l.is_empty()).context("not inside a working tree")?;
        let git_dir = lines.next().context("git did not print its directory")?;
        Ok(Repo { toplevel: PathBuf::from(toplevel), git_dir: PathBuf::from(git_dir) })
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command.current_dir(&self.toplevel);
        command
    }

    /// Runs git at the top level and returns its standard output, failing on a non-zero exit.
    pub fn output(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.output_in(&self.toplevel, args)
    }

    /// Runs git in a directory and returns its standard output, failing on a non-zero exit.
    pub fn output_in(&self, cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("running git {}", args.join(" ")))?;
        if !output.status.success() {
            bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(output.stdout)
    }

    /// Runs git at the top level, feeding it standard input, and returns its standard output.
    pub fn output_with_stdin(&self, args: &[&str], input: Vec<u8>) -> Result<Vec<u8>> {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running git {}", args.join(" ")))?;
        let mut stdin = child.stdin.take().context("opening git's standard input")?;
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let output = child.wait_with_output().context("waiting for git")?;
        writer.join().map_err(|_| anyhow!("writing to git failed"))?.context("writing to git")?;
        if !output.status.success() {
            bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(output.stdout)
    }

    /// Reads one config value from one scope, or from a file when `scope` is `None` and `file` is given.
    pub fn config_get(
        &self,
        scope: Option<ConfigScope>,
        file: Option<&Path>,
        kind: ConfigType,
        key: &str,
    ) -> Result<Option<String>> {
        let mut command = self.command();
        command.arg("config");
        match (scope, file) {
            (Some(ConfigScope::Local), _) => command.arg("--local"),
            (Some(ConfigScope::Global), _) => command.arg("--global"),
            (Some(ConfigScope::System), _) => command.arg("--system"),
            (None, Some(path)) => command.arg("--file").arg(path),
            (None, None) => &mut command,
        };
        match kind {
            ConfigType::Text => {}
            ConfigType::Int => {
                command.arg("--type=int");
            }
            ConfigType::Bool => {
                command.arg("--type=bool");
            }
        }
        command.arg("--get").arg(key);
        let output = command.stderr(Stdio::piped()).output().context("running git config")?;
        match output.status.code() {
            Some(0) => Ok(Some(String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_string())),
            Some(1) => Ok(None),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if scope == Some(ConfigScope::System) && stderr.contains("unable to read config file") {
                    return Ok(None);
                }
                bail!("git config --get {key} failed: {}", stderr.trim())
            }
        }
    }

    /// Writes one config value in one scope.
    pub fn config_set(&self, scope: ConfigScope, key: &str, value: &str) -> Result<()> {
        let flag = match scope {
            ConfigScope::Local => "--local",
            ConfigScope::Global => "--global",
            ConfigScope::System => "--system",
        };
        self.output(&["config", flag, key, value]).map(|_| ())
    }

    /// Removes one config key from one scope, succeeding if it was absent.
    pub fn config_unset(&self, scope: ConfigScope, key: &str) -> Result<()> {
        let flag = match scope {
            ConfigScope::Local => "--local",
            ConfigScope::Global => "--global",
            ConfigScope::System => "--system",
        };
        let status = self
            .command()
            .args(["config", flag, "--unset", key])
            .stderr(Stdio::piped())
            .output()
            .context("running git config")?;
        match status.status.code() {
            Some(0) | Some(5) => Ok(()),
            _ => bail!("git config --unset {key} failed: {}", String::from_utf8_lossy(&status.stderr).trim()),
        }
    }

    /// Lists index entries matching the pathspecs, which are resolved relative to `cwd`.
    /// Paths in the result are relative to the top level.
    pub fn ls_files(&self, cwd: &Path, pathspecs: &[String]) -> Result<Vec<IndexEntry>> {
        let mut args: Vec<&str> = vec!["ls-files", "-s", "-z", "--full-name", "--"];
        args.extend(pathspecs.iter().map(String::as_str));
        let output = self.output_in(cwd, &args)?;
        let mut entries = Vec::new();
        for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let record = std::str::from_utf8(record).context("ls-files printed a path that is not UTF-8")?;
            let (meta, path) = record.split_once('\t').context("unexpected ls-files record")?;
            let mut fields = meta.split(' ');
            let mode = fields.next().context("unexpected ls-files record")?;
            let blob = fields.next().context("unexpected ls-files record")?;
            let stage =
                fields.next().context("unexpected ls-files record")?.parse().context("unexpected ls-files stage")?;
            entries.push(IndexEntry { mode: mode.to_string(), blob: blob.to_string(), stage, path: path.to_string() });
        }
        Ok(entries)
    }

    /// The value of the `filter` attribute for each path, `None` where it is unset or unspecified.
    pub fn filter_attribute(&self, paths: &[String]) -> Result<Vec<Option<String>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }
        let output = self.output_with_stdin(&["check-attr", "-z", "--stdin", "filter"], input)?;
        let fields: Vec<&[u8]> = output.split(|b| *b == 0).collect();
        let mut by_path: HashMap<&[u8], Option<String>> = HashMap::new();
        for triple in fields.as_chunks::<3>().0 {
            let value = std::str::from_utf8(triple[2]).context("check-attr printed a value that is not UTF-8")?;
            let value = match value {
                "unspecified" | "unset" => None,
                other => Some(other.to_string()),
            };
            by_path.insert(triple[0], value);
        }
        Ok(paths.iter().map(|p| by_path.get(p.as_bytes()).cloned().flatten()).collect())
    }

    /// The type and size of each object, `None` where the object is missing.
    pub fn object_sizes(&self, objects: &[String]) -> Result<Vec<Option<(String, u64)>>> {
        if objects.is_empty() {
            return Ok(Vec::new());
        }
        let input = objects.iter().flat_map(|o| format!("{o}\n").into_bytes()).collect();
        let output = self.output_with_stdin(&["cat-file", "--batch-check"], input)?;
        let text = String::from_utf8(output).context("cat-file printed a header that is not UTF-8")?;
        let mut results = Vec::with_capacity(objects.len());
        for line in text.lines() {
            let mut fields = line.split(' ');
            let _object = fields.next();
            match fields.next() {
                Some("missing") | None => results.push(None),
                Some(kind) => {
                    let size = fields
                        .next()
                        .context("unexpected cat-file header")?
                        .parse()
                        .context("unexpected cat-file size")?;
                    results.push(Some((kind.to_string(), size)));
                }
            }
        }
        if results.len() != objects.len() {
            bail!("cat-file answered {} of {} objects", results.len(), objects.len());
        }
        Ok(results)
    }

    /// The content of each object, `None` where the object is missing.
    ///
    /// Every requested object is read whole into memory, so callers keep
    /// this to the small objects a pointer could be.
    pub fn read_objects(&self, objects: &[String]) -> Result<Vec<Option<Vec<u8>>>> {
        if objects.is_empty() {
            return Ok(Vec::new());
        }
        let input = objects.iter().flat_map(|o| format!("{o}\n").into_bytes()).collect();
        let output = self.output_with_stdin(&["cat-file", "--batch"], input)?;
        let mut results = Vec::with_capacity(objects.len());
        let mut rest: &[u8] = &output;
        while !rest.is_empty() {
            let newline = rest.iter().position(|b| *b == b'\n').context("unexpected cat-file output")?;
            let header =
                std::str::from_utf8(&rest[..newline]).context("cat-file printed a header that is not UTF-8")?;
            rest = &rest[newline + 1..];
            let mut fields = header.split(' ');
            let _object = fields.next();
            match fields.next() {
                Some("missing") | None => results.push(None),
                Some(_kind) => {
                    let size: usize = fields
                        .next()
                        .context("unexpected cat-file header")?
                        .parse()
                        .context("unexpected cat-file size")?;
                    if rest.len() < size + 1 {
                        bail!("cat-file output ended inside an object");
                    }
                    results.push(Some(rest[..size].to_vec()));
                    rest = &rest[size + 1..];
                }
            }
        }
        if results.len() != objects.len() {
            bail!("cat-file answered {} of {} objects", results.len(), objects.len());
        }
        Ok(results)
    }

    /// Streams one blob's content into a writer.
    pub fn read_blob_to(&self, object: &str, writer: &mut dyn Write) -> Result<()> {
        let mut child = self
            .command()
            .args(["cat-file", "blob", object])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running git cat-file")?;
        let mut stdout = child.stdout.take().context("opening git's standard output")?;
        std::io::copy(&mut stdout, writer).context("reading the blob")?;
        let output = child.wait_with_output().context("waiting for git")?;
        if !output.status.success() {
            bail!("git cat-file blob {object} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(())
    }

    /// The paths, among those given, whose working tree file differs from the index.
    ///
    /// Git answers from its stat cache and runs the clean filter only where
    /// the stat changed, so this is cheap for files it has seen before.
    pub fn modified_paths(&self, paths: &[String]) -> Result<Vec<String>> {
        let mut modified = Vec::new();
        for batch in paths.chunks(500) {
            let mut args: Vec<&str> = vec!["diff-files", "-z", "--name-only", "--"];
            args.extend(batch.iter().map(String::as_str));
            let output = self.output(&args)?;
            for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                modified
                    .push(String::from_utf8(record.to_vec()).context("diff-files printed a path that is not UTF-8")?);
            }
        }
        Ok(modified)
    }

    /// The paths staged as added or modified relative to `HEAD`, or to the empty tree before the first commit.
    pub fn staged_paths(&self) -> Result<Vec<String>> {
        let output = self.output(&["diff", "--cached", "-z", "--name-only", "--diff-filter=AM", "--no-renames"])?;
        output
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8(r.to_vec()).context("diff printed a path that is not UTF-8"))
            .collect()
    }

    /// Makes git re-examine the given paths and record their current stat.
    pub fn refresh_index(&self, paths: &[String]) -> Result<()> {
        for batch in paths.chunks(500) {
            let mut args: Vec<&str> = vec!["update-index", "-q", "--refresh", "--"];
            args.extend(batch.iter().map(String::as_str));
            self.output(&args)?;
        }
        Ok(())
    }

    /// Resolves a revision to an object id, or `None` if it does not name one.
    pub fn resolve(&self, revision: &str) -> Result<Option<String>> {
        let output = self
            .command()
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options", revision])
            .stderr(Stdio::piped())
            .output()
            .context("running git rev-parse")?;
        if output.status.success() {
            Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
        } else {
            Ok(None)
        }
    }

    /// Whether this repository holds `sha` as a commit. A full object id passes
    /// `rev-parse --verify` on its syntax alone, so the check peels it, which
    /// fails when the object is absent.
    pub fn has_commit(&self, sha: &str) -> Result<bool> {
        Ok(self.resolve(&format!("{sha}^{{commit}}"))?.is_some())
    }

    /// Lists a tree recursively, with paths relative to the top level.
    pub fn ls_tree(&self, tree: &str) -> Result<Vec<TreeEntry>> {
        let output = self.output(&["ls-tree", "-r", "-z", "--full-tree", tree])?;
        let mut entries = Vec::new();
        for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let record = std::str::from_utf8(record).context("ls-tree printed a path that is not UTF-8")?;
            let (meta, path) = record.split_once('\t').context("unexpected ls-tree record")?;
            let mut fields = meta.split(' ');
            let mode = fields.next().context("unexpected ls-tree record")?;
            let kind = fields.next().context("unexpected ls-tree record")?;
            let object = fields.next().context("unexpected ls-tree record")?;
            entries.push(TreeEntry {
                mode: mode.to_string(),
                kind: kind.to_string(),
                object: object.to_string(),
                path: path.to_string(),
            });
        }
        Ok(entries)
    }

    /// The paths present in `old` and absent from `new`.
    pub fn deleted_between(&self, old: &str, new: &str) -> Result<Vec<String>> {
        let output =
            self.output(&["diff-tree", "-r", "-z", "--no-renames", "--diff-filter=D", "--name-only", old, new])?;
        output
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8(r.to_vec()).context("diff-tree printed a path that is not UTF-8"))
            .collect()
    }

    /// Every object reachable as `git rev-list --objects` sees it, with the path a blob or tree was first seen at.
    pub fn list_objects(&self, args: &[&str]) -> Result<Vec<(String, Option<String>)>> {
        let mut full: Vec<&str> = vec!["rev-list", "--objects"];
        full.extend(args);
        let output = self.output(&full)?;
        let text = String::from_utf8(output).context("rev-list printed a path that is not UTF-8")?;
        Ok(text
            .lines()
            .map(|line| match line.split_once(' ') {
                Some((object, path)) => (object.to_string(), Some(path.to_string())),
                None => (line.to_string(), None),
            })
            .collect())
    }

    /// Where git looks for this repository's hooks.
    pub fn hooks_dir(&self) -> Result<PathBuf> {
        let output = self.output(&["rev-parse", "--path-format=absolute", "--git-path", "hooks"])?;
        Ok(PathBuf::from(String::from_utf8_lossy(&output).trim()))
    }

    /// Whether `core.hooksPath` redirects hooks away from the repository.
    pub fn hooks_redirected(&self) -> Result<bool> {
        Ok(self.config_get(None, None, ConfigType::Text, "core.hooksPath")?.is_some())
    }
}

/// Writes one global config value, from anywhere.
pub fn config_set_global(key: &str, value: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["config", "--global", key, value])
        .stderr(Stdio::piped())
        .output()
        .context("running git config")?;
    if !output.status.success() {
        bail!("git config --global {key} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// Removes one global config key, from anywhere, succeeding if it was absent.
pub fn config_unset_global(key: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["config", "--global", "--unset", key])
        .stderr(Stdio::piped())
        .output()
        .context("running git config")?;
    match output.status.code() {
        Some(0) | Some(5) => Ok(()),
        _ => bail!("git config --global --unset {key} failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
    }
}
