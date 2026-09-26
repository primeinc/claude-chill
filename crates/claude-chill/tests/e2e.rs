//! End-to-end tests for claude-chill.
//!
//! On Unix: uses portable-pty (real PTY) to spawn the proxy
//! On Windows: uses std::process::Command with piped I/O since nested ConPTY
//! (portable-pty spawns a ConPTY, proxy creates another ConPTY) causes deadlocks.
//! The piped approach tests the ConPTY child output path but not interactive stdin.

use std::io::Write;
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

/// Spawn the proxy wrapping a shell command and capture output.
/// Returns (stdout, stderr, exit_code).
fn run_proxy_full(extra_args: &[&str], shell_cmd: &str) -> (String, String, Option<i32>) {
    let bin = binary_path();
    let mut args: Vec<&str> = vec!["-a", "0"];
    args.extend_from_slice(extra_args);
    args.push("--");

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
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code();
    (stdout, stderr, code)
}

/// Spawn the proxy wrapping `cmd.exe /c <shell_cmd>` and capture output.
/// Returns (stdout_output, exit_code).
fn run_proxy_with_command(shell_cmd: &str) -> (String, Option<i32>) {
    let (stdout, _, code) = run_proxy_full(&[], shell_cmd);
    (stdout, code)
}

/// Spawn the proxy wrapping the argv `child`, write `input` to the proxy's
/// stdin, then close it. Returns (stdout, exit_code).
fn run_proxy_with_stdin(child: &[&str], input: &[u8]) -> (String, Option<i32>) {
    let bin = binary_path();
    let mut args: Vec<&str> = vec!["-a", "0", "--"];
    args.extend_from_slice(child);

    let mut proc = Command::new(&bin)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to run {bin:?}: {e}"));
    {
        let mut stdin = proc.stdin.take().expect("stdin is piped");
        stdin.write_all(input).expect("write to proxy stdin");
    }
    let output = proc.wait_with_output().expect("wait for proxy");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code(),
    )
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
fn test_e2e_piped_stdin_reaches_child() {
    // Redirected stdin is forwarded to the child, and its EOF does not end
    // the proxy before the child's output is relayed.
    let (child, input): (&[&str], &[u8]) = if cfg!(windows) {
        (
            &["cmd.exe", "/c", "set /p X=& call echo GOT_%X%"],
            b"piped\r",
        )
    } else {
        (&["/bin/sh", "-c", "read X; echo GOT_$X"], b"piped\n")
    };
    let (stdout, code) = run_proxy_with_stdin(child, input);
    assert!(
        stdout.contains("GOT_piped"),
        "Child did not receive piped stdin.\nGot:\n{stdout}"
    );
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
    // The proxy renders the child's screen through its VT emulator (and on
    // Windows ConPTY renders it first), so the stream is screen updates, not
    // the child's bytes: check what a 24x80 terminal (the proxy's fallback
    // size) shows at the end. Earlier lines have scrolled off that screen.
    let mut screen = vt100::Parser::new(24, 80, 0);
    screen.process(stdout.as_bytes());
    let shown = screen.screen().contents();
    assert!(
        shown.contains("Line number 100"),
        "Missing last line on the final screen:\n{shown}\nStream:\n{stdout}"
    );
    assert!(
        shown.contains("Line number 99\n"),
        "Missing the line before it on the final screen:\n{shown}"
    );
    assert_eq!(code, Some(0));
}

#[test]
fn test_e2e_verbose_flag_produces_stderr() {
    // --verbose should enable logging to stderr in release builds
    let cmd = "echo VERBOSE_TEST";
    let (stdout, stderr, code) = run_proxy_full(&["--verbose"], cmd);
    assert!(
        stdout.contains("VERBOSE_TEST"),
        "Expected output in stdout.\nGot:\n{stdout}"
    );
    // Verbose mode should produce some debug output on stderr
    assert!(
        !stderr.is_empty(),
        "Expected verbose logging on stderr but got nothing"
    );
    assert_eq!(code, Some(0));
}

#[test]
fn test_e2e_special_characters() {
    // Test that special characters pass through correctly
    let cmd = if cfg!(windows) {
        "echo SPECIAL_123_ABC"
    } else {
        "echo 'SPECIAL_123_ABC'"
    };
    let (stdout, code) = run_proxy_with_command(cmd);
    assert!(
        stdout.contains("SPECIAL_123_ABC"),
        "Special characters not preserved.\nGot:\n{stdout}"
    );
    assert_eq!(code, Some(0));
}

#[test]
fn test_e2e_nonzero_exit_codes() {
    // Test various non-zero exit codes are forwarded correctly
    for expected_code in [1, 2, 127] {
        let cmd = if cfg!(windows) {
            format!("exit /b {expected_code}")
        } else {
            format!("exit {expected_code}")
        };
        let (_, code) = run_proxy_with_command(&cmd);
        assert_eq!(
            code,
            Some(expected_code),
            "Expected exit code {expected_code}, got {code:?}"
        );
    }
}

#[test]
fn test_e2e_help_flag() {
    // --help should work and exit 0
    let bin = binary_path();
    let output = Command::new(&bin)
        .args(["--help"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run {bin:?}: {e}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("claude-chill") || stdout.contains("PTY proxy"),
        "Help output should mention the tool name.\nGot:\n{stdout}"
    );
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn test_e2e_version_flag() {
    // --version should print version and exit 0
    let bin = binary_path();
    let output = Command::new(&bin)
        .args(["--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run {bin:?}: {e}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.1.5"),
        "Version output should contain version number.\nGot:\n{stdout}"
    );
    assert_eq!(output.status.code(), Some(0));
}
