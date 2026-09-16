#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# The GitHub Action's body, as a script rather than inline YAML.
#
# A script can be run — and is run, by `action.yml`'s own step and by this repository's
# tests — while inline shell in a workflow can only be read. The action is thin on
# purpose: it locates a binary, runs one AgentChecksum command in the caller's project,
# and lets the CLI's exit code decide the step's outcome. It never reinterprets a
# verdict, never swallows an exit code, and never invents arguments.
#
# Usage: github-action.sh <working-directory> <action-path>
#
# Inputs arrive as INPUT_* environment variables, exactly as GitHub passes them.
# AC_RELEASE_BASE overrides where release assets are fetched from, which is what makes
# the download path testable without publishing anything and what a mirror needs.

set -euo pipefail

working_directory="${1:?the working directory is required}"
action_path="${2:?the action path is required}"

command_name="${INPUT_COMMAND:-check}"
extra_args="${INPUT_ARGS:-}"
version="${INPUT_VERSION:-latest}"
config="${INPUT_CONFIG:-agentchecksum.toml}"
lock="${INPUT_LOCK:-agentchecksum.lock}"

repository="f0rknturkoglu/agentchecksum"
release_base="${AC_RELEASE_BASE:-https://github.com/$repository/releases}"

log() { printf '%s\n' "$*" >&2; }

# ---------------------------------------------------------------------------
# Which asset this runner needs
# ---------------------------------------------------------------------------

case "${RUNNER_OS:-$(uname -s)}" in
    Linux) platform="unknown-linux-gnu"; archive="tar.gz"; executable="agentchecksum" ;;
    macOS) platform="apple-darwin"; archive="tar.gz"; executable="agentchecksum" ;;
    # Cargo names an installed binary after the host, so a Windows build — from a
    # release archive or from source — is always `agentchecksum.exe`.
    Windows) platform="pc-windows-msvc"; archive="zip"; executable="agentchecksum.exe" ;;
    *)
        log "::error::AgentChecksum has no release for this operating system"
        exit 3
        ;;
esac

case "${RUNNER_ARCH:-$(uname -m)}" in
    X64 | x64 | amd64 | x86_64) machine="x86_64" ;;
    ARM64 | arm64 | aarch64) machine="aarch64" ;;
    *)
        log "::error::AgentChecksum has no release for this architecture"
        exit 3
        ;;
esac

target="$machine-$platform"

# ---------------------------------------------------------------------------
# Find a binary: a published release when there is one, otherwise the source this
# action ships. Building from source is slow, so it is always announced.
# ---------------------------------------------------------------------------

work="$(mktemp -d "${TMPDIR:-/tmp}/agentchecksum-action.XXXXXX")"
trap 'rm -rf "$work"' EXIT

binary=""
tag=""
if [ "$version" = "latest" ]; then
    # `releases/latest/download` needs the asset's name, which carries the version, so
    # the tag has to be resolved first. An unauthenticated API call is enough for a
    # public repository; a failure here simply falls through to the source build.
    tag="$(curl -fsSL "https://api.github.com/repos/$repository/releases/latest" 2>/dev/null \
        | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tag_name", ""))' 2>/dev/null || true)"
else
    tag="$version"
fi

if [ -n "$tag" ]; then
    asset="agentchecksum-$tag-$target.$archive"
    if [ "$version" = "latest" ]; then
        # GitHub redirects `releases/latest/download/<name>` to the newest release.
        base="$release_base/latest/download"
    else
        base="$release_base/download/$tag"
    fi

    # A missing archive means there is no release for this platform yet, which is the
    # one case where building from source is the right answer. Everything after a
    # successful download is fatal instead: the asset was chosen, so it has to be
    # usable, and "usable" starts with "authenticated".
    if curl -fsSL -o "$work/$asset" "$base/$asset" 2>/dev/null; then
        if ! curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS" 2>/dev/null; then
            log "::error::$asset was downloaded but the release's SHA256SUMS could not be read, so the archive cannot be verified. AgentChecksum does not run an unverified binary."
            exit 3
        fi

        # `awk` rather than `grep | awk`: grep exits 1 when nothing matches, and `set -e`
        # would turn that into a silent exit 1 with no diagnostic — the one shape a
        # distribution failure must never take.
        expected="$(awk -v name="$asset" '$2 == name { print $1 }' "$work/SHA256SUMS")"
        if [ -z "$expected" ]; then
            log "::error::the release's SHA256SUMS has no entry for $asset, so the archive cannot be authenticated"
            exit 3
        fi

        actual="$(cd "$work" && { sha256sum "$asset" 2>/dev/null || shasum -a 256 "$asset"; } | awk '{print $1}')"
        if [ "$expected" != "$actual" ]; then
            log "::error::$asset does not match the checksum the release publishes"
            exit 3
        fi

        case "$archive" in
            tar.gz) tar -xzf "$work/$asset" -C "$work" ;;
            zip) unzip -oq "$work/$asset" -d "$work" ;;
        esac

        found="$(find "$work" -type f -name "$executable" | head -1)"
        if [ -z "$found" ]; then
            log "::error::$asset was verified but holds no $executable; the release asset is malformed"
            exit 3
        fi
        binary="$found"
    fi
fi

if [ -z "$binary" ]; then
    log "no published release asset for $target ($version); building from source instead"
    if ! command -v cargo > /dev/null 2>&1; then
        log "::error::no AgentChecksum release for $target and no cargo to build one from source"
        exit 3
    fi
    cargo install --quiet --locked --path "$action_path" --root "$work/installed"
    binary="$work/installed/bin/$executable"
    if [ ! -f "$binary" ]; then
        log "::error::cargo install finished without installing $binary"
        exit 3
    fi
fi

# ---------------------------------------------------------------------------
# Run the command in the caller's project
# ---------------------------------------------------------------------------

cd "$working_directory"

set +e
if [[ " $extra_args " == *" --format json "* ]] || [[ " $extra_args " == *" --format=json "* ]]; then
    # A JSON report is captured so its `status` can be published as an output; it is
    # still printed, to the stream it belongs on.
    "$binary" --config "$config" --lock "$lock" $command_name $extra_args \
        > "$work/stdout" 2> "$work/stderr"
    code=$?
    cat "$work/stdout"
    cat "$work/stderr" >&2
    status="$(python3 -c 'import json,sys
try:
    print(json.load(open(sys.argv[1])).get("status", ""))
except Exception:
    print("")' "$work/stdout" 2>/dev/null || true)"
else
    # Streamed, so a long capture is visible while it runs.
    "$binary" --config "$config" --lock "$lock" $command_name $extra_args
    code=$?
    status=""
fi
set -e

if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
        echo "exit-code=$code"
        [ -n "$status" ] && echo "status=$status"
    } >> "$GITHUB_OUTPUT"
fi

case "$code" in
    0) log "agentchecksum $command_name: exit 0 (completed)" ;;
    1) log "::error::agentchecksum $command_name: exit 1 (the gate failed — a metric policy failed, or a change was explicitly gated)" ;;
    2) log "::error::agentchecksum $command_name: exit 2 (usage error — this workflow passed a flag combination the CLI rejects)" ;;
    *) log "::error::agentchecksum $command_name: exit $code (the check could not be evaluated; this is not a behavioral verdict)" ;;
esac

exit "$code"
