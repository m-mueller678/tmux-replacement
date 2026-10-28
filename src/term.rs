//! Raw-mode handling and terminal size queries for the outer terminal.

use std::io::{self, Write};
use std::os::fd::BorrowedFd;
use std::sync::Mutex;

use nix::sys::termios::{self, SetArg, Termios};

/// Undoes the input modes an application may have left enabled.
pub const MODE_RESET: &str = "\x1b[?1l\x1b>\x1b[?2004l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[0 q";

/// Puts the terminal back in a sane state for the shell the user returns to.
pub fn reset() -> String {
    format!("{MODE_RESET}\x1b[0m\x1b[?25h")
}

static ORIGINAL: Mutex<Option<Termios>> = Mutex::new(None);

fn stdin_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 0 stays open for the lifetime of the process.
    unsafe { BorrowedFd::borrow_raw(0) }
}

pub fn enable_raw_mode() -> nix::Result<()> {
    let original = termios::tcgetattr(stdin_fd())?;
    let mut raw = original.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(stdin_fd(), SetArg::TCSANOW, &raw)?;
    *ORIGINAL.lock().unwrap() = Some(original);
    Ok(())
}

pub fn restore_mode() {
    if let Some(original) = ORIGINAL.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = termios::tcsetattr(stdin_fd(), SetArg::TCSANOW, original);
    }
}

/// Returns (rows, cols) of the outer terminal.
pub fn size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize into the provided pointer.
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row, ws.ws_col)
    } else {
        (24, 80)
    }
}

pub fn write(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}
