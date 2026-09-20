//! End-to-end tests against a real git, a bare origin, and a `file://` remote.
//!
//! Each test builds a sandbox with its own global git config, so nothing
//! touches the machine's configuration, and puts the built binaries on PATH
//! so the filter and the hooks find them the way a user's would.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use git_hydrate::hash::Hasher;
use git_hydrate::pointer::Pointer;

const HYDRATE: &str = env!("CARGO_BIN_EXE_git-hydrate");
const DEHYDRATE: &str = env!("CARGO_BIN_EXE_git-dehydrate");

struct Sandbox {
    root: tempfile::TempDir,
    remote: PathBuf,
    origin: PathBuf,
    work: PathBuf,
    env: Vec<(String, String)>,
}

/// An S3 endpoint the tests may use, from the environment CI or a developer sets.
struct S3Target {
    remote: String,
    endpoint: Option<String>,
    region: Option<String>,
}

impl S3Target {
    fn from_env() -> Option<S3Target> {
        Some(S3Target {
            remote: std::env::var("GIT_HYDRATE_TEST_S3_REMOTE").ok()?,
            endpoint: std::env::var("GIT_HYDRATE_TEST_S3_ENDPOINT").ok(),
            region: std::env::var("GIT_HYDRATE_TEST_S3_REGION").ok(),
        })
    }
}

impl Sandbox {
    /// A sandbox whose remote is a directory.
    fn new() -> Sandbox {
        Sandbox::build(None)
    }

    /// A sandbox whose remote is the S3 endpoint the environment names, or `None` if it names none.
    fn with_s3() -> Option<Sandbox> {
        S3Target::from_env().map(|target| Sandbox::build(Some(target)))
    }

    fn build(s3: Option<S3Target>) -> Sandbox {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let bin_dir = Path::new(HYDRATE).parent().unwrap().to_string_lossy().to_string();
        let path = format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default());
        let mut env = vec![
            ("HOME".to_string(), home.to_string_lossy().to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), home.join(".gitconfig").to_string_lossy().to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
            ("PATH".to_string(), path),
            ("GIT_AUTHOR_NAME".to_string(), "Test".to_string()),
            ("GIT_AUTHOR_EMAIL".to_string(), "test@example.com".to_string()),
            ("GIT_COMMITTER_NAME".to_string(), "Test".to_string()),
            ("GIT_COMMITTER_EMAIL".to_string(), "test@example.com".to_string()),
        ];
        for key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_PROFILE",
            "AWS_CONFIG_FILE",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_EC2_METADATA_DISABLED",
        ] {
            if let Ok(value) = std::env::var(key) {
                env.push((key.to_string(), value));
            }
        }
        let remote = root.path().join("remote");
        let origin = root.path().join("origin.git");
        let work = root.path().join("work");
        let sandbox = Sandbox { root, remote, origin, work, env };
        sandbox.git(sandbox.root.path(), &["init", "--bare", "-b", "main", "origin.git"]);
        sandbox.git(sandbox.root.path(), &["clone", "-q", "origin.git", "work"]);
        sandbox.git(&sandbox.work, &["config", "--global", "init.defaultBranch", "main"]);
        sandbox.hydrate(&sandbox.work, &["install"]);
        let config = |key: &str, value: &str| {
            sandbox.git(&sandbox.work, &["config", "-f", ".hydrateconfig", key, value]);
        };
        match &s3 {
            None => config("hydrate.remote", &format!("file://{}", sandbox.remote.display())),
            Some(target) => {
                config("hydrate.remote", &target.remote);
                if let Some(endpoint) = &target.endpoint {
                    config("hydrate.endpoint", endpoint);
                }
                if let Some(region) = &target.region {
                    config("hydrate.region", region);
                }
                config("hydrate.pathstyle", "true");
                config("hydrate.partsize", "5m");
            }
        }
        sandbox.hydrate(&sandbox.work, &["track", "*.bin"]);
        sandbox.git(&sandbox.work, &["add", ".hydrateconfig", ".gitattributes"]);
        sandbox.git(&sandbox.work, &["commit", "-q", "-m", "track"]);
        sandbox
    }

    fn command(&self, program: &str, cwd: &Path, args: &[&str]) -> Output {
        Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (OsStr::new(k), OsStr::new(v))))
            .output()
            .unwrap_or_else(|e| panic!("running {program}: {e}"))
    }

    fn run(&self, program: &str, cwd: &Path, args: &[&str]) -> String {
        let output = self.command(program, cwd, args);
        assert!(
            output.status.success(),
            "{program} {} failed\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fails(&self, program: &str, cwd: &Path, args: &[&str]) -> String {
        let output = self.command(program, cwd, args);
        assert!(!output.status.success(), "{program} {} unexpectedly succeeded", args.join(" "));
        format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        self.run("git", cwd, args)
    }

    fn hydrate(&self, cwd: &Path, args: &[&str]) -> String {
        self.run(HYDRATE, cwd, args)
    }

    fn hydrate_fails(&self, cwd: &Path, args: &[&str]) -> String {
        self.fails(HYDRATE, cwd, args)
    }

    fn clone(&self, name: &str) -> PathBuf {
        self.git(self.root.path(), &["clone", "-q", "origin.git", name]);
        self.root.path().join(name)
    }

    fn write(&self, cwd: &Path, path: &str, content: &[u8]) {
        let full = cwd.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn read(&self, cwd: &Path, path: &str) -> Vec<u8> {
        fs::read(cwd.join(path)).unwrap()
    }

    fn index_pointer(&self, cwd: &Path, path: &str) -> Option<Pointer> {
        let blob = self.command("git", cwd, &["show", &format!(":{path}")]);
        assert!(blob.status.success());
        Pointer::parse(&blob.stdout)
    }

    fn status(&self, cwd: &Path) -> String {
        self.git(cwd, &["status", "--porcelain"])
    }

    fn head(&self, cwd: &Path) -> String {
        self.git(cwd, &["rev-parse", "HEAD"]).trim().to_string()
    }

    fn remote_has(&self, pointer: &Pointer) -> bool {
        self.remote.join(&pointer.oid).is_file()
    }
}

fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u64).wrapping_mul(2654435761).wrapping_add(seed as u64) as u8).collect()
}

fn pointer_for(bytes: &[u8]) -> Pointer {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    Pointer { oid: hasher.finish(), size: bytes.len() as u64 }
}

#[test]
fn add_stores_a_pointer_and_commit_uploads_the_object() {
    let sb = Sandbox::new();
    let bytes = content(1, 3 << 20);
    let pointer = pointer_for(&bytes);
    sb.write(&sb.work, "data/a.bin", &bytes);
    sb.git(&sb.work, &["add", "data/a.bin"]);
    assert_eq!(sb.index_pointer(&sb.work, "data/a.bin"), Some(pointer.clone()));
    assert_eq!(sb.read(&sb.work, "data/a.bin"), bytes, "the working tree keeps the content");
    assert!(sb.work.join(".git/hooks/pre-commit").is_file(), "the filter installs the hooks on first use");
    assert!(sb.work.join(".git/hooks/pre-push").is_file());
    assert!(!sb.remote_has(&pointer));

    sb.git(&sb.work, &["commit", "-q", "-m", "add a"]);
    assert!(sb.remote_has(&pointer), "the pre-commit hook uploaded the object");
    assert_eq!(fs::read(sb.remote.join(&pointer.oid)).unwrap(), bytes);
    assert_eq!(sb.status(&sb.work), "");
    sb.hydrate(&sb.work, &["verify"]);

    let empty = sb.work.join("data/empty.bin");
    fs::write(&empty, b"").unwrap();
    sb.git(&sb.work, &["add", "data/empty.bin"]);
    assert_eq!(sb.git(&sb.work, &["show", ":data/empty.bin"]), "", "an empty file stays empty");
    sb.git(&sb.work, &["commit", "-q", "-m", "add empty"]);
}

#[test]
fn a_commit_refuses_a_staged_file_that_changed_after_add() {
    let sb = Sandbox::new();
    let staged = content(12, 1 << 20);
    sb.write(&sb.work, "data/a.bin", &staged);
    sb.git(&sb.work, &["add", "data/a.bin"]);
    sb.write(&sb.work, "data/a.bin", &content(13, 1 << 20));
    let output = sb.fails("git", &sb.work, &["commit", "-q", "-m", "stale"]);
    assert!(output.contains("data/a.bin"), "{output}");
    assert!(output.contains("could not be uploaded"), "{output}");
    assert!(!sb.remote_has(&pointer_for(&staged)));
    assert_eq!(sb.git(&sb.work, &["log", "--oneline"]).lines().count(), 1, "no commit was made");
}

#[test]
fn clones_hold_pointers_and_hydrate_on_request() {
    let sb = Sandbox::new();
    let bytes = content(2, 2 << 20);
    let pointer = pointer_for(&bytes);
    sb.write(&sb.work, "data/a.bin", &bytes);
    sb.write(&sb.work, "notes.txt", b"a label\n");
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "-m", "add a"]);
    sb.git(&sb.work, &["push", "-q", "origin", "main"]);

    let clone = sb.clone("clone");
    assert_eq!(Pointer::parse(&sb.read(&clone, "data/a.bin")), Some(pointer.clone()), "a clone holds pointers");
    assert_eq!(sb.read(&clone, "notes.txt"), b"a label\n");
    let status = sb.hydrate(&clone, &["status", "--porcelain"]);
    assert_eq!(status, format!("dehydrated\tremote\t{}\t{}\tdata/a.bin\n", pointer.oid, pointer.size));

    sb.hydrate(&clone, &["data/a.bin"]);
    assert_eq!(sb.read(&clone, "data/a.bin"), bytes);
    assert_eq!(sb.status(&clone), "", "a hydrated file is clean");
    assert!(sb.hydrate(&clone, &["status", "--porcelain"]).starts_with("hydrated\tremote\t"));
    sb.hydrate(&clone, &["pull", "data/a.bin"]);
    assert_eq!(sb.read(&clone, "data/a.bin"), bytes, "hydrating twice is harmless");

    let sub = clone.join("data");
    sb.run(DEHYDRATE, &sub, &["a.bin"]);
    assert_eq!(Pointer::parse(&sb.read(&clone, "data/a.bin")), Some(pointer.clone()));
    assert_eq!(sb.status(&clone), "", "a dehydrated file is clean");
    sb.hydrate(&sub, &["a.bin"]);
    assert_eq!(sb.read(&clone, "data/a.bin"), bytes, "pathspecs resolve from a subdirectory");
}

#[test]
fn dehydrate_refuses_what_only_the_working_tree_holds() {
    let sb = Sandbox::new();
    let bytes = content(3, 1 << 20);
    sb.write(&sb.work, "data/a.bin", &bytes);
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "--no-verify", "-m", "add a without hooks"]);
    assert!(!sb.remote_has(&pointer_for(&bytes)));

    let output = sb.fails(DEHYDRATE, &sb.work, &["data/a.bin"]);
    assert!(output.contains("not in the remote"), "{output}");
    assert_eq!(sb.read(&sb.work, "data/a.bin"), bytes);

    let changed = content(4, 1 << 20);
    sb.write(&sb.work, "data/a.bin", &changed);
    assert_eq!(sb.status(&sb.work), " M data/a.bin\n");
    let output = sb.fails(DEHYDRATE, &sb.work, &["data/a.bin"]);
    assert!(output.contains("not committed"), "{output}");
    assert_eq!(sb.read(&sb.work, "data/a.bin"), changed);

    sb.git(&sb.work, &["add", "data/a.bin"]);
    sb.git(&sb.work, &["commit", "-q", "-m", "change a"]);
    assert!(sb.remote_has(&pointer_for(&changed)));
    sb.run(DEHYDRATE, &sb.work, &["data/a.bin"]);
    assert_eq!(Pointer::parse(&sb.read(&sb.work, "data/a.bin")), Some(pointer_for(&changed)));
    assert_eq!(sb.status(&sb.work), "");
}

#[test]
fn the_push_hook_uploads_what_a_hookless_commit_left_and_refuses_ghosts() {
    let sb = Sandbox::new();
    let bytes = content(14, 1 << 20);
    let pointer = pointer_for(&bytes);
    sb.write(&sb.work, "data/a.bin", &bytes);
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "--no-verify", "-m", "add a without hooks"]);
    assert!(!sb.remote_has(&pointer));
    sb.git(&sb.work, &["push", "-q", "origin", "main"]);
    assert!(sb.remote_has(&pointer), "the pre-push hook uploaded the object");
    assert_eq!(sb.head(&sb.work), sb.git(&sb.origin, &["rev-parse", "refs/heads/main"]).trim());

    let ghost = Pointer { oid: "7".repeat(64), size: 42 };
    sb.write(&sb.work, "data/ghost.bin", &ghost.to_bytes());
    sb.git(&sb.work, &["add", "data/ghost.bin"]);
    assert_eq!(sb.index_pointer(&sb.work, "data/ghost.bin"), Some(ghost), "a pointer passes through clean");
    let output = sb.fails("git", &sb.work, &["commit", "-q", "-m", "ghost"]);
    assert!(
        output.contains("neither the remote nor the working tree"),
        "the pre-commit hook refuses a pointer to nothing: {output}"
    );

    sb.git(&sb.work, &["commit", "-q", "--no-verify", "-m", "ghost"]);
    let output = sb.fails("git", &sb.work, &["push", "-q", "origin", "main"]);
    assert!(output.contains("data/ghost.bin"), "{output}");
    assert!(output.contains("neither the remote nor the working tree"), "{output}");
    assert_ne!(sb.head(&sb.work), sb.git(&sb.origin, &["rev-parse", "refs/heads/main"]).trim(), "the push was refused");
}

#[test]
fn the_push_hook_walks_from_the_tracking_refs_when_the_remote_tip_is_unknown() {
    let sb = Sandbox::new();
    let a = content(21, 1 << 16);
    sb.write(&sb.work, "data/a.bin", &a);
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "-m", "add a"]);
    sb.git(&sb.work, &["push", "-q", "origin", "main"]);
    let other = sb.clone("other");

    let b = content(22, 1 << 16);
    sb.write(&sb.work, "data/b.bin", &b);
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "-m", "add b"]);
    sb.git(&sb.work, &["push", "-q", "origin", "main"]);
    let origin_tip = sb.git(&sb.origin, &["rev-parse", "refs/heads/main"]).trim().to_string();
    assert!(
        !sb.command("git", &other, &["cat-file", "-e", &origin_tip]).status.success(),
        "the other clone never fetched the remote's tip"
    );

    let c = content(23, 1 << 16);
    let c_pointer = pointer_for(&c);
    sb.write(&other, "data/c.bin", &c);
    let ghost = Pointer { oid: "8".repeat(64), size: 42 };
    sb.write(&other, "data/ghost.bin", &ghost.to_bytes());
    sb.git(&other, &["add", "."]);
    sb.git(&other, &["commit", "-q", "--no-verify", "-m", "add c and a ghost without hooks"]);
    let output = sb.fails("git", &other, &["push", "-q", "--force", "origin", "main"]);
    assert!(output.contains("data/ghost.bin"), "the hook still walks the pushed commits: {output}");
    assert!(!output.contains("bad object"), "the hook does not choke on the unknown tip: {output}");
    assert_eq!(sb.git(&sb.origin, &["rev-parse", "refs/heads/main"]).trim(), origin_tip, "the push was refused");
    assert!(sb.remote_has(&c_pointer), "the hook uploaded what the hookless commit left before refusing");

    sb.git(&other, &["reset", "-q", "--hard", "HEAD~1"]);
    sb.write(&other, "data/c.bin", &c);
    sb.git(&other, &["add", "data/c.bin"]);
    sb.git(&other, &["commit", "-q", "--no-verify", "-m", "add c without hooks"]);
    sb.git(&other, &["push", "-q", "--force", "origin", "main"]);
    assert_eq!(sb.head(&other), sb.git(&sb.origin, &["rev-parse", "refs/heads/main"]).trim());
}

#[test]
fn switching_branches_dehydrates_only_what_changed() {
    let sb = Sandbox::new();
    let a1 = content(5, 1 << 20);
    let b = content(6, 1 << 20);
    sb.write(&sb.work, "data/a.bin", &a1);
    sb.write(&sb.work, "data/b.bin", &b);
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "-m", "a1 b"]);
    sb.git(&sb.work, &["checkout", "-q", "-b", "other"]);
    let a2 = content(7, 1 << 20);
    sb.write(&sb.work, "data/a.bin", &a2);
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "-m", "a2"]);
    sb.git(&sb.work, &["push", "-q", "origin", "main", "other"]);
    assert!(sb.remote_has(&pointer_for(&a1)), "the overwritten version was uploaded when it was committed");

    assert_eq!(sb.read(&sb.work, "data/a.bin"), a2);
    sb.git(&sb.work, &["checkout", "-q", "main"]);
    assert_eq!(
        Pointer::parse(&sb.read(&sb.work, "data/a.bin")),
        Some(pointer_for(&a1)),
        "the changed file became its pointer"
    );
    assert_eq!(sb.read(&sb.work, "data/b.bin"), b, "the unchanged file stayed hydrated");
    assert_eq!(sb.status(&sb.work), "");
    sb.hydrate(&sb.work, &["--all"]);
    assert_eq!(sb.read(&sb.work, "data/a.bin"), a1);

    sb.write(&sb.work, "data/a.bin", b"uncommitted\n");
    let output = sb.fails("git", &sb.work, &["checkout", "-q", "other"]);
    assert!(output.contains("would be overwritten"), "{output}");
}

#[test]
fn export_writes_a_hydrated_tree_and_follows_the_branch() {
    let sb = Sandbox::new();
    let a1 = content(8, 1 << 20);
    let b = content(9, 1 << 20);
    sb.write(&sb.work, "data/a.bin", &a1);
    sb.write(&sb.work, "data/b.bin", &b);
    sb.write(&sb.work, "data/label.json", b"{\"v\":1}\n");
    sb.write(&sb.work, "gone.txt", b"gone\n");
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "-m", "first"]);
    let first = sb.head(&sb.work);

    let out = sb.root.path().join("export");
    sb.hydrate(&sb.work, &["export", "HEAD", out.to_str().unwrap(), "--only", "data/a.bin"]);
    assert_eq!(fs::read(out.join("data/a.bin")).unwrap(), a1);
    assert_eq!(
        Pointer::parse(&fs::read(out.join("data/b.bin")).unwrap()),
        Some(pointer_for(&b)),
        "--only leaves the rest as pointers"
    );
    assert_eq!(fs::read(out.join("data/label.json")).unwrap(), b"{\"v\":1}\n");
    assert_eq!(fs::read_to_string(out.join(".hydrate-commit")).unwrap().trim(), first);
    assert!(!out.join(".git").exists());

    let a2 = content(10, 1 << 20);
    sb.write(&sb.work, "data/a.bin", &a2);
    sb.write(&sb.work, "data/label.json", b"{\"v\":2}\n");
    sb.git(&sb.work, &["rm", "-q", "gone.txt"]);
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "-m", "second"]);
    let second = sb.head(&sb.work);

    let stdout = sb.hydrate(&sb.work, &["export", "main", out.to_str().unwrap(), "--verify"]);
    assert!(stdout.is_empty(), "export reports on stderr");
    assert_eq!(fs::read(out.join("data/a.bin")).unwrap(), a2);
    assert_eq!(fs::read(out.join("data/b.bin")).unwrap(), b, "no --only hydrates everything");
    assert_eq!(fs::read(out.join("data/label.json")).unwrap(), b"{\"v\":2}\n");
    assert!(!out.join("gone.txt").exists(), "a path the commit dropped is removed");
    assert_eq!(fs::read_to_string(out.join(".hydrate-commit")).unwrap().trim(), second);

    fs::write(out.join("data/b.bin"), b"corrupted").unwrap();
    sb.hydrate(&sb.work, &["export", "main", out.to_str().unwrap()]);
    assert_eq!(fs::read(out.join("data/b.bin")).unwrap(), b, "a rerun repairs a file of the wrong size");
}

#[test]
fn adopt_points_at_an_object_the_remote_already_holds() {
    let sb = Sandbox::new();
    let bytes = content(11, 1 << 20);
    let pointer = pointer_for(&bytes);
    fs::create_dir_all(&sb.remote).unwrap();
    fs::write(sb.remote.join(&pointer.oid), &bytes).unwrap();

    let output = sb.hydrate(&sb.work, &["adopt", "data/c.bin", &pointer.oid, &pointer.size.to_string()]);
    assert!(output.contains("data/c.bin: pointer written"), "{output}");
    assert_eq!(Pointer::parse(&sb.read(&sb.work, "data/c.bin")), Some(pointer.clone()));
    sb.git(&sb.work, &["add", "data/c.bin"]);
    assert_eq!(sb.index_pointer(&sb.work, "data/c.bin"), Some(pointer.clone()));
    sb.git(&sb.work, &["commit", "-q", "-m", "adopt c"]);
    sb.git(&sb.work, &["push", "-q", "origin", "main"]);
    sb.hydrate(&sb.work, &["data/c.bin"]);
    assert_eq!(sb.read(&sb.work, "data/c.bin"), bytes);

    let output = sb.hydrate_fails(&sb.work, &["adopt", "data/d.bin", &"0".repeat(64), "5"]);
    assert!(output.contains("does not hold"), "{output}");
    let output = sb.hydrate_fails(&sb.work, &["adopt", "data/e.bin", &pointer.oid, "1"]);
    assert!(output.contains("not 1"), "{output}");
}

#[test]
fn env_and_track_report_the_configuration() {
    let sb = Sandbox::new();
    let env = sb.hydrate(&sb.work, &["env"]);
    assert!(env.contains("filter.hydrate.process = git-hydrate filter-process"), "{env}");
    assert!(env.contains("hydrate.remote = file://"), "{env}");
    assert!(env.contains("(.hydrateconfig)"), "{env}");
    assert!(env.contains("pre-commit hook = "), "{env}");
    assert!(env.contains("(present)"), "{env}");
    let track = sb.hydrate(&sb.work, &["track", "*.bin", "*.mp4"]);
    assert!(track.contains("*.bin is already tracked"), "{track}");
    assert!(track.contains("tracking *.mp4"), "{track}");
    let attributes = fs::read_to_string(sb.work.join(".gitattributes")).unwrap();
    assert_eq!(attributes, "*.bin filter=hydrate -text\n*.mp4 filter=hydrate -text\n");
}

/// Bytes that differ on every run, so an S3 test uploads rather than finding its objects already there.
fn fresh_content(len: usize) -> Vec<u8> {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
        ^ (std::process::id() as u64) << 32;
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

#[test]
fn an_s3_remote_round_trips_multipart_objects() {
    let Some(sb) = Sandbox::with_s3() else {
        eprintln!("skipped: set GIT_HYDRATE_TEST_S3_REMOTE, and the endpoint and region, to run against S3");
        return;
    };
    let big = fresh_content(12 << 20);
    let small = fresh_content(100 << 10);
    sb.write(&sb.work, "data/big.bin", &big);
    sb.write(&sb.work, "data/small.bin", &small);
    sb.git(&sb.work, &["add", "."]);
    sb.git(&sb.work, &["commit", "-q", "-m", "add big and small"]);
    sb.hydrate(&sb.work, &["verify"]);
    let status = sb.hydrate(&sb.work, &["status", "--porcelain"]);
    assert!(status.lines().all(|line| line.starts_with("hydrated\tremote\t")), "{status}");
    sb.git(&sb.work, &["push", "-q", "origin", "main"]);

    let clone = sb.clone("clone");
    assert_eq!(Pointer::parse(&sb.read(&clone, "data/big.bin")), Some(pointer_for(&big)));
    sb.hydrate(&clone, &["--all"]);
    assert_eq!(sb.read(&clone, "data/big.bin"), big, "a multipart object comes back whole");
    assert_eq!(sb.read(&clone, "data/small.bin"), small);
    assert_eq!(sb.status(&clone), "");

    sb.run(DEHYDRATE, &clone, &["--all"]);
    assert_eq!(Pointer::parse(&sb.read(&clone, "data/small.bin")), Some(pointer_for(&small)));
    sb.hydrate(&clone.join("data"), &["big.bin"]);
    assert_eq!(sb.read(&clone, "data/big.bin"), big);

    let out = sb.root.path().join("export");
    sb.hydrate(&sb.work, &["export", "HEAD", out.to_str().unwrap()]);
    assert_eq!(fs::read(out.join("data/big.bin")).unwrap(), big);

    let pointer = pointer_for(&small);
    let output = sb.hydrate_fails(&sb.work, &["adopt", "data/again.bin", &pointer.oid, "1"]);
    assert!(output.contains("not 1"), "{output}");
    sb.hydrate(&sb.work, &["adopt", "data/again.bin", &pointer.oid, &pointer.size.to_string()]);
    sb.git(&sb.work, &["add", "data/again.bin"]);
    sb.git(&sb.work, &["commit", "-q", "-m", "adopt again"]);
    sb.hydrate(&sb.work, &["data/again.bin"]);
    assert_eq!(sb.read(&sb.work, "data/again.bin"), small);
}
