//! Low-level terminal setup utilities: raw mode, signal handlers, I/O helpers.

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::pty::Winsize;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::{isatty, read, write};
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the SIGWINCH handler when the terminal is resized.
pub static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);
/// Set by the SIGINT handler when Ctrl+C is pressed.
pub static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);
/// Set by the SIGTERM handler when a terminate signal is received.
pub static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sigint(_: libc::c_int) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sigterm(_: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::SeqCst);
}

/// Query the terminal dimensions via ioctl. Falls back to 24x80 on failure.
pub fn get_terminal_size() -> Result<Winsize> {
    // SAFETY: Winsize is a plain C struct with no padding requirements;
    // zeroed memory is a valid initial state.
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes the terminal dimensions into ws, a valid
    // Winsize that outlives the call; stdout is a valid fd.
    let ret = unsafe {
        libc::ioctl(
            io::stdout().as_raw_fd(),
            libc::TIOCGWINSZ as libc::c_ulong,
            &mut ws,
        )
    };
    if ret == -1 || ws.ws_row == 0 || ws.ws_col == 0 {
        ws.ws_row = 24;
        ws.ws_col = 80;
    }
    Ok(ws)
}

/// Put stdin into raw mode and return the original termios settings.
/// Returns `None` if stdin is not a TTY.
pub fn setup_raw_mode() -> Result<Option<Termios>> {
    let stdin = io::stdin();
    if !isatty(&stdin).unwrap_or(false) {
        return Ok(None);
    }

    let original = tcgetattr(&stdin).context("tcgetattr failed")?;
    let mut raw = original.clone();
    cfmakeraw(&mut raw);
    tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("tcsetattr failed")?;
    Ok(Some(original))
}

fn setup_signal_handler(signal: Signal, handler: extern "C" fn(libc::c_int)) -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: The signal handler only performs atomic stores, which is
    // async-signal-safe. The SigAction is correctly constructed above.
    unsafe { sigaction(signal, &action) }.context(format!("sigaction {signal:?} failed"))?;
    Ok(())
}

/// Install signal handlers for SIGWINCH, SIGINT, and SIGTERM.
pub fn setup_signal_handlers() -> Result<()> {
    setup_signal_handler(Signal::SIGWINCH, handle_sigwinch)?;
    setup_signal_handler(Signal::SIGINT, handle_sigint)?;
    setup_signal_handler(Signal::SIGTERM, handle_sigterm)?;
    Ok(())
}

/// Set a file descriptor to non-blocking mode.
pub fn set_nonblocking<Fd: AsFd>(fd: &Fd) -> Result<()> {
    let flags = fcntl(fd.as_fd(), FcntlArg::F_GETFL).context("fcntl F_GETFL failed")?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .context("fcntl F_SETFL failed")?;
    Ok(())
}

/// Write all bytes to a file descriptor, retrying on EAGAIN/EINTR.
pub fn write_all<F: AsFd>(fd: &F, data: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < data.len() {
        match write(fd, &data[written..]) {
            Ok(n) => written += n,
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => continue,
            Err(e) => anyhow::bail!("write failed: {e}"),
        }
    }
    Ok(())
}

/// Read from a file descriptor, returning the nix `Errno` on failure.
pub fn nix_read<F: AsFd>(fd: &F, buf: &mut [u8]) -> Result<usize, Errno> {
    read(fd.as_fd(), buf)
}

/// Convert an `ExitStatus` to an integer exit code (128 + signal for signaled processes).
pub fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        1
    }
}

/// Restore terminal settings. Called from `Proxy::drop`.
pub fn restore_termios(termios: &Termios) {
    let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, termios);
}
