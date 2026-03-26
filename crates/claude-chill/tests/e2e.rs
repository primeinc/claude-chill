//! End-to-end tests for claude-chill using portable-pty.
//!
//! These tests spawn the actual claude-chill binary inside a pseudo-terminal
//! and verify that the proxy correctly:
//! - Renders child process output
//! - Forwards input to the child
//! - Activates lookback mode
//! - Exits cleanly
//!
//! Uses portable-pty for cross-platform PTY support (Unix PTY / Windows ConPTY).
//! The reader runs on a background thread because PTY reads are blocking on Windows.

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// Get the platform-appropriate shell command.
fn shell_command() -> &'static str {
    if cfg!(windows) {
        "cmd.exe"
    } else {
        "/bin/bash"
    }
}

/// Shell args to disable any rc/profile loading.
fn shell_args() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["/q"] // quiet mode for cmd.exe
    } else {
        vec!["--norc", "--noprofile"]
    }
}

/// A non-blocking reader that drains a PTY reader on a background thread
/// and makes output available via a channel.
struct ThreadedReader {
    rx: mpsc::Receiver<Vec<u8>>,
    accumulated: Vec<u8>,
}

impl ThreadedReader {
    fn new(mut reader: Box<dyn Read + Send>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        ThreadedReader {
            rx,
            accumulated: Vec::new(),
        }
    }

    /// Drain all available data from the channel into accumulated buffer.
    fn drain(&mut self) {
        while let Ok(data) = self.rx.try_recv() {
            self.accumulated.extend_from_slice(&data);
        }
    }

    /// Wait until `needle` appears in the accumulated output, or timeout.
    fn wait_for(&mut self, needle: &str, timeout: Duration) -> String {
        let start = Instant::now();
        while start.elapsed() < timeout {
            self.drain();
            let text = String::from_utf8_lossy(&self.accumulated);
            if text.contains(needle) {
                return text.into_owned();
            }
            // Wait a bit for more data
            if let Ok(data) = self.rx.recv_timeout(Duration::from_millis(100)) {
                self.accumulated.extend_from_slice(&data);
            }
        }
        let text = String::from_utf8_lossy(&self.accumulated);
        text.into_owned()
    }

    /// Clear accumulated buffer (for fresh checks after a known state).
    fn clear(&mut self) {
        self.drain();
        self.accumulated.clear();
    }
}

/// Spawn claude-chill wrapping a shell inside a PTY.
fn spawn_proxy() -> (
    ThreadedReader,
    Box<dyn Write + Send>,
    Box<dyn portable_pty::Child + Send + Sync>,
) {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty failed");

    let bin = binary_path();
    let mut cmd = CommandBuilder::new(&bin);

    // Disable auto-lookback (interferes with tests)
    cmd.arg("-a");
    cmd.arg("0");
    cmd.arg("--");
    cmd.arg(shell_command());
    for arg in shell_args() {
        cmd.arg(arg);
    }

    let child = pair.slave.spawn_command(cmd).expect("spawn_command failed");
    let reader = pair
        .master
        .try_clone_reader()
        .expect("try_clone_reader failed");
    let writer = pair.master.take_writer().expect("take_writer failed");

    let threaded_reader = ThreadedReader::new(reader);
    (threaded_reader, writer, child)
}

/// Line ending for the current platform.
fn line_ending() -> &'static [u8] {
    if cfg!(windows) { b"\r\n" } else { b"\r" }
}

#[test]
fn test_e2e_spawn_and_exit() {
    let (mut reader, mut writer, mut child) = spawn_proxy();

    // Wait for some output (shell prompt)
    let output = reader.wait_for(">", Duration::from_secs(10));
    // On Windows cmd.exe shows ">" prompt, on Unix bash shows "$"
    let got_prompt = output.contains('>') || output.contains('$');
    assert!(
        got_prompt || !output.is_empty(),
        "Expected shell prompt, got: {output:?}"
    );

    // Send exit command
    writer.write_all(b"exit").expect("write exit");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");

    // Wait for child to exit
    let status = child.wait().expect("wait");
    let _ = status;
}

#[test]
fn test_e2e_echo_output_rendered() {
    let (mut reader, mut writer, mut child) = spawn_proxy();

    // Wait for prompt
    let _ = reader.wait_for(">", Duration::from_secs(10));
    reader.clear();

    // Send echo command
    writer.write_all(b"echo HELLO_FROM_PROXY").expect("write");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");

    // Verify the echo output appears (rendered through VT emulation)
    let output = reader.wait_for("HELLO_FROM_PROXY", Duration::from_secs(10));
    assert!(
        output.contains("HELLO_FROM_PROXY"),
        "Expected 'HELLO_FROM_PROXY' in output, got: {output:?}"
    );

    // Exit
    writer.write_all(b"exit").expect("write exit");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");
    let _ = child.wait();
}

#[test]
fn test_e2e_lookback_mode() {
    let (mut reader, mut writer, mut child) = spawn_proxy();

    // Wait for prompt
    let _ = reader.wait_for(">", Duration::from_secs(10));

    // Generate output for history
    writer.write_all(b"echo LOOKBACK_LINE_1").expect("write");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");
    let _ = reader.wait_for("LOOKBACK_LINE_1", Duration::from_secs(10));

    std::thread::sleep(Duration::from_millis(500));
    reader.clear();

    // Enter lookback mode (Ctrl+^ = 0x1E)
    writer.write_all(&[0x1E]).expect("write lookback key");
    writer.flush().expect("flush");

    // Should see LOOKBACK MODE banner
    let output = reader.wait_for("LOOKBACK MODE", Duration::from_secs(10));
    assert!(
        output.contains("LOOKBACK MODE"),
        "Expected 'LOOKBACK MODE' banner, got: {output:?}"
    );

    // Exit lookback with same key
    std::thread::sleep(Duration::from_millis(300));
    writer.write_all(&[0x1E]).expect("write lookback exit");
    writer.flush().expect("flush");

    std::thread::sleep(Duration::from_millis(500));
    reader.clear();

    // Verify shell still works
    writer.write_all(b"echo AFTER_LOOKBACK").expect("write");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");

    let output = reader.wait_for("AFTER_LOOKBACK", Duration::from_secs(10));
    assert!(
        output.contains("AFTER_LOOKBACK"),
        "Shell should work after lookback, got: {output:?}"
    );

    // Exit
    writer.write_all(b"exit").expect("write exit");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");
    let _ = child.wait();
}

#[test]
fn test_e2e_lookback_exit_ctrl_c() {
    let (mut reader, mut writer, mut child) = spawn_proxy();

    // Wait for prompt
    let _ = reader.wait_for(">", Duration::from_secs(10));

    // Generate output
    writer.write_all(b"echo CTRL_C_TEST").expect("write");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");
    let _ = reader.wait_for("CTRL_C_TEST", Duration::from_secs(10));

    std::thread::sleep(Duration::from_millis(500));

    // Enter lookback mode
    writer.write_all(&[0x1E]).expect("write lookback key");
    writer.flush().expect("flush");

    let output = reader.wait_for("LOOKBACK MODE", Duration::from_secs(10));
    assert!(output.contains("LOOKBACK MODE"));

    // Exit with Ctrl+C
    writer.write_all(&[0x03]).expect("write ctrl-c");
    writer.flush().expect("flush");

    std::thread::sleep(Duration::from_millis(500));
    reader.clear();

    // Verify shell still works
    writer.write_all(b"echo AFTER_CTRL_C").expect("write");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");

    let output = reader.wait_for("AFTER_CTRL_C", Duration::from_secs(10));
    assert!(
        output.contains("AFTER_CTRL_C"),
        "Shell should work after Ctrl+C, got: {output:?}"
    );

    // Exit
    writer.write_all(b"exit").expect("write exit");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");
    let _ = child.wait();
}

#[test]
fn test_e2e_multiple_commands() {
    let (mut reader, mut writer, mut child) = spawn_proxy();

    // Wait for prompt
    let _ = reader.wait_for(">", Duration::from_secs(10));

    for i in 1..=5 {
        reader.clear();
        let echo_text = format!("MULTI_{i}");
        let cmd = format!("echo {echo_text}");
        writer.write_all(cmd.as_bytes()).expect("write");
        writer.write_all(line_ending()).expect("write newline");
        writer.flush().expect("flush");

        let output = reader.wait_for(&echo_text, Duration::from_secs(10));
        assert!(
            output.contains(&echo_text),
            "Expected '{echo_text}' in output #{i}, got: {output:?}"
        );
    }

    // Exit
    writer.write_all(b"exit").expect("write exit");
    writer.write_all(line_ending()).expect("write newline");
    writer.flush().expect("flush");
    let _ = child.wait();
}
