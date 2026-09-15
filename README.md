# git-hydrate

git-hydrate keeps large files out of a git repository and in an object store, with one copy on disk and no local cache. A tracked file is committed as a three-line pointer in git-lfs's format. Its bytes live in an S3 bucket or a directory, keyed by their content hash. A checkout holds the pointer until you ask for the file. The tool is git-lfs's pointer format and git's own filter mechanism with the object store removed.

## Why

git-lfs downloads every object into `.git/lfs/objects` and then copies it into the working tree. A checkout therefore holds each materialized file twice. Both copies stay until you prune or deduplicate. That is the right trade for a source repository with a few large assets, because switching branches never re-downloads. It is the wrong trade for a repository that is mostly large files and larger than the disk it is checked out on. git-hydrate makes the other trade. The working tree is the only local copy. Hydrating a file downloads it into place. Dehydrating a file replaces it with its pointer. Switching branches re-downloads what changed. Nothing is cached anywhere.

## How it works

Git stores pointers. A pointer is the git-lfs pointer file, unchanged:

```
version https://git-lfs.github.com/spec/v1
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393
size 734003200
```

The remote stores bytes. An object lives at `<remote>/<oid>`, is written once, and is never modified, so nothing about the remote needs versioning.

A `.gitattributes` line names the tracked paths:

```
*.mp4 filter=hydrate -text
```

The filter is git's long-running filter process, registered once per machine by `git hydrate install`. Its two halves are deliberately asymmetric. Smudge is the identity, so a checkout writes pointers and git never downloads anything. Clean hashes whatever git hands it and emits the pointer. A hydrated file therefore cleans back to the pointer git already holds, so git sees it as unmodified. A new file becomes a pointer on `git add`. The filter is registered as required, so a machine without git-hydrate cannot check out or add a tracked path. No tracked file can ever be committed as raw bytes.

Two hooks make commits durable. The pre-commit hook uploads the object behind every staged pointer, so a commit's objects are in the remote the moment the commit exists. The pre-push hook uploads whatever a commit made without hooks left behind and refuses the push if an object exists in neither the remote nor the working tree. Between them, no commit that reaches a remote repository points at bytes nobody has.

The tool moves bytes. `git hydrate <paths>` downloads objects into the working tree and verifies every hash. `git dehydrate <paths>` writes pointers back and refuses any file whose object is not in the remote. `git hydrate export <ref> <dir>` writes the tree at a commit into a plain directory, hydrated, for machines that read a dataset and never commit to it.

Everything that decides what is modified, what is clean, and what a checkout may overwrite is git. git-hydrate answers one question, what does this content hash to, and moves bytes on request.

## Install

Each release ships binaries for macOS, Linux, and Windows with an installer script that picks the right one and puts it on your `PATH`:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/c9r/git-hydrate/releases/latest/download/git-hydrate-installer.sh | sh
```

On Windows, the same release carries `git-hydrate-installer.ps1`. Or build from source with Cargo:

```sh
cargo install --git https://github.com/c9r/git-hydrate
```

Then register the filter once per machine:

```sh
git hydrate install
```

This writes `filter.hydrate.process` and `filter.hydrate.required` to your global git config. The crate installs two binaries, `git-hydrate` and `git-dehydrate`, so both read as git subcommands.

## Quick start

In a repository, name the remote and the tracked paths, then commit both files:

```sh
git config -f .hydrateconfig hydrate.remote s3://my-bucket/objects
git config -f .hydrateconfig hydrate.endpoint https://s3.example.com
git config -f .hydrateconfig hydrate.region us-east-1
git hydrate track '*.mp4' '*.bin'
git add .hydrateconfig .gitattributes
git commit -m 'track large files with git-hydrate'
```

Add a large file the way you add anything:

```sh
cp ~/take-17.mp4 footage/take-17/take-17.mp4
git add footage/take-17/take-17.mp4
git commit -m 'add take 17'
git push
```

The index holds a pointer, the commit holds a pointer, and the pre-commit hook uploaded the object before the commit was made. On another machine:

```sh
git clone git@github.com:you/footage.git
cd footage
git hydrate footage/take-17
```

The clone was instant, because it wrote pointers. The hydrate downloaded one take. When the disk fills up:

```sh
git dehydrate footage/take-17
```

The files are pointers again, and the objects are still in the remote.

## Commands

`git hydrate [pull] <pathspec>...` downloads the objects for tracked pointers matching the pathspecs and writes them into place, one temporary file and one rename per object, verifying the hash as it downloads. Files that are already hydrated are skipped. Pass `--all` to hydrate every tracked file in the repository. After the downloads, git refreshes its record of each file, which reads each file once, so `git status` is instant afterwards.

`git hydrate drop <pathspec>...`, also `git dehydrate <pathspec>...`, replaces hydrated files with their pointers. A file is refused when it differs from the pointer in the index, because that content is not committed, and when its object is missing from the remote, because that content is not pushed. Nothing whose only copy is the working tree can be dropped.

`git hydrate push [<pathspec>...]` uploads every object the index points to that the remote does not have, taking the bytes from the working tree and verifying they hash to the pointer's oid. The hooks do this for you on `git commit` and `git push`. The command exists for scripts and for repairing after a commit made with `--no-verify`.

`git hydrate status [<pathspec>...]` lists tracked files with their state, hydrated, dehydrated, modified, or missing, and whether the object is in the remote. Pass `--no-remote` to skip asking the remote and `--porcelain` for tab-separated fields.

`git hydrate verify [<tree-ish>]` checks that every pointer in a tree, by default `HEAD`, has its object in the remote.

`git hydrate export <tree-ish> <dir> [--only <glob>...]` writes the tree into a directory that is not a repository, with every pointer replaced by its object, or only the pointers matching `--only`. The directory records the commit it holds in `.hydrate-commit`. Running the export again against a newer commit downloads only what changed and removes what the commit dropped, so the directory tracks a branch by rerunning one command. An interrupted export is finished by running it again. `--verify` hashes every object already present instead of trusting its size.

`git hydrate adopt <path> <oid> <size>` writes a pointer for an object that already exists in the remote, after confirming it is there at that size. This is how a job that wrote its output straight to the remote enters the repository without the bytes ever passing through the machine that commits.

`git hydrate track <pattern>...` appends `filter=hydrate -text` lines for the patterns to `.gitattributes`.

`git hydrate env` prints the resolved configuration, where each value came from, and the state of the filter and the hooks.

`git hydrate install [--local]` and `git hydrate uninstall [--local]` register and unregister the filter, globally by default, and place or remove the hooks in the current repository.

`git hydrate filter-process`, `git hydrate pre-commit`, and `git hydrate pre-push` are the entry points git calls. You never run them.

## Configuration

Configuration is git config. Values are read from the repository's own config first, then from a committed `.hydrateconfig` file at the root of the working tree, then from the global and system config. A repository commits its remote once, and a machine can override it.

| Key | Meaning | Default |
|---|---|---|
| `hydrate.remote` | Where objects live, `s3://bucket/prefix` or `file:///path` | required |
| `hydrate.endpoint` | S3 endpoint URL for storage that is not AWS | AWS |
| `hydrate.region` | S3 signing region | from the profile or environment |
| `hydrate.profile` | AWS credentials profile name | the default chain |
| `hydrate.pathstyle` | Use path-style S3 requests | `false` |
| `hydrate.partsize` | Bytes per transfer part and multipart upload part, with `k`, `m`, and `g` suffixes | 16 MiB |
| `hydrate.concurrency` | Files transferred at once | 4 |

Credentials come from the AWS SDK's default chain: environment variables, the shared credentials and config files, SSO, and instance metadata. `hydrate.profile` selects a named profile from the shared files. Nothing secret goes into git config.

## Remotes

An `s3://` remote is any S3 API. The endpoint and region keys point it at storage that is not AWS. Objects are uploaded as multipart uploads with parallel parts and downloaded as parallel ranged reads. Both directions hash the bytes in order as they pass, so a corrupted transfer is detected before the file is renamed into place or the upload is completed. The SDK's checksum headers are turned off, because the sha256 is the integrity check and storage that predates those headers rejects them.

A `file://` remote is a directory. Objects are files named by oid. This suits a shared filesystem that every machine mounts, and it is what the tests use.

## The hooks

The first time the filter runs in a repository it places a pre-commit hook and a pre-push hook, unless a hook already exists there or `core.hooksPath` is set. In that case it says so and leaves yours alone. Each hook runs the verb of its own name.

The pre-commit hook finds the staged pointers, asks the remote which objects it lacks, and uploads those from the working tree. A staged file that no longer hashes to its pointer, because it was changed after `git add`, refuses the commit, so a pointer to nothing is never committed. Committing with `--no-verify` skips the upload. The pre-push hook picks it up.

The pre-push hook enumerates every pointer reachable from the commits being pushed and not from the remote's refs, uploads any object the remote lacks from the working tree, and aborts the push naming any object it can find nowhere.

That is the whole account of unpushed objects. There is no local store to lose them in, because an object reaches the remote when its commit is made, and until then `git dehydrate` refuses to remove it.

## Behavior worth knowing

An empty file is committed as an empty file, not a pointer, as in git-lfs.

Any content under 1024 bytes that parses as a pointer passes through clean unchanged. Pointers written by git-lfs or by `git hydrate adopt` are preserved. A repository can move between git-lfs and git-hydrate by changing the filter name in `.gitattributes` and putting the objects where the other tool looks.

A branch switch overwrites only the tracked files whose pointer differs between the two commits, and it overwrites them with pointers. Exactly the files that changed become dehydrated. A hydrated file with uncommitted modifications makes git refuse the switch, as it would for any modified file.

After `git hydrate` writes a file, git reads it once to record that it matches the pointer. That read happens inside the hydrate command. Nothing reads a hydrated file again until it changes.

A pipeline that rewrites a tracked file in place produces a modified file in `git status`, because its clean hash differs from the pointer in the index. `git add` records the new pointer, `git commit` uploads the object, and the previous object remains in the remote for the commits that point to it. The one window in which a tracked file's content exists only in the working tree is between `git add` and `git commit`, which is the window in which any uncommitted change exists only there.

Objects are never deleted from the remote. Every commit that ever existed can be hydrated.

## License

MIT. See `LICENSE`.
