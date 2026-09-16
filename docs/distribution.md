# Distribution

Where AgentChecksum is published, what each channel actually does, and how a release
updates them. Every command here has been run against the released `v0.1.0`.

The product is one binary per platform. Nothing in this document changes what it does —
only how it arrives.

| Channel | What it installs | Status |
|---|---|---|
| [GitHub Releases](#github-releases) | a prebuilt archive for four targets, or the source | live |
| [crates.io](#cargo--cratesio) | compiles from the published crate | live |
| [Homebrew](#homebrew) | a prebuilt macOS binary, no compilation | live |
| [cargo-binstall](#cargo-binstall) | a prebuilt archive, no compilation | prepared — active from the next published version |
| [GitHub Action](#github-action) | runs the CLI in a workflow, download verified against `SHA256SUMS` | live |

## GitHub Releases

Every release builds one archive per target on a native runner, smoke-tests the archive it
just produced, and attaches the archives plus a `SHA256SUMS` manifest. The archives are
built deterministically — fixed member order, timestamps, ownership and compression
settings — so a rebuild of the same input produces the same bytes, and a digest that
changes means something really changed.

```text
agentchecksum-v0.1.0-aarch64-apple-darwin.tar.gz        macOS, Apple Silicon
agentchecksum-v0.1.0-x86_64-apple-darwin.tar.gz         macOS, Intel
agentchecksum-v0.1.0-x86_64-unknown-linux-gnu.tar.gz    Linux, x86_64
agentchecksum-v0.1.0-x86_64-pc-windows-msvc.zip         Windows, x86_64
```

Each archive holds one directory named after the asset, containing the binary, `README.md`
and both license files. Verify a download before running it:

```bash
shasum -a 256 -c SHA256SUMS --ignore-missing
```

**Who updates it:** the release workflow, from the tag push. Nothing manual.

## Cargo / crates.io

```bash
cargo install agentchecksum --locked
```

Compiles from the published crate, so it needs a Rust toolchain and a few minutes, and it
is the only channel that works anywhere the compiler does. The published version is the
version in `Cargo.toml` at the tagged commit.

**Who updates it:** a maintainer, deliberately and separately from the GitHub Release,
because a crates.io version can never be replaced — see
[releasing.md](releasing.md#publishing-the-crate).

## Homebrew

```bash
brew tap f0rknturkoglu/tap
brew install f0rknturkoglu/tap/agentchecksum
```

Installs the prebuilt macOS binary — Apple Silicon on `arm64`, Intel on `x86_64` — and
compiles nothing. The formula lives in
[f0rknturkoglu/homebrew-tap](https://github.com/f0rknturkoglu/homebrew-tap) and points at
the immutable versioned release assets, with digests taken from the release's own
`SHA256SUMS` rather than transcribed.

**Who updates it:** a maintainer, with the generator below. See
[Maintainer update process](#maintainer-update-process).

## cargo-binstall

```bash
cargo binstall agentchecksum
```

Reads `[package.metadata.binstall]` from the crate's manifest and downloads the release
archive for the platform instead of compiling. The metadata is in `Cargo.toml` and maps the
release layout directly: one archive per target, named
`agentchecksum-v<version>-<target>.<ext>`, with the binary in a directory of the same name.

**Status: prepared, and active from the next published version.** `cargo-binstall` takes
that table from the manifest of the version it is asked to install, and the published
`0.1.0` predates it — so today `cargo binstall agentchecksum` still compiles from source.
It starts using the release binaries as soon as a version carrying this metadata is
published. Nothing else is missing: the metadata has been exercised against the live
release, with no compilation:

```bash
cargo binstall --manifest-path ./Cargo.toml --no-confirm agentchecksum@0.1.0
# WARN The package agentchecksum v0.1.0 (aarch64-apple-darwin) has been downloaded from github.com
```

**Who updates it:** nobody. The metadata is part of the manifest, and the release workflow
already produces the layout it points at.

## GitHub Action

```yaml
- uses: f0rknturkoglu/agentchecksum@v0.1
  with:
    command: check
```

`v0.1` is the moving tag that tracks the 0.1.x Action line; `v0.1.0` is the immutable
product release. A consumer that needs an immovable reference should pin the full commit
SHA instead of either tag.

The action downloads the release asset for the runner and verifies it against the release's
`SHA256SUMS` before running it; an archive that cannot be authenticated is refused with
exit `3` rather than executed. When there is no asset for the platform it builds from the
source it ships, which is what makes it work on a target with no release.

**Who updates it:** nobody for a release. `v0.1` moves only when the tagged action itself
changes; see [releasing.md](releasing.md#the-moving-action-tag).

## Maintainer update process

```bash
# After a release, from a checkout of this repository:
python3 scripts/release_artifacts.py homebrew --output /path/to/homebrew-tap/Formula/agentchecksum.rb
cd /path/to/homebrew-tap && git commit -am "agentchecksum <version>" && git push
```

The generator reads the version from `Cargo.toml`, refuses a tag that disagrees with it, and
refuses to run at all unless both macOS archives are listed in the release's own
`SHA256SUMS`. It writes the formula; a human reviews and commits it. Nothing in this
repository has write access to the tap, and nothing needs it: a release is one command and
a review, not a chain of manual hash edits.

## Deferred channels

Scoop, WinGet, Nix, AUR, Docker images and the rest are not planned work. The releases are
shaped so that adding one is a small, self-contained change — a stable name, one directory,
a checksum manifest that lists every asset — and it can be considered when there is demand
for it.
