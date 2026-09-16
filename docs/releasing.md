# Releasing AgentChecksum

A release is a tag. `.github/workflows/release.yml` turns the tag into the four archives,
the `SHA256SUMS` file and the GitHub Release, and it refuses to build anything if the tag
and the version in `Cargo.toml` disagree — a mistagged release would otherwise publish a
binary whose `--version` contradicts the tag it is filed under.

This document is the maintainer checklist. It describes what the workflow does, so the two
cannot drift apart without the document being wrong.

Nothing here publishes to crates.io. That is a separate, deliberate step, because a
crates.io version can never be replaced.

## What a release consists of

| Target | Built on | Asset |
| --- | --- | --- |
| `aarch64-apple-darwin` | `macos-14` | `agentchecksum-v<VERSION>-aarch64-apple-darwin.tar.gz` |
| `x86_64-apple-darwin` | `macos-15-intel` | `agentchecksum-v<VERSION>-x86_64-apple-darwin.tar.gz` |
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` | `agentchecksum-v<VERSION>-x86_64-unknown-linux-gnu.tar.gz` |
| `x86_64-pc-windows-msvc` | `windows-latest` | `agentchecksum-v<VERSION>-x86_64-pc-windows-msvc.zip` |

plus `SHA256SUMS`, one line per archive, sorted by file name, in the format `sha256sum`
writes and `sha256sum -c` reads.

Each archive is one directory holding four files and nothing else — no `target/`, no
build metadata, no per-machine differences:

```
agentchecksum-v<VERSION>-<TARGET>/
  agentchecksum            (agentchecksum.exe on Windows)
  README.md
  LICENSE-MIT
  LICENSE-APACHE
```

Each archive is built on a runner of its own architecture — these are native builds, not
cross builds — and `cargo build --release --locked --target <TARGET>` is given the triple
the binary must be, so a build that would produce the wrong architecture fails instead of
shipping under the wrong name.

## The checklist

1. **Set the version.** `version = "X.Y.Z"` in `Cargo.toml`. It is the only place the
   version is written: the workflow and `scripts/release_artifacts.py` read it from there,
   and the tag is checked against it.

2. **Write the entry.** `CHANGELOG.md`: what changed and why, in the plain version a user
   reads before upgrading. The release notes the workflow generates come from commit
   subjects; the changelog is the part written for a reader.

3. **Run what CI runs**, on the tree you are about to commit:
   ```bash
   cargo fmt --all --check
   cargo clippy --all-targets -- -D warnings
   cargo test --all-targets
   ```

4. **Commit.** The release commit is what the tag will point at.

5. **Check that it packages**, from that commit:
   ```bash
   cargo package --locked
   cargo publish --dry-run --locked
   ```
   Both refuse to run on a dirty working tree unless given `--allow-dirty`, and that
   refusal is the point: the package they check is the one the tag names. Run them after
   committing, not before — that is why this step follows step 4.

6. **Dry-run the release** (recommended before the first release of a new target, and
   after touching the workflow at all):
   ```bash
   gh workflow run release.yml
   gh run watch
   ```
   A dry run does everything except publish: it verifies the version, builds all four
   archives, smoke-tests them and uploads them as workflow artifacts. A release should
   fail in a dry run first, where failing is free.

7. **Tag it, annotated, and push the tag.**
   ```bash
   git tag -a vX.Y.Z -m "AgentChecksum vX.Y.Z"
   git push origin vX.Y.Z
   ```
   `vX.Y.Z` — the `v` is part of the tag, and `vX.Y.Z` must equal `v` + the version in
   `Cargo.toml`. The workflow checks this before it builds anything, so a typo here costs
   one red job instead of a wrong release.

8. **Watch the workflow.** It runs three jobs: the version check, one build per target,
   then the release. The release job writes `SHA256SUMS`, checks it with
   `sha256sum -c SHA256SUMS`, and only then attaches everything to the GitHub Release for
   the tag.

9. **Read the release.** All four archives and `SHA256SUMS` are there:
   ```bash
   gh release view vX.Y.Z
   ```

10. **Publish the crate**, if this release is meant for crates.io — see
    [Publishing the crate](#publishing-the-crate). The workflow never does this.

11. **Move the Action's major tag**, if one exists — see
    [The moving Action tag](#the-moving-action-tag).

## Verifying a release as a user

What a user is expected to do after downloading, and worth doing once yourself against the
real release page. `SHA256SUMS` covers all four archives, so either check the one you
downloaded against its line, or download all four and check the file:

```bash
curl -LO https://github.com/f0rknturkoglu/agentchecksum/releases/download/v0.1.0/SHA256SUMS
curl -LO https://github.com/f0rknturkoglu/agentchecksum/releases/download/v0.1.0/agentchecksum-v0.1.0-<target>.tar.gz
grep " agentchecksum-v0.1.0-<target>.tar.gz$" SHA256SUMS | shasum -a 256 -c -   # sha256sum -c - on Linux
tar -xzf agentchecksum-v0.1.0-<target>.tar.gz
./agentchecksum-v0.1.0-<target>/agentchecksum --version
```

(`shasum -a 256 -c SHA256SUMS` checks all four at once once every archive is in the
directory.)
On Windows, `Expand-Archive -Path agentchecksum-v0.1.0-x86_64-pc-windows-msvc.zip -DestinationPath .`
and then `.\agentchecksum-v0.1.0-x86_64-pc-windows-msvc\agentchecksum.exe --version`.
`--version` prints `agentchecksum 0.1.0` — the version in `Cargo.toml`, not the tag, and
the workflow asserts the same thing about the binary inside each archive before it is
uploaded.

## Publishing the crate

This is separate from the GitHub Release, and the workflow deliberately does not do it: a
crates.io version cannot be replaced, so it should be pushed by a human who has just read
the dry run.

```bash
cargo publish --locked
```

The version on crates.io is the version in `Cargo.toml` at the tagged commit. If the crate
was already published and you later decide the release is wrong, you cannot reuse that
version: yank it (`cargo yank --version X.Y.Z`) and release the next patch version
instead.

## The moving Action tag

`action.yml` lets a workflow run the CLI with `uses: f0rknturkoglu/agentchecksum@vX.Y.Z`.
An exact tag is the honest default. If the project also publishes a *moving* tag so users
can follow a line — `vX`, or `vX.Y` — that tag is moved after the release, and only that
tag is ever moved:

```bash
git tag -f vX
git push --force origin vX
```

Never move `vX.Y.Z`: it names one commit, one set of binaries and one `SHA256SUMS`, and a
user who verified a download against it must be able to still do so tomorrow. There is no
moving tag today, so this step is a no-op until one is created.

## When a release is wrong

The rule is: delete, fix, re-tag. Never move a published tag silently — anyone who already
downloaded the archive has a checksum that would then disagree with the release page.

```bash
gh release delete vX.Y.Z --cleanup-tag --yes     # the release and the tag
git tag -d vX.Y.Z                                # the local tag
git fetch --tags
```

Then fix the problem on `main` and start again from step 1. Re-tagging the same version is
fine as long as the crate was not published to crates.io; if it was, the version is spent
and the fix belongs in the next patch version.

A wrong tag is caught before anything is built, so the common case costs one failed job and
no release at all: fix the tag (`git tag -d`, re-tag, push) rather than the version.

## What the workflow runs

For reference, so this document can be checked against the workflow. Every command below is
in `.github/workflows/release.yml`; the subcommands are
`scripts/release_artifacts.py`, which reads the version and the package name from
`Cargo.toml` rather than repeating them.

| Job | Runner | What it runs |
| --- | --- | --- |
| `verify-tag` | `ubuntu-latest` | `release_artifacts.py check-tag --tag "$GITHUB_REF_NAME" --event "$GITHUB_EVENT_NAME"` |
| `build` | one per target | compiler pin check, `cargo build --release --locked --target <TARGET>`, `release_artifacts.py package`, `release_artifacts.py smoke` (with `--project` on the Linux and macOS runners), `actions/upload-artifact` |
| `release` | `ubuntu-latest` | `actions/download-artifact`, `release_artifacts.py checksums`, `sha256sum -c SHA256SUMS`, `gh release create`/`gh release upload` |

`smoke` is the part that keeps the release honest: it extracts the archive that will be
uploaded, asserts it contains exactly the four expected files, and runs the binary inside
it — `--version` must print the crate version, `--help` must print help, and on the Linux
and macOS runners a throwaway project is then taken end to end (`init`, a prompt file,
`snapshot`, `diff`) to prove the packaged binary finds its configuration and can discover
dependencies.

`package` writes the archive deterministically — fixed member order, fixed timestamp,
fixed owner, no gzip timestamp — so rebuilding the same binary produces the same file, and
`SHA256SUMS` means something across a rebuild. A rebuild that produces different bytes is
therefore a real difference (a different compiler, a different binary, a different input),
not noise.
