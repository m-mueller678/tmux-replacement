//! Session output covering the emulator's features, checking that a repaint
//! reproduces the screen, history and modes it leaves behind, at its size
//! and in tiny terminals.

use crate::modes::Hooks;
use crate::session::{SCROLLBACK, nz, repaint, view_size};

/// Name, columns, rows and output, with newlines as a pty sends them.
const CASES: &[(&str, u16, u16, &[u8])] = &[
    // Cursor movement.
    ("relative-moves", 12, 5, b"start\x1b[3B\x1b[2Dx\x1b[2A\x1b[4Cy\x1b[Ez\x1b[2Fw"),
    ("absolute-moves", 12, 5, b"\x1b[4;7Ha\x1b[2Gb\x1b[5dc\x1b[1;12fd\x1b[;He"),
    ("moves-clamped", 6, 4, b"\x1b[99;99Hx\x1b[99Ay\x1b[99Dz\x1b[0;0Hw"),
    ("save-restore-esc", 10, 4, b"one\x1b7\x1b[3;3Htwo\x1b8!"),
    ("save-restore-csi", 10, 4, b"\x1b[2;4H\x1b[s\x1b[4;1Hlow\x1b[uhi"),
    ("index-and-reverse", 8, 4, b"a\x1bDb\x1bDc\x1bMd\x1bEe"),
    ("backspace-at-margin", 6, 3, b"\x08\x08ab\x08\x08\x08c"),
    ("carriage-return", 8, 3, b"overwrite\rOVER"),
    // Tab stops.
    ("default-tabs", 20, 3, b"\tx\ty\r\n1\t\t2"),
    ("custom-tabs", 20, 3, b"\x1b[3G\x1bH\x1b[7G\x1bH\r\tp\tq\r\n\tr"),
    ("cleared-tabs", 20, 3, b"\x1b[3g\tend\r\n\x1b[9G\x1bH\r\tset\x1b[0g\r\n\tnone"),
    ("tab-forward-back", 24, 3, b"\x1b[2Ia\x1b[Zb\x1b[Zc"),
    // Editing the line.
    ("insert-chars", 10, 3, b"0123456789\r\x1b[4G\x1b[3@ab"),
    ("delete-chars", 10, 3, b"0123456789\r\x1b[2G\x1b[4P"),
    ("erase-chars", 10, 3, b"0123456789\x1b[1;5H\x1b[3X"),
    ("erase-line-parts", 9, 4, b"abcdefghi\x1b[5D\x1b[K\r\nabcdefghi\x1b[5D\x1b[1K\r\nabcdefghi\x1b[2K!"),
    ("insert-mode", 10, 3, b"tail\r\x1b[4hhead \x1b[4lX"),
    ("repeat-char", 10, 3, b"-\x1b[5b|\x1b[20b"),
    ("screen-alignment", 5, 3, b"text\x1b#8"),
    // Editing the screen.
    ("erase-below", 6, 4, b"aaaaaa\r\nbbbbbb\r\ncccccc\x1b[2;3H\x1b[0J"),
    ("erase-above", 6, 4, b"aaaaaa\r\nbbbbbb\r\ncccccc\x1b[2;3H\x1b[1J"),
    ("erase-all", 6, 4, b"aaaaaa\r\nbbbbbb\x1b[2Jafter"),
    ("insert-lines", 6, 5, b"l1\r\nl2\r\nl3\r\nl4\x1b[2H\x1b[2Lnew"),
    ("delete-lines", 6, 5, b"l1\r\nl2\r\nl3\r\nl4\x1b[2H\x1b[2Mnew"),
    ("scroll-up-down", 6, 4, b"r1\r\nr2\r\nr3\r\nr4\x1b[2S\x1b[1T"),
    // Scrolling regions and origin mode.
    ("region-scroll", 6, 6, b"top\x1b[6;1Hbottom\x1b[2;4r\x1b[4;1Hx\r\ny\r\nz"),
    ("region-reverse", 6, 6, b"top\x1b[2;4r\x1b[2;1Hm\x1bMn\x1bMo"),
    ("region-insert", 6, 6, b"1\r\n2\r\n3\r\n4\r\n5\x1b[2;4r\x1b[3;1H\x1b[5L+"),
    ("region-reset", 6, 4, b"\x1b[2;3r\x1b[r\x1b[4;1H\n\nend"),
    ("origin-mode", 8, 6, b"\x1b[3;5r\x1b[?6h\x1b[Hfirst\x1b[9;1Hlast\x1b[?6lhome"),
    ("bad-region", 8, 4, b"\x1b[3;2r\x1b[4;1Hstill\r\nscrolls"),
    // Wrapping.
    ("autowrap", 5, 3, b"abcdefghijk"),
    ("pending-wrap", 5, 3, b"abcde\rX\x1b[1;5Hy\x08Z"),
    ("no-autowrap", 5, 3, b"\x1b[?7labcdefgh\x1b[?7h\r\nijklmn"),
    ("wrap-then-scroll", 4, 2, b"aaaabbbbccccdd"),
    // History.
    ("history", 6, 3, b"h1\r\nh2\r\nh3\r\nh4\r\nh5\r\nh6\r\nh7"),
    ("history-wrapped", 4, 2, b"longer line\r\nnext\r\nlast"),
    ("history-colored", 6, 2, b"\x1b[31mred\r\n\x1b[42mgreen\r\n\x1b[mplain\r\nend"),
    ("history-then-clear", 6, 2, b"old1\r\nold2\r\nold3\x1b[H\x1b[2Jfresh"),
    // Attributes.
    ("sgr-basic", 20, 3, b"\x1b[1mb\x1b[3mi\x1b[4mu\x1b[7mr\x1b[22;23;24;27mn\x1b[1;7mx\x1b[0my"),
    ("sgr-colors", 20, 3, b"\x1b[33ma\x1b[44mb\x1b[93mc\x1b[105md\x1b[39;49me"),
    ("sgr-indexed", 20, 3, b"\x1b[38;5;208mo\x1b[48;5;17mn\x1b[38:5:46mg\x1b[m."),
    ("sgr-rgb", 20, 3, b"\x1b[38;2;10;20;30mr\x1b[48;2;200;100;0mg\x1b[38:2::1:2:3mb\x1b[m."),
    ("sgr-erase-color", 8, 3, b"\x1b[41m\x1b[2J\x1b[2;3H\x1b[K\x1b[m\x1b[3;1Hx"),
    ("sgr-garbage", 10, 3, b"\x1b[38;5mx\x1b[38;2;1my\x1b[999mz\x1b[;;1mw"),
    // Characters.
    ("utf8", 12, 3, "grüße ñ €".as_bytes()),
    ("wide", 8, 3, "日本語abc".as_bytes()),
    ("wide-at-edge", 5, 3, "abcd日e".as_bytes()),
    ("wide-overwritten", 6, 3, "日本\x1b[1;2Hx\x1b[1;3Hy".as_bytes()),
    ("combining", 8, 3, "e\u{301}a\u{308}\u{323}x".as_bytes()),
    ("emoji-sequences", 10, 3, "👍🏽🇩🇪x".as_bytes()),
    ("variation-selector", 10, 3, "✔\u{fe0f}✔\u{fe0e}.".as_bytes()),
    ("invalid-utf8", 10, 3, b"a\xffb\xc3c\xe2\x82d\xed\xa0\x80e"),
    ("controls-ignored", 8, 3, b"a\x00b\x05c\x7fd\x1be"),
    // Character sets.
    ("line-drawing", 10, 3, b"\x1b(0lqqk\r\nx  x\x1b(B ok"),
    ("shifted-g1", 10, 3, b"\x1b)0a\x0eqq\x0fb\x0ex"),
    // Screens.
    ("alternate-1049", 8, 3, b"main\x1b[?1049halt\x1b[2;2Hscreen"),
    ("alternate-back", 8, 3, b"main\x1b[?1049halt\x1b[?1049l!"),
    ("alternate-47", 8, 3, b"main\x1b[?47hother\x1b[?47l\x1b[?1047hthird"),
    ("full-reset", 8, 3, b"\x1b[31;1mold\x1b[?25l\x1b[?1hc\x1bcnew"),
    // Modes.
    ("hidden-cursor", 8, 3, b"x\x1b[?25l"),
    ("input-modes", 8, 3, b"\x1b[?1h\x1b=\x1b[?2004h\x1b[?1000h\x1b[?1006h"),
    ("mouse-modes", 8, 3, b"\x1b[?1002h\x1b[?1003h\x1b[?1003l\x1b[?1005h"),
    ("tracked-modes", 8, 3, b"\x1b[?1004h\x1b[?7l\x1b[4h\x1b[>1u\x1b[>4;2m\x1b]2;title\x07"),
    ("tracked-colors", 8, 3, b"\x1b]4;1;#112233\x07\x1b]11;rgb:00/00/00\x1b\\"),
    ("cursor-style", 8, 3, b"\x1b[4 qx"),
    // Malformed and unknown sequences.
    ("unknown-sequences", 12, 3, b"a\x1b[?4242xb\x1b[!zc\x1b]7777;?\x07d\x1bPq#0\x1b\\e"),
    ("cancelled", 12, 3, b"a\x1b[12\x18b\x1b]2;x\x1ac\x1b[1;\x1b[2md"),
    ("unterminated-osc", 12, 3, b"a\x1b]0;never ends\r\nstill title"),
    ("huge-params", 12, 3, b"\x1b[99999999;99999999Hx\x1b[99999999@y\x1b[99999999Lz"),
    ("many-params", 12, 3, b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24;25;26;27;28;29;30;31;32;33mx"),
    ("strings", 12, 3, b"a\x1b_apc\x1b\\b\x1b^pm\x1b\\c\x1bXsos\x1b\\d"),
];

fn emulate(rows: u16, cols: u16, output: &[u8]) -> vt100::Parser<Hooks> {
    let mut parser =
        vt100::Parser::new_with_callbacks(nz(rows), nz(cols), SCROLLBACK, Hooks::default());
    crate::modes::process(&mut parser, output);
    parser
}

/// What a terminal shows of a parser's state: cells, history, cursor and
/// modes.
fn state(parser: &mut vt100::Parser<Hooks>) -> String {
    // The history line by line, oldest first. Erased cells come back as
    // spaces.
    parser.screen_mut().set_scrollback(usize::MAX);
    let mut history = String::new();
    for offset in (1..=parser.screen().scrollback()).rev() {
        parser.screen_mut().set_scrollback(offset);
        let line = parser.screen().rows(0, u16::MAX).next().unwrap_or_default();
        history.push_str(line.trim_end());
        history.push('\n');
    }
    parser.screen_mut().set_scrollback(0);
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let mut out = String::new();
    for row in 0..rows.get() {
        for col in 0..cols.get() {
            let cell = screen.cell(row, col).unwrap();
            // Cells never written and cells holding a space look the same.
            let text = match cell.contents() {
                "" => " ",
                text => text,
            };
            out.push_str(&format!(
                "{text}{:?}{:?}{}{}{}{}{} ",
                cell.fgcolor(),
                cell.bgcolor(),
                cell.bold() as u8,
                cell.italic() as u8,
                cell.underline() as u8,
                cell.inverse() as u8,
                cell.is_wide() as u8,
            ));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "{history}\ncursor {:?} hidden {} alternate {}\n{:?}\n{:?}",
        screen.cursor_position(),
        screen.hide_cursor(),
        screen.alternate_screen(),
        screen.input_mode_formatted(),
        parser.callbacks().restore(screen.alternate_screen()),
    ));
    out
}

#[test]
fn repaints_cases() {
    for &(name, cols, rows, output) in CASES {
        // The size a session gets in a terminal one row taller for the bar.
        let (rows, cols) = view_size((rows + 1, cols));
        let mut session = emulate(rows, cols, output);
        let mut terminal = emulate(rows, cols, repaint(&mut session).as_bytes());
        assert_eq!(state(&mut terminal), state(&mut session), "{name}");
    }
}

#[test]
fn survives_tiny_terminals() {
    for &(_, _, _, output) in CASES {
        for size in (1..4).flat_map(|rows| (1..4).map(move |cols| (rows, cols))) {
            let (rows, cols) = view_size(size);
            let mut session = emulate(rows, cols, output);
            session.screen_mut().set_size(nz(rows + 2), nz(cols + 3));
            repaint(&mut session);
        }
    }
}
