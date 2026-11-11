//! A shell running on its own pty, plus an emulated copy of its screen so it
//! can be redrawn when the user switches back to it.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use vt100::capture::{BasicFormattedCaptureRange, BasicFormattedCaptureState};

use crate::modes::Hooks;
use crate::server::Event;
use crate::term::MODE_RESET;

pub type SessionId = u64;

pub const SCROLLBACK: usize = 10_000;

pub struct Session {
    pub name: String,
    pub pid: i32,
    master: File,
    parser: vt100::Parser<Hooks>,
    /// Why the session wants the user's attention, while nobody looked yet.
    pub attention: Option<String>,
    /// Whether the application was last told it has focus.
    focused: bool,
}

/// Requests for the user's attention found in a session's output.
#[derive(Default)]
pub struct Alerts {
    pub bell: bool,
    pub notifications: Vec<Notification>,
}

/// A desktop notification escape sequence (OSC 9, 777 or 99).
pub struct Notification {
    pub text: String,
    /// The sequence re-encoded, for forwarding to the outer terminal.
    pub sequence: Vec<u8>,
}

impl vt100::Callbacks for Alerts {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bell = true;
    }

    fn visual_bell(&mut self, _: &mut vt100::Screen) {
        self.bell = true;
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let text = match params {
            // ConEmu's progress reporting shares OSC 9 but is no request.
            [b"9", b"4", ..] => return,
            [b"9", message @ ..] => message.join(&b';'),
            [b"777", b"notify", title, body @ ..] => {
                [title.to_vec(), body.join(&b';')].join(&b": "[..])
            }
            // Kitty: the payload is last, after the metadata.
            [b"99", .., payload] => payload.to_vec(),
            _ => return,
        };
        let text = String::from_utf8_lossy(&text)
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let mut sequence = b"\x1b]".to_vec();
        sequence.extend(params.join(&b';'));
        sequence.push(0x07);
        self.notifications.push(Notification { text, sequence });
    }
}

/// The size a session gets in a terminal of `size`: the bottom row is left
/// for the status bar, and it is at least two columns wide.
/// Variables describing the user's terminal. Sessions get the values of the
/// client that creates them rather than the server's, which may have been
/// started without a terminal, e.g. through `ssh -T` with TERM=dumb.
pub const TERMINAL_VARS: &[&str] = &["TERM", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"];

pub fn view_size((rows, cols): (u16, u16)) -> (u16, u16) {
    // vt100 panics drawing a wide character on a single column.
    (if rows > 1 { rows - 1 } else { rows }, cols.max(2))
}

pub fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

fn winsize((rows, cols): (u16, u16)) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Session {
    pub fn spawn(
        id: SessionId,
        name: String,
        size: (u16, u16),
        cwd: Option<PathBuf>,
        terminal: &[(OsString, OsString)],
        events: Sender<Event>,
    ) -> io::Result<Session> {
        let pty = nix::pty::openpty(Some(&winsize(size)), None)?;
        // Keep these fds out of the shells spawned for other sessions, or a
        // session's master would never see EOF while another session lives.
        for fd in [pty.master.as_fd(), pty.slave.as_fd()] {
            fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        }

        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "bash".into());
        let mut cmd = Command::new(shell);
        cmd.stdin(Stdio::from(pty.slave.try_clone()?))
            .stdout(Stdio::from(pty.slave.try_clone()?))
            .stderr(Stdio::from(pty.slave))
            .env("BACK_BURNER_SESSION", &name)
            // The server may have been started from inside tmux, but its
            // sessions are not tmux panes. Programs seeing these would use
            // tmux, e.g. helix its buffers instead of the system clipboard.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            // Without a display, programs copy through the terminal (OSC
            // 52), which reaches the user's terminal wherever the session
            // runs, local or remote. The server's display may also be long
            // gone, as it outlives the login that started it.
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        for name in TERMINAL_VARS {
            cmd.env_remove(name);
        }
        cmd.envs(terminal.iter().cloned());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // Drop our copies of the slave side.
        drop(cmd);

        let master = File::from(pty.master);
        let mut reader = master.try_clone()?;
        thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if events.send(Event::Output(id, buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = events.send(Event::Exited(id));
        });

        Ok(Session {
            name,
            pid: child.id() as i32,
            master,
            parser: vt100::Parser::new_with_callbacks(
                nz(size.0),
                nz(size.1),
                SCROLLBACK,
                Hooks::default(),
            ),
            attention: None,
            focused: false,
        })
    }

    /// Tells an application asking for focus reports that a client shows
    /// it now. Sent on every attach: the terminal's own focus-in may have
    /// gone to a picker or been lost with the connection, leaving e.g.
    /// helix drawing its cursor as unfocused.
    pub fn focus_in(&mut self) {
        if self.parser.callbacks().focus_reports() {
            self.write(b"\x1b[I");
        }
        self.focused = true;
    }

    /// Tells the application that no client shows it any more.
    pub fn focus_out(&mut self) {
        if self.focused && self.parser.callbacks().focus_reports() {
            self.write(b"\x1b[O");
        }
        self.focused = false;
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.master.write_all(bytes);
    }

    pub fn process(&mut self, bytes: &[u8]) -> Alerts {
        crate::modes::process(&mut self.parser, bytes);
        std::mem::take(&mut self.parser.callbacks_mut().alerts)
    }

    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Returns whether the size changed.
    pub fn resize(&mut self, size: (u16, u16)) -> bool {
        let (rows, cols) = self.parser.screen().size();
        if (rows.get(), cols.get()) == size {
            return false;
        }
        // SAFETY: TIOCSWINSZ reads a winsize from the provided pointer.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &winsize(size)) };
        self.parser.set_size(nz(size.0), nz(size.1));
        true
    }

    /// Working directory of the session's foreground process.
    pub fn cwd(&self) -> Option<PathBuf> {
        // SAFETY: plain ioctl on an fd we own.
        let pgrp = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        [pgrp, self.pid]
            .into_iter()
            .filter(|pid| *pid > 0)
            .find_map(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
    }

    /// Escape sequences restoring the input modes and cursor visibility the
    /// application inside the session expects.
    pub fn modes(&self) -> String {
        modes(&self.parser)
    }

    /// Escape sequences undoing what [`Self::modes`] restores and vt100 does
    /// not reset by itself.
    pub fn leave(&self) -> String {
        leave(&self.parser)
    }

    pub fn repaint(&mut self) -> String {
        repaint(&mut self.parser)
    }
}

pub fn modes(parser: &vt100::Parser<Hooks>) -> String {
    let screen = parser.screen();
    let cursor = if screen.hide_cursor() {
        "\x1b[?25l"
    } else {
        "\x1b[?25h"
    };
    // Applications like editors set the cursor's shape once and expect it
    // to stay.
    let shape = match screen.cursor_style() {
        vt100::CursorStyle::Default => 0,
        vt100::CursorStyle::BlinkBlock => 1,
        vt100::CursorStyle::SteadyBlock => 2,
        vt100::CursorStyle::BlinkUnderline => 3,
        vt100::CursorStyle::SteadyUnderline => 4,
        vt100::CursorStyle::BlinkVerticalBar => 5,
        vt100::CursorStyle::SteadyVerticalBar => 6,
    };
    format!(
        "{MODE_RESET}{}\x1b[{shape} q{cursor}{}",
        screen.input_mode_formatted(),
        parser.callbacks().restore(screen.alternate_screen())
    )
}

pub fn leave(parser: &vt100::Parser<Hooks>) -> String {
    parser.callbacks().reset(parser.screen().alternate_screen())
}

/// Escape sequences that replace the outer terminal's screen and scrollback
/// with the emulated terminal's. Feeding them to a fresh emulator of the
/// same size reproduces the state too.
pub fn repaint(parser: &mut vt100::Parser<Hooks>) -> String {
    let mut out = String::from("\x1b[?2026h\x1b[?25l\x1b[?1049l\x1b[0m\x1b[H\x1b[2J\x1b[3J");

    // Emit the main screen with its scrollback as plain lines, so the outer
    // terminal scrolls the history into its own scrollback buffer.
    let alt = parser.screen().alternate_screen();
    if alt {
        // ?47 switches grids without clearing or touching saved cursors.
        parser.process(b"\x1b[?47l");
    }
    let screen = parser.screen();
    let mut text = String::new();
    let mut state = BasicFormattedCaptureState::new();
    let _ = screen
        .write_contents_formatted_basic(&mut text, BasicFormattedCaptureRange::Full(&mut state));
    out.push_str(&text.replace('\n', "\r\n"));
    out.push_str("\x1b[0m");
    out.push_str(&screen.cursor_state_formatted());
    if alt {
        parser.process(b"\x1b[?47h");
        out.push_str("\x1b[?1049h");
        out.push_str(&parser.screen().contents_formatted());
    } else {
        out.push_str(&screen.attributes_formatted());
    }
    out.push_str(&modes(parser));
    out.push_str("\x1b[?2026l");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alerts(bytes: &[u8]) -> Alerts {
        let mut parser = vt100::Parser::new_with_callbacks(nz(24), nz(80), 0, Alerts::default());
        parser.process(bytes);
        std::mem::take(parser.callbacks_mut())
    }

    #[test]
    fn detects_attention_requests() {
        assert!(alerts(b"done\x07").bell);
        assert!(!alerts(b"\x1b]0;title\x07").bell);

        let n = alerts(b"\x1b]9;build; done\x07").notifications;
        assert_eq!(n[0].text, "build; done");
        assert_eq!(n[0].sequence, b"\x1b]9;build; done\x07");
        let n = alerts(b"\x1b]777;notify;make;finished\x1b\\").notifications;
        assert_eq!(n[0].text, "make: finished");
        assert_eq!(
            alerts(b"\x1b]99;i=1;hello\x07").notifications[0].text,
            "hello"
        );
        assert!(alerts(b"\x1b]9;4;1;50\x07").notifications.is_empty());
    }

    #[test]
    fn restores_cursor_shape() {
        let mut parser = vt100::Parser::new_with_callbacks(nz(24), nz(80), 0, Hooks::default());
        parser.process(b"\x1b[2 q");
        assert!(modes(&parser).ends_with("\x1b[2 q\x1b[?25h"));
    }
}
