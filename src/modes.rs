//! Terminal state a session can change that vt100 does not keep: modes
//! affecting keyboard input, wrapping and character sets, colors, and the
//! window title. They reach the outer terminal along with the session's output, so
//! leaving the session has to undo them, or they leak into the picker and
//! other sessions, and returning to it has to restore them.

use std::collections::BTreeMap;

use crate::session::Alerts;

/// DEC private modes vt100 ignores, with their defaults.
const PRIVATE_MODES: &[(u16, bool)] = &[
    (5, false),    // reverse video
    (7, true),     // autowrap
    (1004, false), // focus reports
    (1007, false), // mouse wheel as arrow keys on the alternate screen
    (1015, false), // urxvt mouse encoding
    (1016, false), // SGR mouse encoding in pixels
    (2031, false), // color scheme reports
    (2048, false), // in-band resize reports
];

/// Feeds output to a session's emulator. Also tracks what vt100 handles
/// without telling its callbacks: full resets, and shifting to G1 and back
/// (SO and SI).
pub fn process(parser: &mut vt100::Parser<Hooks>, bytes: &[u8]) {
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if parser.callbacks_mut().scan(b) {
            parser.process(&bytes[start..=i]);
            reset(parser);
            start = i + 1;
        }
    }
    parser.process(&bytes[start..]);
}

fn reset(parser: &mut vt100::Parser<Hooks>) {
    let hooks = parser.callbacks_mut();
    // A reset leaves the title alone. Whether it resets colors depends on
    // the terminal.
    *hooks = Hooks {
        alerts: std::mem::take(&mut hooks.alerts),
        title: hooks.title.take(),
        colors_unknown: true,
        ..Hooks::default()
    };
}

/// Where the output so far ended, as far as [`Hooks::scan`] needs to know.
#[derive(Default, Clone, Copy)]
enum Scan {
    #[default]
    Ground,
    Esc,
    /// After `ESC` and intermediate bytes, as in `ESC ( 0`.
    EscIntermediate,
    Csi,
    /// Inside an OSC, DCS or similar string, where controls are data.
    Str,
}

/// Callbacks for a session's emulator: collects attention requests and
/// tracks the state vt100 leaves out.
#[derive(Default)]
pub struct Hooks {
    pub alerts: Alerts,
    /// Modes from PRIVATE_MODES that differ from their default.
    private: BTreeMap<u16, bool>,
    insert: bool,
    /// Kitty keyboard protocol flag stacks of the main and alternate
    /// screens; the top entry is in effect.
    kitty: [Vec<u16>; 2],
    /// xterm's modifyOtherKeys level.
    modify_other_keys: u16,
    /// Whether G0 and G1 hold the line drawing character set.
    graphics: [bool; 2],
    /// Whether text is drawn from G1 (after SO).
    shifted: bool,
    /// Palette entries changed with OSC 4, as specified.
    palette: BTreeMap<u16, Vec<u8>>,
    /// Foreground, background and cursor color set with OSC 10, 11 and 12.
    dynamic: [Option<Vec<u8>>; 3],
    title: Option<Vec<u8>>,
    scan: Scan,
    /// Whether colors may have been changed before a full reset.
    colors_unknown: bool,
}

impl Hooks {
    /// Follows the output byte by byte, as vt100's parser does, so SO, SI
    /// and resets count only where they take effect, even when split
    /// across reads. Returns whether `b` completes a full reset.
    fn scan(&mut self, b: u8) -> bool {
        self.scan = match (self.scan, b) {
            (Scan::Str, 0x07 | 0x18 | 0x1a) => Scan::Ground,
            // ESC ends a string; `ESC \` is just the usual way.
            (_, 0x1b) => Scan::Esc,
            (Scan::Str, _) => Scan::Str,
            (_, 0x18 | 0x1a) => Scan::Ground,
            // Controls take effect even inside escape sequences.
            (state, 0x0e | 0x0f) => {
                self.shifted = b == 0x0e;
                state
            }
            (Scan::Esc, b'c') => {
                self.scan = Scan::Ground;
                return true;
            }
            (Scan::Esc, b'[') => Scan::Csi,
            (Scan::Esc, b']' | b'P' | b'X' | b'^' | b'_') => Scan::Str,
            (Scan::Esc | Scan::EscIntermediate, 0x20..=0x2f) => Scan::EscIntermediate,
            (state @ (Scan::Esc | Scan::EscIntermediate | Scan::Csi), 0x00..=0x1f) => state,
            (Scan::Csi, 0x40..=0x7e) => Scan::Ground,
            (Scan::Csi, _) => Scan::Csi,
            _ => Scan::Ground,
        };
        false
    }

    /// Whether the application asked to be told when the terminal gains
    /// and loses focus.
    pub fn focus_reports(&self) -> bool {
        self.private.get(&1004) == Some(&true)
    }

    fn kitty_flags(&self, alternate: bool) -> u16 {
        self.kitty[alternate as usize].last().copied().unwrap_or(0)
    }

    /// Sequences undoing the session's changes, for leaving it while it is
    /// on the given screen.
    pub fn reset(&self, alternate: bool) -> String {
        let mut out = String::new();
        for (mode, &on) in &self.private {
            out.push_str(&format!("\x1b[?{mode}{}", if on { 'l' } else { 'h' }));
        }
        out.push_str(&self.reset_text());
        if self.kitty_flags(alternate) != 0 {
            out.push_str("\x1b[=0;1u");
        }
        if self.modify_other_keys != 0 {
            out.push_str("\x1b[>4;0m");
        }
        if self.colors_unknown {
            out.push_str("\x1b]104\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07");
        } else {
            if !self.palette.is_empty() {
                let entries: Vec<_> = self.palette.keys().map(u16::to_string).collect();
                out.push_str(&format!("\x1b]104;{}\x07", entries.join(";")));
            }
            for (i, color) in self.dynamic.iter().enumerate() {
                if color.is_some() {
                    out.push_str(&format!("\x1b]{}\x07", 110 + i));
                }
            }
        }
        if self.title.is_some() {
            out.push_str("\x1b]2;back-burner\x07");
        }
        out
    }

    /// Sequences restoring the session's changes, for returning to it on
    /// the given screen.
    pub fn restore(&self, alternate: bool) -> String {
        let mut out = String::new();
        for (mode, &on) in &self.private {
            out.push_str(&format!("\x1b[?{mode}{}", if on { 'h' } else { 'l' }));
        }
        out.push_str(&self.restore_text());
        let flags = self.kitty_flags(alternate);
        if flags != 0 {
            out.push_str(&format!("\x1b[={flags};1u"));
        }
        if self.modify_other_keys != 0 {
            out.push_str(&format!("\x1b[>4;{}m", self.modify_other_keys));
        }
        for (entry, color) in &self.palette {
            let color = String::from_utf8_lossy(color);
            out.push_str(&format!("\x1b]4;{entry};{color}\x07"));
        }
        for (i, color) in self.dynamic.iter().enumerate() {
            if let Some(color) = color {
                let color = String::from_utf8_lossy(color);
                out.push_str(&format!("\x1b]{};{color}\x07", 10 + i));
            }
        }
        if let Some(title) = &self.title {
            out.push_str(&format!("\x1b]2;{}\x07", String::from_utf8_lossy(title)));
        }
        out
    }

    /// Makes text written next appear as is, not inserted or as line
    /// drawing; for drawing over the session.
    pub fn reset_text(&self) -> String {
        let mut out = String::new();
        if self.insert {
            out.push_str("\x1b[4l");
        }
        if self.graphics[0] {
            out.push_str("\x1b(B");
        }
        if self.graphics[1] {
            out.push_str("\x1b)B");
        }
        if self.shifted {
            out.push('\x0f');
        }
        out
    }

    /// Undoes [`Self::reset_text`].
    pub fn restore_text(&self) -> String {
        let mut out = String::new();
        if self.insert {
            out.push_str("\x1b[4h");
        }
        if self.graphics[0] {
            out.push_str("\x1b(0");
        }
        if self.graphics[1] {
            out.push_str("\x1b)0");
        }
        if self.shifted {
            out.push('\x0e');
        }
        out
    }
}

impl vt100::Callbacks for Hooks {
    fn audible_bell(&mut self, screen: &mut vt100::Screen) {
        self.alerts.audible_bell(screen);
    }

    fn visual_bell(&mut self, screen: &mut vt100::Screen) {
        self.alerts.visual_bell(screen);
    }

    fn unhandled_osc(&mut self, screen: &mut vt100::Screen, params: &[&[u8]]) {
        self.alerts.unhandled_osc(screen, params);
        let number = |p: &[u8]| std::str::from_utf8(p).ok()?.parse::<u16>().ok();
        let Some((&first, rest)) = params.split_first() else {
            return;
        };
        // "?" asks the terminal for the color instead of setting it.
        match number(first) {
            Some(4) => {
                for pair in rest.chunks_exact(2) {
                    if let (Some(entry), color) = (number(pair[0]), pair[1])
                        && color != b"?"
                    {
                        self.palette.insert(entry, color.to_vec());
                    }
                }
            }
            Some(104) if rest.is_empty() => self.palette.clear(),
            Some(104) => {
                for entry in rest.iter().filter_map(|p| number(p)) {
                    self.palette.remove(&entry);
                }
            }
            // OSC 10 may go on to set 11 and 12, and so on.
            Some(n @ 10..=12) => {
                for (i, &color) in (n as usize - 10..3).zip(rest) {
                    if color != b"?" {
                        self.dynamic[i] = Some(color.to_vec());
                    }
                }
            }
            Some(n @ 110..=112) => self.dynamic[n as usize - 110] = None,
            _ => {}
        }
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(title.to_vec());
    }

    fn unhandled_escape(&mut self, _: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, b: u8) {
        match i1 {
            Some(b'(') => self.graphics[0] = b == b'0',
            Some(b')') => self.graphics[1] = b == b'0',
            _ => {}
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let param = |i: usize| params.get(i).and_then(|p| p.first().copied());
        let kitty = &mut self.kitty[screen.alternate_screen() as usize];
        match (i1, i2, c) {
            (Some(b'?'), None, 'h' | 'l') => {
                for &mode in params.iter().filter_map(|p| p.first()) {
                    let Some(&(_, default)) = PRIVATE_MODES.iter().find(|(m, _)| *m == mode) else {
                        continue;
                    };
                    if (c == 'h') == default {
                        self.private.remove(&mode);
                    } else {
                        self.private.insert(mode, c == 'h');
                    }
                }
            }
            (None, None, 'h' | 'l') if params.iter().any(|p| p.first() == Some(&4)) => {
                self.insert = c == 'h';
            }
            (Some(b'>'), None, 'u') => kitty.push(param(0).unwrap_or(0)),
            (Some(b'<'), None, 'u') => {
                let n = param(0).unwrap_or(1).max(1) as usize;
                kitty.truncate(kitty.len().saturating_sub(n));
            }
            (Some(b'='), None, 'u') => {
                let flags = param(0).unwrap_or(0);
                if kitty.is_empty() {
                    kitty.push(0);
                }
                let top = kitty.last_mut().unwrap();
                match param(1).unwrap_or(1) {
                    1 => *top = flags,
                    2 => *top |= flags,
                    3 => *top &= !flags,
                    _ => {}
                }
            }
            (Some(b'>'), None, 'm') => match param(0) {
                Some(4) => self.modify_other_keys = param(1).unwrap_or(0),
                None => self.modify_other_keys = 0,
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::nz;

    fn hooks(bytes: &[u8]) -> vt100::Parser<Hooks> {
        let mut parser = vt100::Parser::new_with_callbacks(nz(24), nz(80), 0, Hooks::default());
        parser.process(bytes);
        parser
    }

    #[test]
    fn tracks_what_vt100_does_not() {
        // An editor on the alternate screen enabling focus reports and the
        // kitty keyboard protocol; autowrap switched off and on again.
        let p = hooks(b"\x1b[?1049h\x1b[?1004h\x1b[>1u\x1b[=3;2u\x1b[?7l\x1b[?7h\x1b]2;hx\x07");
        let h = p.callbacks();
        assert!(h.focus_reports());
        assert_eq!(h.reset(true), "\x1b[?1004l\x1b[=0;1u\x1b]2;back-burner\x07");
        assert_eq!(h.restore(true), "\x1b[?1004h\x1b[=3;1u\x1b]2;hx\x07");
        // The main screen's keyboard stack is untouched.
        assert_eq!(h.reset(false), "\x1b[?1004l\x1b]2;back-burner\x07");

        let p = hooks(b"\x1b[>1u\x1b[>5u\x1b[<2u\x1b[>4;2m\x1b[4h\x1b(0\x1b[?7l");
        let h = p.callbacks();
        assert_eq!(h.reset(false), "\x1b[?7h\x1b[4l\x1b(B\x1b[>4;0m");
        assert_eq!(h.restore(false), "\x1b[?7l\x1b[4h\x1b(0\x1b[>4;2m");

        assert_eq!(
            hooks(b"\x1b[?1004h\x1b[?1004l").callbacks().reset(false),
            ""
        );
    }

    #[test]
    fn tracks_colors() {
        let p = hooks(
            b"\x1b]4;1;#ff0000;2;?\x07\x1b]4;3;red\x1b\\\x1b]104;3\x07\
              \x1b]11;#000000;#00ff00\x07\x1b]10;?\x07\x1b]112\x07",
        );
        let h = p.callbacks();
        assert_eq!(h.reset(false), "\x1b]104;1\x07\x1b]111\x07");
        assert_eq!(h.restore(false), "\x1b]4;1;#ff0000\x07\x1b]11;#000000\x07");
    }

    #[test]
    fn notices_shifts_and_resets() {
        let mut p = hooks(b"\x1b)0\x0eabc");
        process(&mut p, b"");
        assert_eq!(p.callbacks().reset_text(), "\x1b)B");
        process(&mut p, b"\x0e");
        assert_eq!(p.callbacks().reset_text(), "\x1b)B\x0f");
        assert_eq!(p.callbacks().restore_text(), "\x1b)0\x0e");
        process(&mut p, b"\x0f\x1b]2;t\x07\x1b[?1004h");
        assert_eq!(p.callbacks().reset_text(), "\x1b)B");
        let after_reset = "\x1b]104\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07\x1b]2;back-burner\x07";
        process(&mut p, b"\x1b[4h\x1bc\x1b[?2004h");
        assert_eq!(p.callbacks().reset(false), after_reset);
        assert_eq!(p.callbacks().restore(false), "\x1b]2;t\x07");
        process(&mut p, b"\x1b[?1004h\x1b");
        process(&mut p, b"c");
        assert_eq!(p.callbacks().reset(false), after_reset);
    }

    #[test]
    fn ignores_lookalikes_in_strings() {
        // SO as data in strings, and `c` ending `ESC (` rather than
        // following a lone ESC, split across reads; SO in a CSI counts.
        let mut p = hooks(b"");
        process(&mut p, b"\x1bP\x0e\x1b\\\x1b)0\x1b[?1004h\x1b(");
        process(&mut p, b"c\x1b]0;\x0e");
        process(&mut p, b"\x07\x1b[1\x0e;1H");
        let h = p.callbacks();
        assert_eq!(h.restore_text(), "\x1b)0\x0e");
        assert!(h.reset(false).starts_with("\x1b[?1004l"));
    }
}
