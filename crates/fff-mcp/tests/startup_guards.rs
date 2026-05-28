//! Axis-D integration tests: spawn the `fff-mcp` binary and assert the
//! startup-guard CLI flags behave correctly. These cover the wire-up
//! between the new `Args` fields, `resolve_base_path`, and the JSON
//! stderr error path.
//!
//! Cargo auto-populates `CARGO_BIN_EXE_fff-mcp` for integration tests of
//! the `fff-mcp` package, so the binary is always built before these
//! tests run.

use std::process::Command;

use tempfile::TempDir;

fn fff_mcp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fff-mcp"))
}

/// `--validate-root --require-git-root <non_git_dir>` must exit non-zero
/// and emit a `NO_GIT_ROOT` JSON line on stderr — without ever starting
/// the MCP server.
#[test]
fn require_git_root_rejects_non_git_dir() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let output = fff_mcp()
        .arg("--validate-root")
        .arg("--require-git-root")
        .arg(dir.to_str().unwrap())
        .output()
        .expect("failed to spawn fff-mcp");

    assert!(
        !output.status.success(),
        "expected non-zero exit, got status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("\"code\":\"NO_GIT_ROOT\""),
        "expected NO_GIT_ROOT in stderr; got:\n{stderr}"
    );
    assert!(
        stderr.contains("\"event\":\"fff_startup_error\""),
        "expected event=fff_startup_error in stderr; got:\n{stderr}"
    );
}

/// `--validate-root --root-must-be-under <prefix>` rejects a base path
/// outside the allowlist with `ROOT_NOT_ALLOWED`.
#[test]
fn root_must_be_under_rejects_outside_path() {
    let tmp = TempDir::new().unwrap();
    let outside = tmp.path();
    // Pick an allowlist prefix that is guaranteed not to be a parent of
    // `outside` — `/nonexistent-prefix-axis-d-test/` is safe.
    let prefix = "/nonexistent-prefix-axis-d-test/";

    let output = fff_mcp()
        .arg("--validate-root")
        .arg("--root-must-be-under")
        .arg(prefix)
        .arg(outside.to_str().unwrap())
        .output()
        .expect("failed to spawn fff-mcp");

    assert!(
        !output.status.success(),
        "expected non-zero exit; stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("\"code\":\"ROOT_NOT_ALLOWED\""),
        "expected ROOT_NOT_ALLOWED in stderr; got:\n{stderr}"
    );
}

/// `--validate-root` against a path the guards accept (no `--require-git-root`
/// and no allowlist) must exit 0 without writing a JSON error.
#[test]
fn validate_root_succeeds_on_plain_directory() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let output = fff_mcp()
        .arg("--validate-root")
        .arg(dir.to_str().unwrap())
        .output()
        .expect("failed to spawn fff-mcp");

    assert!(
        output.status.success(),
        "expected zero exit, got status={:?}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("OK"),
        "expected OK in stdout; got: {stdout}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("fff_startup_error"),
        "did not expect startup error on success; got: {stderr}"
    );
}

/// `--healthcheck` resolves an explicit, PID-suffixed log path by default.
/// Two concurrent fff-mcp processes must compute distinct log paths so they
/// do not contend on the same file.
#[test]
fn default_log_path_is_per_pid() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".cache")).unwrap();

    let run = |label: &str| -> String {
        let output = fff_mcp()
            .arg("--healthcheck")
            // No --base-path: healthcheck only validates configured paths.
            .env("HOME", home.path())
            .env_remove("USERPROFILE") // ignore Windows fallback
            .output()
            .unwrap_or_else(|e| panic!("{label}: failed to spawn fff-mcp: {e}"));

        assert!(
            output.status.code().is_some(),
            "{label}: process did not exit normally",
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let stdout1 = run("run 1");
    let stdout2 = run("run 2");

    // Both runs must print a `fff_mcp-<pid>.log` path on the Log file line.
    let extract_log_path = |out: &str| -> Option<String> {
        for line in out.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("[+] Log file:") {
                return Some(rest.trim().to_string());
            }
            if let Some(rest) = trimmed.strip_prefix("[x] Log file:") {
                return Some(rest.trim().to_string());
            }
        }
        None
    };

    let path1 = extract_log_path(&stdout1)
        .unwrap_or_else(|| panic!("no log path in run 1 stdout:\n{stdout1}"));
    let path2 = extract_log_path(&stdout2)
        .unwrap_or_else(|| panic!("no log path in run 2 stdout:\n{stdout2}"));

    assert!(
        path1.contains("fff_mcp-") && path1.ends_with(".log"),
        "run 1 log path missing PID suffix: {path1}"
    );
    assert!(
        path2.contains("fff_mcp-") && path2.ends_with(".log"),
        "run 2 log path missing PID suffix: {path2}"
    );
    assert_ne!(
        path1, path2,
        "two fff-mcp processes computed the same default log path: {path1}"
    );
}
