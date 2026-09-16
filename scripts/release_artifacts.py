#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Build the release archives for AgentChecksum, check them, and checksum them.

This is the release tooling `.github/workflows/release.yml` drives. It is deliberately
one script rather than a shell step per platform: the same code runs on the Linux, macOS
and Windows runners, so the four archives cannot come out of four different
implementations of the same idea.

Subcommands:

  version                 Print the version in `Cargo.toml`.
  check-tag --tag T --event E
                          Refuse a tag that disagrees with the crate version.
  package --target T      Build `agentchecksum-v<VERSION>-<T>.tar.gz` (or `.zip` for
                          Windows) from `target/<T>/release/`.
  smoke --target T [--project]
                          Extract the archive and exercise the binary inside it.
  checksums               Write `SHA256SUMS` over the archives, sorted by filename.

Every subcommand derives the version and the package name from `Cargo.toml`; nothing
here repeats a version number, so a release cannot ship an archive whose name
contradicts the binary inside it.

The archives are byte-for-byte reproducible for fixed inputs: members are written in a
fixed order with a fixed timestamp, owner and mode, and gzip carries no timestamp. Two
`package` runs over the same binary therefore produce the same file, which is what makes
`SHA256SUMS` meaningful across a rebuild.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile
from pathlib import Path
from typing import NoReturn

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "Cargo.toml"

# 1980-01-01T00:00:00Z: the earliest timestamp a zip can represent, so one constant
# serves both formats. Fixed on purpose — an archive's bytes should depend on what was
# packaged, not on when.
FIXED_MTIME = 315_532_800

# The files every archive carries, with the mode they are given on Unix.
PAYLOAD = (("README.md", 0o644), ("LICENSE-MIT", 0o644), ("LICENSE-APACHE", 0o644))

BINARY_MODE = 0o755


def archive_pattern(name: str) -> re.Pattern[str]:
    """The names a release may carry: nothing else is ever attached to a release."""
    return re.compile(rf"^{re.escape(name)}-v[0-9]+\.[0-9]+\.[0-9]+-[A-Za-z0-9_.-]+\.(?:tar\.gz|zip)$")


def fail(message: str) -> NoReturn:
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def crate(manifest: Path = MANIFEST) -> tuple[str, str]:
    """Return `(name, version)` from the `[package]` table."""
    if not manifest.is_file():
        fail(f"no manifest at {manifest}")
    with manifest.open("rb") as handle:
        package = tomllib.load(handle).get("package")
    if not isinstance(package, dict):
        fail(f"{manifest} has no [package] table")
    name, version = package.get("name"), package.get("version")
    if not isinstance(name, str) or not isinstance(version, str):
        fail(f"{manifest} has no [package] name and version")
    return name, version


def is_windows(target: str) -> bool:
    return "windows" in target


def binary_of(name: str, target: str) -> str:
    return f"{name}.exe" if is_windows(target) else name


def archive_of(name: str, version: str, target: str) -> str:
    # The Unix name is `.tar.gz` and the Windows one `.zip` because the archive is meant
    # to be openable by the platform it was built for.
    suffix = "zip" if is_windows(target) else "tar.gz"
    return f"{name}-v{version}-{target}.{suffix}"


def payload(name: str, version: str, target: str, binary: Path) -> list[tuple[str, bytes, int]]:
    """The archive's members, as `(path inside the archive, contents, mode)`."""
    stem = f"{name}-v{version}-{target}"
    if not binary.is_file():
        fail(f"no release binary at {binary}; build it first (cargo build --release --locked --target {target})")
    members = [(f"{stem}/{binary_of(name, target)}", binary.read_bytes(), BINARY_MODE)]
    for filename, mode in PAYLOAD:
        path = ROOT / filename
        if not path.is_file():
            fail(f"no {filename} at {path}")
        members.append((f"{stem}/{filename}", path.read_bytes(), mode))
    return members


def write_tar_gz(archive: Path, members: list[tuple[str, bytes, int]]) -> None:
    raw = io.BytesIO()
    # USTAR: no extended headers, so no pax metadata can sneak a timestamp back in.
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for path, contents, mode in members:
            info = tarfile.TarInfo(path)
            info.size = len(contents)
            info.mtime, info.mode = FIXED_MTIME, mode
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.type = tarfile.REGTYPE
            tar.addfile(info, io.BytesIO(contents))
    with archive.open("wb") as handle:
        # No filename in the gzip header and a fixed mtime: the compression wrapper
        # records nothing about this machine.
        with gzip.GzipFile(filename="", mode="wb", fileobj=handle, mtime=FIXED_MTIME) as gz:
            gz.write(raw.getvalue())


def write_zip(archive: Path, members: list[tuple[str, bytes, int]]) -> None:
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        for path, contents, _mode in members:
            info = zipfile.ZipInfo(path, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            # No Unix modes: this archive is opened on Windows, where they mean nothing.
            info.create_system = 0
            zf.writestr(info, contents, compresslevel=9)


def extract(archive: Path, into: Path) -> list[str]:
    """Extract `archive` and return the sorted relative paths it contained."""
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as zf:
            zf.extractall(into)
            return sorted(zf.namelist())
    with tarfile.open(archive, mode="r:gz") as tar:
        names = sorted(member.name for member in tar.getmembers())
        tar.extractall(into, filter="data")
        return names


def run(command: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(command, cwd=cwd, capture_output=True, text=True)
    except OSError as error:
        fail(f"could not run {command[0]}: {error}")


def check(result: subprocess.CompletedProcess[str], what: str) -> str:
    if result.returncode != 0:
        fail(f"{what} exited {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}")
    return result.stdout


def ok(message: str) -> None:
    print(f"ok  {message}")


def cmd_version(_args: argparse.Namespace) -> None:
    print(crate()[1])


def cmd_check_tag(args: argparse.Namespace) -> None:
    name, version = crate()
    if args.event != "push":
        # A dry run has no tag to check. Say which tag a real release would need, so the
        # log states the expectation rather than leaving it implicit.
        print(f"dry run ({args.event}): nothing is published; a release would need the tag v{version}")
        return
    if args.tag != f"v{version}":
        fail(f"tag {args.tag} does not match {name} {version} in Cargo.toml (expected v{version})")
    print(f"tag {args.tag} matches {name} {version}")


def cmd_package(args: argparse.Namespace) -> None:
    name, version = crate()
    binary = ROOT / "target" / args.target / "release" / binary_of(name, args.target)
    archive = args.dist / archive_of(name, version, args.target)
    args.dist.mkdir(parents=True, exist_ok=True)
    members = payload(name, version, args.target, binary)
    if is_windows(args.target):
        write_zip(archive, members)
    else:
        write_tar_gz(archive, members)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    print(f"{archive.relative_to(ROOT)}\n  sha256:{digest}\n  {len(members)} members")


def cmd_smoke(args: argparse.Namespace) -> None:
    name, version = crate()
    stem = f"{name}-v{version}-{args.target}"
    archive = args.dist / archive_of(name, version, args.target)
    if not archive.is_file():
        fail(f"no archive at {archive}; run `package --target {args.target}` first")

    with tempfile.TemporaryDirectory(prefix="agentchecksum-smoke-") as workspace:
        root = Path(workspace)
        members = extract(archive, root)
        binary_in_archive = f"{stem}/{binary_of(name, args.target)}"
        expected = sorted([binary_in_archive, *[f"{stem}/{filename}" for filename, _ in PAYLOAD]])
        if members != expected:
            fail(f"{archive.name} holds {members}, expected {expected}")
        ok(f"{archive.name}: {len(members)} members, layout as expected")

        binary = root / binary_in_archive
        reported = check(run([str(binary), "--version"], root), f"{binary.name} --version").strip()
        if reported != f"{name} {version}":
            fail(f"{binary.name} --version printed {reported!r}, expected {name} {version}")
        ok(f"{binary.name} --version prints the crate version ({reported})")

        help_text = check(run([str(binary), "--help"], root), f"{binary.name} --help")
        if "Usage:" not in help_text:
            fail(f"{binary.name} --help printed something that is not help:\n{help_text}")
        ok(f"{binary.name} --help is help")

        if args.project:
            project = root / "project"
            project.mkdir()
            check(run([str(binary), "init"], project), "init")
            config = project / "agentchecksum.toml"
            if not (project / "probes" / "no-tools.toml").is_file() or not config.is_file():
                fail("init did not scaffold agentchecksum.toml and probes/no-tools.toml")
            # `init` writes the prompt paths it expects but not the prompts, so the
            # project supplies them: a config whose `[[prompts]]` entry cannot be read
            # fails the run that follows.
            with config.open("rb") as handle:
                prompts = tomllib.load(handle).get("prompts", [])
            for prompt in prompts:
                path = project / prompt["path"]
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("Answer from what you already know.\n")
            ok("init scaffolds a config and an example probe")

            check(run([str(binary), "snapshot"], project), "snapshot")
            lock = project / "agentchecksum.lock"
            if not lock.is_file():
                fail("snapshot did not write agentchecksum.lock")
            if f'"version": "{version}"' not in lock.read_text():
                fail(f"the lockfile snapshot wrote does not name generator version {version}")
            ok("snapshot discovers the prompt and writes a lockfile naming this version")

            diff = check(run([str(binary), "diff"], project), "diff")
            if "No dependency changes detected." not in diff:
                fail(f"diff of a just-snapshotted project reported something else:\n{diff}")
            ok("diff compares the lockfile against the project without drift")


def cmd_checksums(args: argparse.Namespace) -> None:
    name, version = crate()
    archives = sorted(path for path in args.dist.iterdir() if path.is_file() and path.name != "SHA256SUMS")
    if not archives:
        fail(f"no archives in {args.dist}")
    for path in archives:
        if not archive_pattern(name).match(path.name):
            fail(f"{path.name} is not a release archive of {name}; nothing else may be attached to a release")
    lines = [f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}" for path in archives]
    # `sha256sum` writes `<digest>  <name>`; keeping that exact shape is what lets the
    # lines be checked with `sha256sum -c` afterwards.
    (args.dist / "SHA256SUMS").write_text("".join(f"{line}\n" for line in lines))
    ok(f"{len(archives)} archives over {name} v{version}, sorted by filename")
    sys.stdout.write("".join(f"{line}\n" for line in lines))


# The release assets a Homebrew formula installs from. macOS only, because that is the
# platform Homebrew's formula in this tap is for; the Linux and Windows archives are
# reachable through the other channels.
HOMEBREW_TARGETS = {"arm": "aarch64-apple-darwin", "intel": "x86_64-apple-darwin"}

HOMEBREW_BASE = "https://github.com/f0rknturkoglu/agentchecksum/releases"


def fetch(url: str) -> str:
    """Read a release asset over HTTPS, failing loudly rather than silently."""
    result = subprocess.run(["curl", "-fsSL", url], capture_output=True, text=True)
    if result.returncode != 0:
        fail(f"cannot read {url}: {result.stderr.strip() or 'curl failed'}")
    return result.stdout


def cmd_homebrew(args: argparse.Namespace) -> None:
    name, version = crate()
    tag = args.tag or f"v{version}"

    # The formula pins a version and digests that must belong to one release. A tag that
    # disagrees with the manifest is exactly how a formula ends up pointing at 0.1.0's
    # hashes while claiming to be 0.1.1.
    if tag != f"v{version}":
        fail(f"tag {tag} does not match {name} {version} in Cargo.toml (expected v{version})")

    base = args.release_base.rstrip("/")
    sums_url = f"{base}/download/{tag}/SHA256SUMS"
    try:
        manifest = fetch(sums_url)
    except SystemExit:
        raise
    digests = {}
    for line in manifest.splitlines():
        parts = line.split()
        if len(parts) == 2:
            digests[parts[1]] = parts[0]

    # Both macOS assets are required. A formula that silently loses one of them installs a
    # broken product on half the machines that ask for it.
    assets = {}
    for flavour, target in HOMEBREW_TARGETS.items():
        asset = archive_of(name, version, target)
        digest = digests.get(asset)
        if digest is None:
            fail(f"{asset} is not listed in {sums_url}")
        if len(digest) != 64 or not re.fullmatch(r"[0-9a-f]{64}", digest):
            fail(f"{asset} has an unusable sha256 in {sums_url}: {digest!r}")
        assets[flavour] = (target, asset, digest)

    formula = render_homebrew_formula(name, version, tag, base, assets)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(formula)
        ok(f"wrote {args.output} for {name} {version} from {tag}")
    else:
        sys.stdout.write(formula)


def render_homebrew_formula(name, version, tag, base, assets) -> str:
    arm_target, arm_asset, arm_digest = assets["arm"]
    intel_target, intel_asset, intel_digest = assets["intel"]

    return f"""# This file is generated. Edit the generator, not the formula:
#
#   python3 scripts/release_artifacts.py homebrew --output Formula/{name}.rb
#
# which reads the version from Cargo.toml and the digests from the release's own
# SHA256SUMS, and refuses to run if either macOS archive is missing.
class Agentchecksum < Formula
  desc "Dependency fingerprint and behavioral regression gate for AI agents"
  homepage "https://github.com/f0rknturkoglu/agentchecksum"
  license any_of: ["MIT", "Apache-2.0"]

  # No `version`: both URLs carry `v{version}`, and Homebrew reads it from them —
  # `brew audit` reports it as redundant when it is written out. The generator already
  # refuses a tag that disagrees with Cargo.toml, so the version cannot drift.

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    if Hardware::CPU.arm?
      url "{base}/download/{tag}/{arm_asset}"
      sha256 "{arm_digest}"
    else
      url "{base}/download/{tag}/{intel_asset}"
      sha256 "{intel_digest}"
    end
  end

  def install
    # Each archive holds one directory named after the asset, with the binary and the
    # two license files in it.
    bin.install "{name}"
  end

  test do
    assert_match "agentchecksum #{{version}}", shell_output("#{{bin}}/{name} --version")
  end
end
"""


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dist", type=Path, default=ROOT / "dist", help="directory holding the archives (default: dist/)")
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    subparsers.add_parser("version", help="print the version in Cargo.toml")

    check_tag = subparsers.add_parser("check-tag", help="refuse a tag that disagrees with the crate version")
    check_tag.add_argument("--tag", required=True, help="the tag being released, e.g. v0.1.0")
    check_tag.add_argument("--event", required=True, help="the event that started the run; a dry run is not a release")

    package = subparsers.add_parser("package", help="build the archive for one target")
    package.add_argument("--target", required=True, help="the target triple the binary was built for")

    smoke = subparsers.add_parser("smoke", help="extract the archive and exercise the binary inside it")
    smoke.add_argument("--target", required=True, help="the target triple the archive was built for")
    smoke.add_argument("--project", action="store_true", help="also run init, snapshot and diff in a temporary project")

    subparsers.add_parser("checksums", help="write SHA256SUMS over the archives")

    homebrew = subparsers.add_parser("homebrew", help="render the Homebrew formula from a published release")
    homebrew.add_argument("--tag", help="the release tag to read (default: v<version>)")
    homebrew.add_argument("--release-base", default=HOMEBREW_BASE, help="where the release assets live")
    homebrew.add_argument("--output", type=Path, help="write the formula here instead of stdout")

    args = parser.parse_args(argv)
    commands = {
        "version": cmd_version,
        "check-tag": cmd_check_tag,
        "package": cmd_package,
        "smoke": cmd_smoke,
        "checksums": cmd_checksums,
        "homebrew": cmd_homebrew,
    }
    commands[args.subcommand](args)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
