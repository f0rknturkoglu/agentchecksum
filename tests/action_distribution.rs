// SPDX-License-Identifier: MIT OR Apache-2.0

//! The distribution paths of the GitHub Action, exercised through the script itself.
//!
//! `scripts/github-action.sh` decides between three ways of getting a binary: use the
//! release asset for this platform — which must be authenticated against the release's
//! own `SHA256SUMS` — build from source when there is no asset, or stop. Each of those
//! is a claim a user's CI depends on, and none of them is visible from a unit test of
//! the CLI, because the CLI is not what chooses.
//!
//! These tests run the real script against a local mirror and a stub `cargo`, so the
//! assertions are about the paths actually taken: which binary was executed, whether a
//! source build was attempted, and which exit code came out. The marker file the stubs
//! append to is what makes "the release binary ran, not a source build" checkable
//! rather than assumed.
//!
//! Unix only: the script is bash and the harness needs a POSIX shell. The Windows path
//! is exercised by simulating the runner's environment (`RUNNER_OS=Windows`), which is
//! exactly how the platform decision is made.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The version and tag these tests pretend have been released.
const VERSION: &str = "v0.1.0";

/// One line per stub invocation: which kind of binary ran, and where it came from.
struct Marker(PathBuf);

impl Marker {
    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.0)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Whether any binary ran at all.
    fn executed(&self) -> bool {
        !self.lines().is_empty()
    }

    /// The kind of binary that ran, when exactly one did.
    fn only_kind(&self) -> String {
        let lines = self.lines();
        assert_eq!(lines.len(), 1, "expected one invocation, got {lines:?}");
        lines[0].split_whitespace().next().unwrap().to_string()
    }
}

/// A workspace: a project to run in, a stub toolchain, and a marker file.
struct Harness {
    dir: tempfile::TempDir,
    marker: Marker,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/prompts")).unwrap();
        std::fs::write(
            dir.path().join("project/prompts/system.md"),
            "Be concise.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("project/agentchecksum.toml"),
            "version = 1\n[agent]\nname = \"action-test\"\n[model]\nprovider = \"openai-compatible\"\nid = \"m\"\nendpoint = \"http://127.0.0.1:9\"\n[[prompts]]\npath = \"prompts/system.md\"\n",
        )
        .unwrap();

        let marker = Marker(dir.path().join("marker"));
        std::fs::write(&marker.0, b"").unwrap();
        write_stub_cargo(dir.path());

        Self { dir, marker }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn marker(&self) -> &Marker {
        &self.marker
    }

    /// Run the action script the way `action.yml` does.
    fn run(&self, server: Option<&MockServer>, inputs: &[(&str, &str)]) -> std::process::Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/github-action.sh");
        let mut command = Command::new("bash");
        command
            .arg(script)
            .arg(self.project())
            .arg(self.dir.path())
            .env("PATH", self.toolchain_path())
            .env("AC_TEST_MARKER", &self.marker.0)
            .env("INPUT_COMMAND", "snapshot")
            .env("INPUT_ARGS", "")
            .env("INPUT_VERSION", VERSION)
            .env("INPUT_CONFIG", "agentchecksum.toml")
            .env("INPUT_LOCK", "agentchecksum.lock")
            .env_remove("GITHUB_OUTPUT");

        for (key, value) in inputs {
            command.env(key, value);
        }
        if let Some(server) = server {
            command.env("AC_RELEASE_BASE", format!("{}/releases", server.uri()));
        }

        command.output().expect("the action script runs")
    }

    fn toolchain_path(&self) -> std::ffi::OsString {
        let mut paths = vec![self.dir.path().join("bin")];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(paths).unwrap()
    }
}

/// A `cargo` that installs a stub binary where the real one would, so the fallback can
/// be tested without compiling anything.
fn write_stub_cargo(root: &Path) {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        r#"#!/bin/sh
# Only `cargo install --root <dir> --path <dir>` is expected. The installed binary is
# named the way the host would name it, which is the whole point of the test.
root=""
while [ $# -gt 0 ]; do
    case "$1" in
        --root) root="$2"; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$root" ] || { echo "stub cargo: no --root" >&2; exit 1; }
name="agentchecksum"
[ "${RUNNER_OS:-}" = "Windows" ] && name="agentchecksum.exe"
mkdir -p "$root/bin"
cat > "$root/bin/$name" <<STUB
#!/bin/sh
printf 'source %s\n' "\$0" >> "\$AC_TEST_MARKER"
exit 0
STUB
chmod +x "$root/bin/$name"
"#,
    )
    .unwrap();
    make_executable(&cargo);
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

/// A binary that records that it ran and exits with `AC_TEST_EXIT`.
fn cli_stub(kind: &str) -> String {
    format!(
        r#"#!/bin/sh
printf '{kind} %s\n' "$0" >> "$AC_TEST_MARKER"
exit "${{AC_TEST_EXIT:-0}}"
"#
    )
}

/// Build the archive a release would publish: a stem directory holding the binary.
fn archive(dir: &Path, target: &str, binary: &str, contents: &str) -> Vec<u8> {
    let stem = format!("agentchecksum-{VERSION}-{target}");
    let staging = dir.join("staging").join(&stem);
    std::fs::create_dir_all(&staging).unwrap();
    let path = staging.join(binary);
    std::fs::write(&path, contents).unwrap();
    make_executable(&path);
    std::fs::write(staging.join("README.md"), "# AgentChecksum\n").unwrap();

    let archive_path = dir.join(format!("{stem}.tar.gz"));
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&archive_path)
        .arg("-C")
        .arg(dir.join("staging"))
        .arg(&stem)
        .status()
        .expect("tar runs");
    assert!(status.success(), "tar failed");
    std::fs::read(&archive_path).unwrap()
}

/// The digest a release's `SHA256SUMS` would carry, taken through the crate's own
/// digest type so the tests and the tooling agree on the encoding.
fn sha256_hex(bytes: &[u8]) -> String {
    agentchecksum::manifest::Digest::sha256(bytes)
        .hex()
        .to_string()
}

/// Mount a release asset and a `SHA256SUMS` for it.
async fn mount_release(server: &MockServer, asset: &str, bytes: &[u8], sums: Option<String>) {
    Mock::given(method("GET"))
        .and(path(format!("/releases/download/{VERSION}/{asset}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
        .mount(server)
        .await;

    if let Some(sums) = sums {
        Mock::given(method("GET"))
            .and(path(format!("/releases/download/{VERSION}/SHA256SUMS")))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums))
            .mount(server)
            .await;
    }
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// A verified release asset is used, and its exit code is the step's.
#[tokio::test]
async fn a_verified_release_binary_is_used_and_its_exit_code_is_propagated() {
    let harness = Harness::new();
    let server = MockServer::start().await;
    let asset = "agentchecksum-v0.1.0-x86_64-unknown-linux-gnu.tar.gz";
    let bytes = archive(
        harness.dir.path(),
        "x86_64-unknown-linux-gnu",
        "agentchecksum",
        &cli_stub("release"),
    );
    mount_release(
        &server,
        asset,
        &bytes,
        Some(format!("{}  {asset}\n", sha256_hex(&bytes))),
    )
    .await;

    let output = harness.run(
        Some(&server),
        &[("RUNNER_OS", "Linux"), ("RUNNER_ARCH", "X64")],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(harness.marker().only_kind(), "release");
    assert!(
        !stderr(&output).contains("building from source"),
        "{}",
        stderr(&output)
    );
}

/// The CLI's own exit code is what the caller sees — not a code the action invents.
#[tokio::test]
async fn a_failing_cli_result_is_reported_unchanged() {
    let harness = Harness::new();
    let server = MockServer::start().await;
    let asset = "agentchecksum-v0.1.0-x86_64-unknown-linux-gnu.tar.gz";
    let bytes = archive(
        harness.dir.path(),
        "x86_64-unknown-linux-gnu",
        "agentchecksum",
        &cli_stub("release"),
    );
    mount_release(
        &server,
        asset,
        &bytes,
        Some(format!("{}  {asset}\n", sha256_hex(&bytes))),
    )
    .await;

    let output = harness.run(
        Some(&server),
        &[
            ("RUNNER_OS", "Linux"),
            ("RUNNER_ARCH", "X64"),
            ("AC_TEST_EXIT", "1"),
        ],
    );

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

/// A release asset that cannot be authenticated is not run, and is not replaced by a
/// source build either: the asset was selected, so the failure is the answer.
#[tokio::test]
async fn a_release_asset_without_a_checksum_manifest_is_refused() {
    let harness = Harness::new();
    let server = MockServer::start().await;
    let asset = "agentchecksum-v0.1.0-x86_64-unknown-linux-gnu.tar.gz";
    let bytes = archive(
        harness.dir.path(),
        "x86_64-unknown-linux-gnu",
        "agentchecksum",
        &cli_stub("release"),
    );
    mount_release(&server, asset, &bytes, None).await;

    let output = harness.run(
        Some(&server),
        &[("RUNNER_OS", "Linux"), ("RUNNER_ARCH", "X64")],
    );

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("SHA256SUMS"), "{diagnostic}");
    assert!(diagnostic.contains("cannot be verified"), "{diagnostic}");
    assert!(
        !harness.marker().executed(),
        "nothing may run when the archive cannot be authenticated: {:?}",
        harness.marker().lines()
    );
    assert!(
        !diagnostic.contains("building from source"),
        "an unverifiable asset is not a reason to build from source: {diagnostic}"
    );
}

/// A digest that disagrees with the manifest is a refusal, not a warning.
#[tokio::test]
async fn a_release_asset_whose_digest_disagrees_is_refused() {
    let harness = Harness::new();
    let server = MockServer::start().await;
    let asset = "agentchecksum-v0.1.0-x86_64-unknown-linux-gnu.tar.gz";
    let bytes = archive(
        harness.dir.path(),
        "x86_64-unknown-linux-gnu",
        "agentchecksum",
        &cli_stub("release"),
    );
    mount_release(
        &server,
        asset,
        &bytes,
        Some(format!("{}  {asset}\n", "0".repeat(64))),
    )
    .await;

    let output = harness.run(
        Some(&server),
        &[("RUNNER_OS", "Linux"), ("RUNNER_ARCH", "X64")],
    );

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("does not match"),
        "{}",
        stderr(&output)
    );
    assert!(!harness.marker().executed());
}

/// A manifest that lists other assets and not this one is equally unusable.
#[tokio::test]
async fn an_asset_missing_from_the_manifest_is_refused() {
    let harness = Harness::new();
    let server = MockServer::start().await;
    let asset = "agentchecksum-v0.1.0-x86_64-unknown-linux-gnu.tar.gz";
    let bytes = archive(
        harness.dir.path(),
        "x86_64-unknown-linux-gnu",
        "agentchecksum",
        &cli_stub("release"),
    );
    mount_release(
        &server,
        asset,
        &bytes,
        Some(format!(
            "{}  agentchecksum-v0.1.0-aarch64-apple-darwin.tar.gz\n",
            sha256_hex(&bytes)
        )),
    )
    .await;

    let output = harness.run(
        Some(&server),
        &[("RUNNER_OS", "Linux"), ("RUNNER_ARCH", "X64")],
    );

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no entry for"),
        "{}",
        stderr(&output)
    );
    assert!(!harness.marker().executed());
}

/// With no asset published for this platform, the source build is still the answer —
/// that is what makes the action usable before the first release.
#[tokio::test]
async fn a_missing_release_asset_falls_back_to_a_source_build() {
    let harness = Harness::new();
    let server = MockServer::start().await;

    let output = harness.run(
        Some(&server),
        &[("RUNNER_OS", "Linux"), ("RUNNER_ARCH", "X64")],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(harness.marker().only_kind(), "source");
    assert!(
        stderr(&output).contains("building from source"),
        "{}",
        stderr(&output)
    );
}

/// The Windows fallback has to invoke the executable Cargo actually installs there.
/// A `.exe`-named script with a shebang runs on Unix, so the assertion is about the
/// name the action chose, not about the host's loader.
#[tokio::test]
async fn the_windows_source_fallback_runs_the_dot_exe() {
    let harness = Harness::new();
    let server = MockServer::start().await;

    let output = harness.run(
        Some(&server),
        &[("RUNNER_OS", "Windows"), ("RUNNER_ARCH", "X64")],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lines = harness.marker().lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("source "), "{lines:?}");
    assert!(
        lines[0].ends_with("installed/bin/agentchecksum.exe"),
        "the Windows fallback must run the .exe: {lines:?}"
    );
}

/// An unsupported platform is a distribution failure, not a gate failure.
#[tokio::test]
async fn an_unsupported_platform_exits_three() {
    let harness = Harness::new();
    let server = MockServer::start().await;

    let output = harness.run(
        Some(&server),
        &[("RUNNER_OS", "Plan9"), ("RUNNER_ARCH", "X64")],
    );

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(!harness.marker().executed());
}
