//! The fzf-like session picker.

use std::fmt::Write;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::session::SessionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    New,
    Exit,
}

/// Menu entries with fixed names. Sessions sharing one of these names are
/// shown as `s#<name>`.
pub const BUILTINS: &[(&str, Action, &str)] = &[
    ("new", Action::New, "create a new session"),
    ("exit", Action::Exit, "quit, ending all sessions"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Session(SessionId),
    Action(Action),
}

pub struct Item {
    pub label: String,
    pub target: Target,
    /// Dimmed text shown after the label.
    pub note: Option<String>,
}

pub enum Outcome {
    /// Keep the menu open.
    Continue,
    Cancel,
    /// Ctrl-b: return to the previous session and send it a literal ctrl-b.
    PassPrefix,
    Select(Target),
    /// Ask for the name of a new session, prefilled with the given text.
    NameSession(String),
    /// The name prompt was confirmed.
    Create(String),
}

enum Key {
    Text(String),
    Enter,
    Escape,
    Up,
    Down,
    Backspace,
    ClearLine,
    DeleteWord,
    Prefix,
    Interrupt,
    Ignored,
}

/// Decodes one key from the start of `bytes`, returning it and its length.
fn parse_key(bytes: &[u8]) -> (Key, usize) {
    let key = match bytes[0] {
        0x1b => match bytes.get(1) {
            // A lone escape at the end of a read is the escape key.
            None => return (Key::Escape, 1),
            Some(b'[' | b'O') => {
                let end = (2..bytes.len())
                    .find(|&j| (0x40..=0x7e).contains(&bytes[j]))
                    .unwrap_or(bytes.len() - 1);
                let key = match bytes[end] {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    _ => Key::Ignored,
                };
                return (key, end + 1);
            }
            // Alt+key.
            Some(_) => return (Key::Ignored, 2),
        },
        b'\r' => Key::Enter,
        0x02 => Key::Prefix,
        0x03 | 0x07 => Key::Interrupt,
        0x10 | 0x0b => Key::Up,
        0x0e | b'\n' => Key::Down,
        0x7f | 0x08 => Key::Backspace,
        0x15 => Key::ClearLine,
        0x17 => Key::DeleteWord,
        b if b < 0x20 => Key::Ignored,
        b => {
            let len = match b {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            }
            .min(bytes.len());
            let key = match std::str::from_utf8(&bytes[..len]) {
                Ok(s) => Key::Text(s.to_string()),
                Err(_) => Key::Ignored,
            };
            return (key, len);
        }
    };
    (key, 1)
}

/// Applies a line-editing key to `line`. Returns false if the key is not an
/// editing key.
fn edit(line: &mut String, key: &Key) -> bool {
    match key {
        Key::Text(s) => line.push_str(s),
        Key::Backspace => {
            line.pop();
        }
        Key::ClearLine => line.clear(),
        Key::DeleteWord => {
            let trimmed = line.trim_end_matches(' ');
            let keep = trimmed.rfind(' ').map_or(0, |p| p + 1);
            line.truncate(keep);
        }
        _ => return false,
    }
    true
}

struct NamePrompt {
    name: String,
    /// Used when the name is left empty.
    default: String,
}

/// Bytes that arrive for the session while the menu is open, replayed when
/// returning to it so the outer terminal stays in sync without a repaint.
const PENDING_LIMIT: usize = 4 * 1024 * 1024;

pub struct Menu {
    pub return_to: Option<SessionId>,
    pending: Vec<u8>,
    overflowed: bool,
    query: String,
    selected: usize,
    items: Vec<Item>,
    /// Indices into `items` with the matched char positions, best first.
    matches: Vec<(usize, Vec<usize>)>,
    message: Option<String>,
    naming: Option<NamePrompt>,
}

impl Menu {
    pub fn new(return_to: Option<SessionId>, items: Vec<Item>) -> Menu {
        let mut menu = Menu {
            return_to,
            pending: Vec::new(),
            overflowed: false,
            query: String::new(),
            selected: 0,
            items: Vec::new(),
            matches: Vec::new(),
            message: None,
            naming: None,
        };
        menu.set_items(items);
        menu
    }

    pub fn set_items(&mut self, items: Vec<Item>) {
        self.items = items;
        self.refilter();
    }

    pub fn forget_return_target(&mut self) {
        self.return_to = None;
        self.pending.clear();
    }

    pub fn prompt_name(&mut self, name: String, default: String) {
        self.message = None;
        self.naming = Some(NamePrompt { name, default });
    }

    pub fn set_message(&mut self, message: String) {
        self.message = Some(message);
    }

    pub fn buffer_output(&mut self, bytes: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.pending.len() + bytes.len() > PENDING_LIMIT {
            self.overflowed = true;
            self.pending = Vec::new();
        } else {
            self.pending.extend_from_slice(bytes);
        }
    }

    /// The buffered output, or None if too much arrived to replay it.
    pub fn take_pending(&mut self) -> Option<Vec<u8>> {
        (!self.overflowed).then(|| std::mem::take(&mut self.pending))
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(i64, usize, Vec<usize>)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                fuzzy_match(&self.query, &item.label).map(|(s, pos)| (s, i, pos))
            })
            .collect();
        // Stable sort keeps the original order among equal scores.
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        self.matches = scored.into_iter().map(|(_, i, pos)| (i, pos)).collect();
        self.selected = 0;
    }

    /// Handles keyboard input. Returns the outcome and how many bytes were
    /// consumed; bytes after a closing outcome belong to whatever comes next.
    pub fn handle_input(&mut self, bytes: &[u8]) -> (Outcome, usize) {
        let mut i = 0;
        while i < bytes.len() {
            let (key, len) = parse_key(&bytes[i..]);
            i += len;
            let outcome = match key {
                Key::Prefix => Outcome::PassPrefix,
                Key::Interrupt => Outcome::Cancel,
                _ if self.naming.is_some() => self.name_key(key),
                _ => self.list_key(key),
            };
            if !matches!(outcome, Outcome::Continue) {
                return (outcome, i);
            }
        }
        (Outcome::Continue, i)
    }

    fn list_key(&mut self, key: Key) -> Outcome {
        match key {
            Key::Escape => return Outcome::Cancel,
            Key::Enter => return self.accept(),
            Key::Up => self.move_selection(1),
            Key::Down => self.move_selection(-1),
            key => {
                if edit(&mut self.query, &key) {
                    self.message = None;
                    self.refilter();
                }
            }
        }
        Outcome::Continue
    }

    fn name_key(&mut self, key: Key) -> Outcome {
        let prompt = self.naming.as_mut().unwrap();
        match key {
            Key::Escape => {
                self.naming = None;
                self.message = None;
            }
            Key::Enter => {
                let name = prompt.name.trim();
                let name = if name.is_empty() {
                    &prompt.default
                } else {
                    name
                };
                if name.contains('#') {
                    self.message = Some("session names cannot contain '#'".into());
                } else {
                    return Outcome::Create(name.to_string());
                }
            }
            key => {
                if edit(&mut prompt.name, &key) {
                    self.message = None;
                }
            }
        }
        Outcome::Continue
    }

    fn move_selection(&mut self, delta: isize) {
        let max = self.matches.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
    }

    fn accept(&mut self) -> Outcome {
        match self.matches.get(self.selected) {
            Some((i, _)) => Outcome::Select(self.items[*i].target),
            None => Outcome::NameSession(self.query.trim().to_string()),
        }
    }

    /// Draws the menu fzf-style: prompt on the bottom row, best match just
    /// above it.
    pub fn render(&self, (rows, cols): (u16, u16)) -> String {
        let cols = cols as usize;
        let mut out = String::from("\x1b[?2026h\x1b[?25l\x1b[0m\x1b[H\x1b[2J");

        if let Some(prompt) = &self.naming {
            let (color, info) = match &self.message {
                Some(msg) => ("31", msg.clone()),
                None => (
                    "2",
                    format!(
                        "name for the new session (empty: {}), esc to go back",
                        prompt.default
                    ),
                ),
            };
            let _ = write!(
                out,
                "\x1b[{};1H  \x1b[{color}m{}\x1b[0m",
                rows.saturating_sub(1).max(1),
                truncate(&info, cols.saturating_sub(2))
            );
            return finish_prompt(out, rows, cols, "name", &prompt.name);
        }

        let list_rows = (rows as usize).saturating_sub(2);
        let offset = (self.selected + 1).saturating_sub(list_rows);
        for (n, (i, positions)) in self.matches.iter().enumerate().skip(offset).take(list_rows) {
            let item = &self.items[*i];
            let row = list_rows - (n - offset);
            let selected = n == self.selected;
            let _ = write!(out, "\x1b[{row};1H");
            out.push_str(if selected {
                "\x1b[1;31m>\x1b[0;1m "
            } else {
                "  "
            });
            let mut used = 2;
            for (ci, c) in item.label.chars().enumerate() {
                let w = c.width().unwrap_or(0);
                if used + w > cols {
                    break;
                }
                used += w;
                if positions.contains(&ci) {
                    out.push_str("\x1b[32m");
                    out.push(c);
                    out.push_str("\x1b[39m");
                } else {
                    out.push(c);
                }
            }
            if let Some(note) = &item.note {
                let note = truncate(note, cols.saturating_sub(used + 2));
                if !note.is_empty() {
                    let _ = write!(out, "  \x1b[2m{note}");
                }
            }
            out.push_str("\x1b[0m");
        }

        let info = match &self.message {
            Some(msg) => format!("\x1b[31m{}", truncate(msg, cols.saturating_sub(2))),
            None => {
                let count = format!("{}/{}", self.matches.len(), self.items.len());
                let rule = "─".repeat(cols.saturating_sub(count.len() + 3));
                format!("\x1b[33m{count} \x1b[2m{rule}")
            }
        };
        let _ = write!(
            out,
            "\x1b[{};1H  {info}\x1b[0m",
            rows.saturating_sub(1).max(1)
        );

        finish_prompt(out, rows, cols, "", &self.query)
    }
}

/// Draws the input line on the bottom row and leaves the cursor in it.
fn finish_prompt(mut out: String, rows: u16, cols: usize, label: &str, text: &str) -> String {
    let prefix = format!("{label}> ");
    let text = truncate(text, cols.saturating_sub(prefix.width() + 1));
    let _ = write!(
        out,
        "\x1b[{rows};1H\x1b[1;34m{prefix}\x1b[0m{text}\x1b[{rows};{}H\x1b[?25h\x1b[?2026l",
        1 + prefix.width() + text.width()
    );
    out
}

fn truncate(s: &str, max: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= max
        })
        .collect()
}

/// Case-insensitive subsequence match. Returns a score (higher is better) and
/// the matched char positions in `text`.
fn fuzzy_match(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let text: Vec<char> = text
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();

    // Try every start position of the first query char and greedily match
    // the rest; keep the best scoring alignment.
    let mut best: Option<(i64, Vec<usize>)> = None;
    for start in (0..text.len()).filter(|&s| text[s] == query[0]) {
        let mut positions = vec![start];
        let mut t = start + 1;
        for &qc in &query[1..] {
            match (t..text.len()).find(|&j| text[j] == qc) {
                Some(j) => {
                    positions.push(j);
                    t = j + 1;
                }
                None => break,
            }
        }
        if positions.len() != query.len() {
            break;
        }
        let mut score = 0i64;
        for (k, &p) in positions.iter().enumerate() {
            let boundary = p == 0 || !text[p - 1].is_alphanumeric();
            if boundary {
                score += 8;
            }
            if k > 0 {
                let gap = (p - positions[k - 1] - 1) as i64;
                score += if gap == 0 { 6 } else { -gap.min(10) };
            }
        }
        score -= (text.len() as i64) / 8;
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, positions));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_basics() {
        assert!(fuzzy_match("abc", "a_b_c").is_some());
        assert!(fuzzy_match("abd", "abc").is_none());
        assert_eq!(fuzzy_match("NE", "new").unwrap().1, vec![0, 1]);
        let contiguous = fuzzy_match("web", "web-server").unwrap().0;
        let scattered = fuzzy_match("web", "wild-ember").unwrap().0;
        assert!(contiguous > scattered);
    }

    #[test]
    fn keys() {
        let items = vec![
            Item {
                label: "work".into(),
                target: Target::Session(1),
                note: None,
            },
            Item {
                label: "new".into(),
                target: Target::Action(Action::New),
                note: None,
            },
        ];
        let mut menu = Menu::new(Some(1), items);
        assert!(matches!(menu.handle_input(b"ne"), (Outcome::Continue, 2)));
        assert!(matches!(
            menu.handle_input(b"\rls"),
            (Outcome::Select(Target::Action(Action::New)), 1)
        ));
        assert!(matches!(
            menu.handle_input(b"\x1b[A"),
            (Outcome::Continue, 3)
        ));
        assert!(matches!(menu.handle_input(b"\x1b"), (Outcome::Cancel, 1)));
        assert!(matches!(
            menu.handle_input(b"\x02x"),
            (Outcome::PassPrefix, 1)
        ));
        menu.handle_input(b"\x15zzz");
        assert!(matches!(menu.handle_input(b"\r"), (Outcome::NameSession(ref n), _) if n == "zzz"));
    }

    #[test]
    fn name_prompt() {
        let mut menu = Menu::new(None, Vec::new());
        menu.prompt_name("ab".into(), "3".into());
        menu.handle_input(b"\x7fc");
        assert!(matches!(menu.handle_input(b"\r"), (Outcome::Create(ref n), _) if n == "ac"));
        menu.handle_input(b"\x15");
        assert!(matches!(menu.handle_input(b"\r"), (Outcome::Create(ref n), _) if n == "3"));
        menu.handle_input(b"a#b");
        assert!(matches!(menu.handle_input(b"\r"), (Outcome::Continue, 1)));
        assert!(menu.message.is_some());
        menu.handle_input(b"\x1b");
        assert!(menu.naming.is_none());
        assert!(matches!(menu.handle_input(b"\x1b"), (Outcome::Cancel, 1)));
    }
}
