# Changelog

## 0.1.2 (2026-09-19)

- The pre-push hook no longer fails when the remote's current tip is an object the repository does not hold, as after a history rewrite has pruned it or on a force push to a remote that moved since the last fetch. It walks from the remote's tracking refs instead, as it already did for a new branch, and checks more objects than it strictly needs to rather than refusing the push with a bad-object error.

## 0.1.1 (2026-09-15)

- Linux binaries are built against glibc on native runners, so a release runs on any distribution at least as new as the runner's.
- `git hydrate` and `git dehydrate` refuse `--all` together with paths instead of silently acting on everything.
- `git hydrate export` explains when the previous commit its marker records is gone, as after a rewritten branch, and names the marker to remove instead of failing inside diff-tree.
- A Homebrew formula is published with each release, so `brew install c9r/tap/git-hydrate` installs the current release and `brew upgrade` follows new ones.

## 0.1.0 (2026-09-15)

- The first release. The filter process hashes on clean and passes content through on smudge. Remotes are an S3 API or a directory of objects named by oid. The verbs are hydrate, dehydrate, push, status, verify, export, adopt, track, env, install, and uninstall. The pre-commit hook uploads staged objects so a commit is durable when it exists, and the pre-push hook refuses a push whose objects exist nowhere. Prebuilt binaries for macOS, Linux, and Windows ship with shell and PowerShell installers.
