//! End-to-end tests for claude-chill.
//!
//! On Unix: uses portable-pty (real PTY) to spawn the proxy
//! On Windows: uses std::process::Command with piped I/O since nested ConPTY
//! (portable-pty spawns a ConPTY, proxy creates another ConPTY) causes deadlocks.
//! The piped approach tests the ConPTY child output path but not interactive stdin.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Get the path to the claude-chill binary built by cargo.
fn binary_path() -> PathBuf {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent")
        .parent()
        .expect("grandparent")
        .to_path_buf();
    if cfg!(windows) {
        path.push("claude-chill.exe");
    } else {
        path.push("claude-chill");
    }
    assert!(
        path.exists(),
        "Binary not found at {path:?}. Run `cargo build` first."
    );
    path
}

/// Spawn the proxy wrapping `cmd.exe /c <shell_cmd>` and capture output.
/// Returns (stdout_output, exit_code).
fn run_proxy_with_command(shell_cmd: &str) -> (String, Option<i32>) {
    let bin = binary_path();
    let mut args = vec!["-a", "0", "--"];

    if cfg!(windows) {
        args.extend_from_slice(&["cmd.exe", "/c", shell_cmd]);
    } else {
        args.extend_from_slice(&["/bin/sh", "-c", shell_cmd]);
    }

    let output = Command::new(&bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run {bin:?}: {e}"));

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let code = output.status.code();
    (stdout, code)
}

#[test]
fn test_e2e_echo_output() {
    let (stdout, code) = run_proxy_with_command("echo HELLO_E2E_TEST");
    assert!(
        stdout.contains("HELLO_E2E_TEST"),
        "Expected 'HELLO_E2E_TEST' in proxy output.\nGot stdout:\n{stdout}"
    );
    assert_eq!(code, Some(0), "Expected exit code 0, got {code:?}");
}

#[test]
fn test_e2e_exit_code_forwarded() {
    if cfg!(windows) {
        let (_, code) = run_proxy_with_command("exit /b 42");
        assert_eq!(code, Some(42), "Expected exit code 42, got {code:?}");
    } else {
        let (_, code) = run_proxy_with_command("exit 42");
        assert_eq!(code, Some(42), "Expected exit code 42, got {code:?}");
    }
}

#[test]
fn test_e2e_multiple_lines() {
    let cmd = if cfg!(windows) {
        "echo LINE_1 && echo LINE_2 && echo LINE_3"
    } else {
        "echo LINE_1; echo LINE_2; echo LINE_3"
    };
    let (stdout, code) = run_proxy_with_command(cmd);
    assert!(stdout.contains("LINE_1"), "Missing LINE_1 in:\n{stdout}");
    assert!(stdout.contains("LINE_2"), "Missing LINE_2 in:\n{stdout}");
    assert!(stdout.contains("LINE_3"), "Missing LINE_3 in:\n{stdout}");
    assert_eq!(code, Some(0));
}

#[test]
fn test_e2e_empty_output() {
    // A command that produces no visible output
    let cmd = if cfg!(windows) {
        "rem no output"
    } else {
        "true"
    };
    let (_, code) = run_proxy_with_command(cmd);
    assert_eq!(code, Some(0), "Expected exit code 0, got {code:?}");
}

#[test]
fn test_e2e_large_output() {
    // Generate enough output to exercise buffering
    let cmd = if cfg!(windows) {
        "for /L %i in (1,1,100) do @echo Line number %i"
    } else {
        "seq 1 100 | while read i; do echo Line number $i; done"
    };
    let (stdout, code) = run_proxy_with_command(cmd);
    assert!(
        stdout.contains("Line number 1"),
        "Missing first line in:\n{stdout}"
    );
    assert!(
        stdout.contains("Line number 100"),
        "Missing last line in:\n{stdout}"
    );
    assert_eq!(code, Some(0));
}
