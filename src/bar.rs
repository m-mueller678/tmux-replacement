//! The status bar on the terminal's bottom row while a session is shown.
//!
//! Session output still goes to the terminal as it arrives, so scrolling
//! and selection keep working natively. A scrolling region keeps the
//! session in the rows above the bar, and [`Filter`] adjusts the few
//! sequences that would otherwise reach or erase it.

use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthChar;

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Ground,
    Esc,
    /// After `ESC #`.
    EscHash,
    Csi,
    /// Inside an OSC, DCS or similar string.
    Str,
    /// An ESC inside a string: `\` ends it, anything else starts a new
    /// sequence.
    StrEsc,
}

/// Rewrites a session's output for a terminal with the status bar below it.
#[derive(Default)]
pub struct Filter {
    state: State,
    /// Bytes of the control sequence after `ESC [`, held back until its
    /// final byte shows whether it needs adjusting.
    csi: Vec<u8>,
    /// UTF-8 continuation bytes still expected.
    utf8: u8,
    /// Scrolling region the session set; None for the whole session area.
    region: Option<(u16, u16)>,
    damaged: bool,
}

impl Filter {
    /// `rows` is the number of rows the session has, all above the bar.
    pub fn process(&mut self, input: &[u8], rows: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            match self.state {
                State::Ground => {
                    out.push(b);
                    match b {
                        0x1b => {
                            self.state = State::Esc;
                            self.utf8 = 0;
                        }
                        0xc0..=0xdf => self.utf8 = 1,
                        0xe0..=0xef => self.utf8 = 2,
                        0xf0..=0xf7 => self.utf8 = 3,
                        0x80..=0xbf => self.utf8 = self.utf8.saturating_sub(1),
                        _ => self.utf8 = 0,
                    }
                }
                State::Esc => {
                    out.push(b);
                    self.escape(b);
                }
                State::EscHash => {
                    out.push(b);
                    if b == b'8' {
                        // DECALN fills the whole screen.
                        self.damaged = true;
                    }
                    self.state = State::Ground;
                }
                State::Csi => match b {
                    0x40..=0x7e => {
                        self.finish_csi(b, rows, &mut out);
                        self.state = State::Ground;
                    }
                    0x1b => {
                        out.append(&mut self.csi);
                        out.push(b);
                        self.state = State::Esc;
                    }
                    0x18 | 0x1a => {
                        out.append(&mut self.csi);
                        out.push(b);
                        self.state = State::Ground;
                    }
                    _ => self.csi.push(b),
                },
                State::Str => {
                    out.push(b);
                    match b {
                        0x07 | 0x18 | 0x1a => self.state = State::Ground,
                        0x1b => self.state = State::StrEsc,
                        _ => {}
                    }
                }
                State::StrEsc => {
                    out.push(b);
                    if b == b'\\' {
                        self.state = State::Ground;
                    } else {
                        self.escape(b);
                    }
                }
            }
        }
        out
    }

    fn escape(&mut self, b: u8) {
        self.state = match b {
            b'[' => {
                self.csi.clear();
                State::Csi
            }
            b']' | b'P' | b'_' | b'^' | b'X' => State::Str,
            b'#' => State::EscHash,
            // Intermediate bytes, as in `ESC ( B`.
            0x1b | 0x20..=0x2f => State::Esc,
            b'c' => {
                // A full reset clears the screen and the scrolling region.
                self.region = None;
                self.damaged = true;
                State::Ground
            }
            _ => State::Ground,
        }
    }

    fn finish_csi(&mut self, last: u8, rows: u16, out: &mut Vec<u8>) {
        let csi = std::mem::take(&mut self.csi);
        let private = csi.first().is_some_and(|b| (b'<'..=b'?').contains(b));
        let plain = !private && csi.iter().all(|b| (0x30..=0x3f).contains(b));
        let params = || {
            std::str::from_utf8(&csi)
                .unwrap_or("")
                .split(';')
                .map(|p| p.parse::<u16>().unwrap_or(0))
                .collect::<Vec<_>>()
        };
        match last {
            // Setting the scrolling region: keep it above the bar.
            b'r' if plain => {
                let p = params();
                let top = p.first().copied().unwrap_or(0).max(1);
                let bottom = match p.get(1).copied().unwrap_or(0) {
                    0 => rows,
                    b => b.min(rows),
                };
                if top < bottom {
                    self.region = Some((top, bottom));
                }
                let (top, bottom) = self.region(rows);
                out.extend(format!("{top};{bottom}r").as_bytes());
                return;
            }
            // Absolute cursor moves: stay above the bar.
            b'H' | b'f' | b'd' if plain => {
                let mut p = params();
                if p.first().is_some_and(|&row| row > rows) {
                    p[0] = rows;
                    let p: Vec<_> = p.iter().map(u16::to_string).collect();
                    out.extend(p.join(";").as_bytes());
                    out.push(last);
                    return;
                }
            }
            // Erasing the display may erase the bar.
            b'J' => self.damaged = true,
            // So may switching between the main and alternate screens.
            b'h' | b'l'
                if csi.first() == Some(&b'?')
                    && csi[1..]
                        .split(|&b| b == b';')
                        .any(|m| matches!(m, b"47" | b"1047" | b"1049")) =>
            {
                self.damaged = true;
            }
            _ => {}
        }
        out.extend(&csi);
        out.push(last);
    }

    /// The scrolling region to restore after drawing the bar.
    pub fn region(&self, rows: u16) -> (u16, u16) {
        match self.region {
            Some((top, bottom)) if top < bottom.min(rows) => (top, bottom.min(rows)),
            _ => (1, rows),
        }
    }

    /// Whether the output so far may have erased the bar. Clears the flag.
    pub fn take_damage(&mut self) -> bool {
        std::mem::take(&mut self.damaged)
    }

    /// Whether the output so far ended between characters and sequences, so
    /// the bar can be drawn without splitting one.
    pub fn at_boundary(&self) -> bool {
        self.state == State::Ground && self.utf8 == 0
    }
}

/// The bar's text, exactly `cols` columns wide: `left` (the session) on the
/// left, the host on the right.
pub fn text(left: &str, host: &str, cols: u16) -> String {
    let cols = cols as usize;
    let width = |s: &str| s.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
    let mut text = String::new();
    let mut used = 0;
    for c in format!(" {left}").chars() {
        let w = c.width().unwrap_or(0);
        if used + w > cols {
            break;
        }
        text.push(c);
        used += w;
    }
    let right = format!("{host} ");
    let right = if used + 1 + width(&right) <= cols {
        right
    } else {
        String::new()
    };
    let gap = cols - used - width(&right);
    text.extend(std::iter::repeat_n(' ', gap));
    text.push_str(&right);
    text
}

/// `when` in local time: the time of day, with the date unless it is the
/// same day as `now`.
pub fn timestamp(when: SystemTime, now: SystemTime) -> String {
    let (w, n) = (local_time(when), local_time(now));
    let time = format!("{:02}:{:02}:{:02}", w.tm_hour, w.tm_min, w.tm_sec);
    if (w.tm_year, w.tm_yday) == (n.tm_year, n.tm_yday) {
        time
    } else {
        let date = format!("{}-{:02}-{:02}", w.tm_year + 1900, w.tm_mon + 1, w.tm_mday);
        format!("{date} {time}")
    }
}

fn local_time(t: SystemTime) -> libc::tm {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: localtime_r only writes to the tm it is given, which is plain
    // integers and a pointer that may be null.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(chunks: &[&[u8]]) -> (Filter, Vec<u8>) {
        let mut f = Filter::default();
        let out = chunks.iter().flat_map(|c| f.process(c, 23)).collect();
        (f, out)
    }

    #[test]
    fn keeps_sessions_above_the_bar() {
        let (f, out) = filter(&[b"a\x1b[r\x1b[999;5Hb\x1b[2;30r\x1b[30d"]);
        assert_eq!(out, b"a\x1b[1;23r\x1b[23;5Hb\x1b[2;23r\x1b[23d");
        assert!(f.region(23) == (2, 23));
        assert!(f.region(10) == (2, 10));
        // Split across chunks, and other sequences pass untouched.
        let (_, out) = filter(&[b"\x1b[", b"?25l\x1b[3", b"r\x1b]0;t\x07\x1b[5;9H"]);
        assert_eq!(out, b"\x1b[?25l\x1b[3;23r\x1b]0;t\x07\x1b[5;9H");
    }

    #[test]
    fn notices_damage_and_boundaries() {
        let (mut f, _) = filter(&[b"x\x1b[?25h"]);
        assert!(!f.take_damage());
        f.process(b"\x1b[?1049h", 23);
        assert!(f.take_damage());
        assert!(!f.take_damage());
        f.process(b"\x1b[2J", 23);
        assert!(f.take_damage());
        f.process("é".as_bytes().split_at(1).0, 23);
        assert!(!f.at_boundary());
        f.process(b"\xa9\x1b]2;", 23);
        assert!(!f.at_boundary());
        f.process(b"x\x1b\\", 23);
        assert!(f.at_boundary());
    }

    #[test]
    fn formats_timestamps() {
        let now = SystemTime::now();
        let today = timestamp(now, now);
        assert_eq!(today.len(), 8, "{today}");
        let old = timestamp(now - std::time::Duration::from_secs(3 * 86400), now);
        assert_eq!(old.len(), 19, "{old}");
        assert!(old.ends_with(&today[2..]), "{old} {today}");
    }

    #[test]
    fn fits_text() {
        assert_eq!(text("work", "box", 12), " work   box ");
        assert_eq!(text("work", "box", 8), " work   ");
        assert_eq!(text("work", "box", 3), " wo");
    }
}
