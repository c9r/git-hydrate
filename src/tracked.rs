//! The tracked files of a repository and the state of each in the working tree.
//!
//! A file is tracked when its path carries the `hydrate` filter attribute and
//! the index holds a pointer for it. Both conditions come from git, so what
//! the tool acts on is exactly what git filters.

use std::path::Path;

use anyhow::Result;

use crate::git::Repo;
use crate::pointer::{MAX_LEN, Pointer};

/// The attribute value that marks a path as tracked.
pub const FILTER_NAME: &str = "hydrate";

/// One tracked file as the index records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracked {
    /// The path relative to the top level.
    pub path: String,
    /// Whether the index mode carries the executable bit.
    pub executable: bool,
    pub pointer: Pointer,
}

/// What the working tree holds at a tracked path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    /// The pointer the index holds.
    Dehydrated,
    /// Content, which may or may not still hash to the pointer.
    Hydrated,
    /// Nothing.
    Missing,
    /// A pointer for a different object, which someone edited by hand.
    PointerMismatch,
}

/// The tracked files whose paths match the pathspecs, resolved relative to `cwd`.
pub fn tracked_pointers(repo: &Repo, cwd: &Path, pathspecs: &[String]) -> Result<Vec<Tracked>> {
    let entries = repo.ls_files(cwd, pathspecs)?;
    let entries: Vec<_> =
        entries.into_iter().filter(|e| e.stage == 0 && (e.mode == "100644" || e.mode == "100755")).collect();
    let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    let attributes = repo.filter_attribute(&paths)?;
    let candidates: Vec<_> = entries
        .into_iter()
        .zip(attributes)
        .filter(|(_, attribute)| attribute.as_deref() == Some(FILTER_NAME))
        .map(|(entry, _)| entry)
        .collect();
    let blobs: Vec<String> = candidates.iter().map(|e| e.blob.clone()).collect();
    let sizes = repo.object_sizes(&blobs)?;
    let small: Vec<_> = candidates
        .into_iter()
        .zip(sizes)
        .filter(
            |(_, size)| matches!(size, Some((kind, size)) if kind == "blob" && *size > 0 && (*size as usize) < MAX_LEN),
        )
        .map(|(entry, _)| entry)
        .collect();
    let contents = repo.read_objects(&small.iter().map(|e| e.blob.clone()).collect::<Vec<_>>())?;
    Ok(small
        .into_iter()
        .zip(contents)
        .filter_map(|(entry, content)| {
            let pointer = Pointer::parse(&content?)?;
            Some(Tracked { path: entry.path, executable: entry.mode == "100755", pointer })
        })
        .collect())
}

/// What the working tree holds for a tracked file.
pub fn work_state(toplevel: &Path, tracked: &Tracked) -> std::io::Result<WorkState> {
    let path = toplevel.join(&tracked.path);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WorkState::Missing),
        Err(e) => return Err(e),
    };
    if !meta.is_file() || meta.len() as usize >= MAX_LEN {
        return Ok(WorkState::Hydrated);
    }
    let content = std::fs::read(&path)?;
    Ok(match Pointer::parse(&content) {
        Some(pointer) if pointer == tracked.pointer => WorkState::Dehydrated,
        Some(_) => WorkState::PointerMismatch,
        None => WorkState::Hydrated,
    })
}

/// Writes a pointer file at a path, atomically, with the executable bit git expects.
pub fn write_pointer(path: &Path, pointer: &Pointer, executable: bool) -> Result<()> {
    use anyhow::Context;
    let dir = path.parent().context("the path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".hydrate-")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    std::io::Write::write_all(&mut temp, &pointer.to_bytes()).context("writing the pointer")?;
    set_executable(temp.path(), executable)?;
    temp.persist(path).with_context(|| format!("placing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
pub fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use anyhow::Context;
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode of {}", path.display()))
}

#[cfg(not(unix))]
pub fn set_executable(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

/// A count of bytes as a person reads it.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}
