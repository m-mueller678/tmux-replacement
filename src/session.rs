//! A shell running on its own pty, plus an emulated copy of its screen so it
//! can be redrawn when the user switches back to it.

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

use crate::server::Event;
use crate::term::MODE_RESET;

pub type SessionId = u64;

const SCROLLBACK: usize = 10_000;

pub struct Session {
    pub name: String,
    pub pid: i32,
    master: File,
    parser: vt100::Parser<Alerts>,
    /// Why the session wants the user's attention, while nobody looked yet.
    pub attention: Option<String>,
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

fn nz(n: u16) -> NonZeroU16 {
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
            .env("TMUX_REPLACEMENT_SESSION", &name);
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
                Alerts::default(),
            ),
            attention: None,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.master.write_all(bytes);
    }

    pub fn process(&mut self, bytes: &[u8]) -> Alerts {
        self.parser.process(bytes);
        std::mem::take(self.parser.callbacks_mut())
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
        let screen = self.parser.screen();
        let cursor = if screen.hide_cursor() {
            "\x1b[?25l"
        } else {
            "\x1b[?25h"
        };
        format!("{MODE_RESET}{}{cursor}", screen.input_mode_formatted())
    }

    /// Escape sequences that replace the outer terminal's screen and
    /// scrollback with this session's.
    pub fn repaint(&mut self) -> String {
        let mut out = String::from("\x1b[?2026h\x1b[?25l\x1b[?1049l\x1b[0m\x1b[H\x1b[2J\x1b[3J");

        // Emit the main screen with its scrollback as plain lines, so the
        // outer terminal scrolls the history into its own scrollback buffer.
        let alt = self.alternate_screen();
        if alt {
            // ?47 switches grids without clearing or touching saved cursors.
            self.parser.process(b"\x1b[?47l");
        }
        let screen = self.parser.screen();
        let mut text = String::new();
        let mut state = BasicFormattedCaptureState::new();
        let _ = screen.write_contents_formatted_basic(
            &mut text,
            BasicFormattedCaptureRange::Full(&mut state),
        );
        out.push_str(&text.replace('\n', "\r\n"));
        out.push_str("\x1b[0m");
        if alt {
            out.push_str(&screen.cursor_state_formatted());
            self.parser.process(b"\x1b[?47h");
            out.push_str("\x1b[?1049h");
            out.push_str(&self.parser.screen().contents_formatted());
        } else {
            out.push_str(&screen.cursor_state_formatted());
            out.push_str(&screen.attributes_formatted());
        }
        out.push_str(&self.modes());
        out.push_str("\x1b[?2026l");
        out
    }
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
}
