//! Windows terminal setup utilities: console mode, Ctrl handler, I/O helpers.

use anyhow::{Context, Result};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::{BOOL, FALSE, HANDLE, TRUE};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, CTRL_BREAK_EVENT, CTRL_C_EVENT, ENABLE_ECHO_INPUT,
    ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, GetConsoleMode,
    GetConsoleScreenBufferInfo, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler, SetConsoleMode,
};

/// Set by the resize polling when the terminal is resized.
pub static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);
/// Set by the Ctrl handler when Ctrl+C is pressed.
pub static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);
/// Set by the Ctrl handler when Ctrl+Break is pressed.
pub static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Console modes saved by `setup_raw_mode`, restored on drop: stdin's, and
/// stdout's when it is a console (VT processing is turned on there). Dropping
/// it on any early return from `Proxy::spawn` restores the user's console.
pub struct ConsoleMode {
    stdin_handle: HANDLE,
    original_mode: u32,
    stdout_handle: HANDLE,
    original_out_mode: Option<u32>,
}

impl Drop for ConsoleMode {
    fn drop(&mut self) {
        // SAFETY: both handles came from GetStdHandle in setup_raw_mode, and
        // the modes are the ones read there before any change.
        unsafe { SetConsoleMode(self.stdin_handle, self.original_mode) };
        if let Some(out_mode) = self.original_out_mode {
            // SAFETY: as above.
            unsafe { SetConsoleMode(self.stdout_handle, out_mode) };
        }
    }
}

/// Console Ctrl handler callback.
///
/// # Safety
/// Called by the Windows console subsystem. Only performs atomic stores,
/// which are safe from any thread context.
unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        CTRL_C_EVENT => {
            SIGINT_RECEIVED.store(true, Ordering::SeqCst);
            TRUE
        }
        CTRL_BREAK_EVENT => {
            SIGTERM_RECEIVED.store(true, Ordering::SeqCst);
            TRUE
        }
        _ => FALSE,
    }
}

/// Query the terminal dimensions. Falls back to 24x80 on failure.
pub fn get_terminal_size() -> Result<TerminalSize> {
    // SAFETY: GetStdHandle returns a pseudo-handle for stdout; no cleanup needed.
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    // SAFETY: CONSOLE_SCREEN_BUFFER_INFO is a plain C struct; zeroed is valid.
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: handle is a valid stdout handle, info is a properly-sized buffer.
    let ret = unsafe { GetConsoleScreenBufferInfo(handle, &mut info) };
    if ret == 0 {
        return Ok(TerminalSize {
            ws_row: 24,
            ws_col: 80,
        });
    }
    let rows = (info.srWindow.Bottom - info.srWindow.Top + 1) as u16;
    let cols = (info.srWindow.Right - info.srWindow.Left + 1) as u16;
    Ok(TerminalSize {
        ws_row: if rows == 0 { 24 } else { rows },
        ws_col: if cols == 0 { 80 } else { cols },
    })
}

/// Terminal dimensions, cross-platform replacement for `nix::pty::Winsize`.
#[derive(Debug, Clone, Copy)]
pub struct TerminalSize {
    pub ws_row: u16,
    pub ws_col: u16,
}

/// Put stdin into raw mode and return the saved console mode.
/// Returns `None` if stdin is not a console.
pub fn setup_raw_mode() -> Result<Option<ConsoleMode>> {
    // SAFETY: GetStdHandle returns a pseudo-handle for stdin.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode: u32 = 0;
    // SAFETY: handle is a valid console handle, mode is a valid u32 pointer.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Ok(None); // Not a console
    }

    let original_mode = mode;

    // Disable line input, echo, and processed input (raw mode)
    mode &= !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
    // Enable virtual terminal input for escape sequences
    mode |= ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT;

    // SAFETY: handle is a valid console handle, mode has been modified above.
    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        anyhow::bail!("SetConsoleMode failed: {}", io::Error::last_os_error());
    }

    // Also enable VT processing on stdout
    // SAFETY: GetStdHandle returns a pseudo-handle for stdout.
    let stdout_handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let mut out_mode: u32 = 0;
    let mut original_out_mode = None;
    // SAFETY: stdout_handle and out_mode are valid pointers for GetConsoleMode.
    if unsafe { GetConsoleMode(stdout_handle, &mut out_mode) } != 0 {
        original_out_mode = Some(out_mode);
        out_mode |= ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        // SAFETY: stdout_handle is valid, out_mode has VT processing flag set.
        unsafe { SetConsoleMode(stdout_handle, out_mode) };
    }

    Ok(Some(ConsoleMode {
        stdin_handle: handle,
        original_mode,
        stdout_handle,
        original_out_mode,
    }))
}

/// Install Ctrl+C / Ctrl+Break handler.
pub fn setup_signal_handlers() -> Result<()> {
    // SAFETY: ctrl_handler is a valid extern "system" callback that only
    // performs atomic stores, which are safe from any thread context.
    if unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), TRUE) } == 0 {
        anyhow::bail!(
            "SetConsoleCtrlHandler failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}

/// No-op on Windows — handles don't use fcntl-style non-blocking.
pub fn set_nonblocking<T>(_: &T) -> Result<()> {
    Ok(())
}

/// Write all bytes via std::io::Write, retrying on interrupts.
pub fn write_all<W: io::Write>(writer: &mut W, data: &[u8]) -> Result<()> {
    writer.write_all(data).context("write failed")?;
    Ok(())
}

/// Write all bytes to stdout.
pub fn write_all_stdout(data: &[u8]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(data).context("write to stdout failed")?;
    stdout.flush().context("flush stdout failed")?;
    Ok(())
}

/// Read from stdin, returning bytes read. Returns Err on failure.
pub fn read_stdin(buf: &mut [u8]) -> Result<usize, io::Error> {
    let mut stdin = io::stdin().lock();
    stdin.read(buf)
}

/// Convert an `ExitStatus` to an integer exit code.
pub fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
