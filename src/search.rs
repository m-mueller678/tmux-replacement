//! Matching picker entries against a query the way fzf does by default.
//!
//! The query is split at spaces into terms that must all match, in any
//! order. A term matches fuzzily, as a subsequence, unless written as
//! `'exact`, `^prefix` or `suffix$`; `!term` excludes entries containing
//! it; `a | b` matches either. Matching ignores case unless the query has
//! an uppercase letter.

/// Score for each matched character.
const MATCH: i64 = 16;
const GAP_START: i64 = -3;
const GAP_EXTENSION: i64 = -1;
/// Bonus for a match after a space, at the start, or after a delimiter
/// or other non-word character.
const BOUNDARY_WHITE: i64 = MATCH / 2 + 2;
const BOUNDARY_DELIMITER: i64 = MATCH / 2 + 1;
const BOUNDARY: i64 = MATCH / 2;
/// Bonus for a match at a camelCase hump or the start of a number.
const CAMEL: i64 = BOUNDARY - 1;
/// Least bonus for a match right after another.
const CONSECUTIVE: i64 = -(GAP_START + GAP_EXTENSION);
/// The first query character's bonus counts this many times.
const FIRST_MULTIPLIER: i64 = 2;

#[derive(Debug, PartialEq)]
enum Kind {
    Fuzzy,
    Exact,
    Prefix,
    Suffix,
    /// `^word$`.
    Whole,
}

#[derive(Debug)]
struct Term {
    kind: Kind,
    negated: bool,
    text: Vec<char>,
}

/// Matches `query` against `text`. Returns a score, higher is better, and
/// the positions of matched chars in `text`, sorted.
pub fn search(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    let case_sensitive = query.chars().any(char::is_uppercase);
    let fold = |c: char| {
        if case_sensitive {
            c
        } else {
            c.to_lowercase().next().unwrap_or(c)
        }
    };
    let original: Vec<char> = text.chars().collect();
    let bonuses: Vec<i64> = (0..original.len())
        .map(|i| bonus(i.checked_sub(1).map(|p| original[p]), original[i]))
        .collect();
    let text: Vec<char> = original.iter().copied().map(fold).collect();

    let mut score = 0;
    let mut positions = Vec::new();
    for group in parse(query, fold) {
        // The first alternative that matches counts, as in fzf.
        let (s, p) = group
            .iter()
            .find_map(|term| match_term(term, &text, &bonuses))?;
        score += s;
        positions.extend(p);
    }
    positions.sort_unstable();
    positions.dedup();
    Some((score, positions))
}

/// Terms, grouped into alternatives joined by `|`.
fn parse(query: &str, fold: impl Fn(char) -> char) -> Vec<Vec<Term>> {
    let mut groups: Vec<Vec<Term>> = Vec::new();
    let mut or = false;
    for word in query.split_whitespace() {
        if word == "|" {
            or = !groups.is_empty();
            continue;
        }
        let (negated, word) = match word.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, word),
        };
        let (kind, word) = if let Some(rest) = word.strip_prefix('\'') {
            (Kind::Exact, rest)
        } else if let Some(rest) = word.strip_prefix('^') {
            match rest.strip_suffix('$') {
                Some(rest) => (Kind::Whole, rest),
                None => (Kind::Prefix, rest),
            }
        } else if let Some(rest) = word.strip_suffix('$') {
            (Kind::Suffix, rest)
        } else if negated {
            // fzf: excluding by a fuzzy match would exclude too much.
            (Kind::Exact, word)
        } else {
            (Kind::Fuzzy, word)
        };
        if word.is_empty() {
            continue;
        }
        let term = Term {
            kind,
            negated,
            text: word.chars().map(&fold).collect(),
        };
        push(&mut groups, &mut or, term);
    }
    groups
}

fn push(groups: &mut Vec<Vec<Term>>, or: &mut bool, term: Term) {
    match groups.last_mut() {
        Some(group) if *or => group.push(term),
        _ => groups.push(vec![term]),
    }
    *or = false;
}

fn match_term(term: &Term, text: &[char], bonuses: &[i64]) -> Option<(i64, Vec<usize>)> {
    let found = match term.kind {
        Kind::Fuzzy => fuzzy(&term.text, text, bonuses),
        Kind::Exact => (0..=text.len().saturating_sub(term.text.len()))
            .filter(|&start| text[start..].starts_with(&term.text))
            .map(|start| contiguous(start, term.text.len(), bonuses))
            .max_by_key(|(score, _)| *score),
        Kind::Prefix => text
            .starts_with(&term.text)
            .then(|| contiguous(0, term.text.len(), bonuses)),
        Kind::Whole => (text == term.text).then(|| contiguous(0, text.len(), bonuses)),
        Kind::Suffix => text
            .ends_with(&term.text)
            .then(|| contiguous(text.len() - term.text.len(), term.text.len(), bonuses)),
    };
    match (found, term.negated) {
        (Some(found), false) => Some(found),
        (None, true) => Some((0, Vec::new())),
        _ => None,
    }
}

/// Bonus for matching `c` after `prev`.
fn bonus(prev: Option<char>, c: char) -> i64 {
    let Some(prev) = prev else {
        return BOUNDARY_WHITE;
    };
    if !c.is_alphanumeric() {
        return 0;
    }
    if prev.is_whitespace() {
        BOUNDARY_WHITE
    } else if matches!(prev, '/' | ',' | ':' | ';' | '|' | '-' | '_' | '.') {
        BOUNDARY_DELIMITER
    } else if !prev.is_alphanumeric() {
        BOUNDARY
    } else if (prev.is_lowercase() && c.is_uppercase()) || (!prev.is_numeric() && c.is_numeric()) {
        CAMEL
    } else {
        0
    }
}

/// Score of a run of `len` matched chars starting at `start`.
fn contiguous(start: usize, len: usize, bonuses: &[i64]) -> (i64, Vec<usize>) {
    let mut score = 0;
    let mut first = 0;
    for (k, p) in (start..start + len).enumerate() {
        let b = bonuses[p];
        score += MATCH
            + if k == 0 {
                first = b;
                b * FIRST_MULTIPLIER
            } else {
                if b >= BOUNDARY && b > first {
                    first = b;
                }
                b.max(first).max(CONSECUTIVE)
            };
    }
    (score, (start..start + len).collect())
}

/// The best scoring alignment of `query` as a subsequence of `text`.
fn fuzzy(query: &[char], text: &[char], bonuses: &[i64]) -> Option<(i64, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let (n, m) = (query.len(), text.len());
    // Best score with query[i] matched at text[j]; the bonus of the run of
    // consecutive matches it ends; and where query[i - 1] matched.
    #[derive(Clone, Copy)]
    struct Cell {
        score: i64,
        run_bonus: i64,
        from: usize,
    }
    let mut table: Vec<Vec<Option<Cell>>> = vec![vec![None; m]; n];
    for i in 0..n {
        let above = if i > 0 {
            table[i - 1].clone()
        } else {
            Vec::new()
        };
        for j in i..m {
            if text[j] != query[i] {
                continue;
            }
            let b = bonuses[j];
            if i == 0 {
                table[0][j] = Some(Cell {
                    score: MATCH + b * FIRST_MULTIPLIER,
                    run_bonus: b,
                    from: 0,
                });
                continue;
            }
            let mut best: Option<Cell> = None;
            for (k, prev) in above.iter().enumerate().take(j).skip(i - 1) {
                let Some(prev) = *prev else {
                    continue;
                };
                let cell = if k + 1 == j {
                    let run_bonus = if b >= BOUNDARY && b > prev.run_bonus {
                        b
                    } else {
                        prev.run_bonus
                    };
                    Cell {
                        score: prev.score + MATCH + b.max(run_bonus).max(CONSECUTIVE),
                        run_bonus,
                        from: k,
                    }
                } else {
                    let gap = (j - k - 1) as i64;
                    Cell {
                        score: prev.score + MATCH + b + GAP_START + GAP_EXTENSION * (gap - 1),
                        run_bonus: b,
                        from: k,
                    }
                };
                if best.is_none_or(|best| cell.score > best.score) {
                    best = Some(cell);
                }
            }
            table[i][j] = best;
        }
    }
    let (mut j, last) = (0..m)
        .filter_map(|j| Some((j, table[n - 1][j]?)))
        .max_by_key(|(j, cell)| (cell.score, std::cmp::Reverse(*j)))?;
    let score = last.score;
    let mut positions = vec![0; n];
    for i in (0..n).rev() {
        positions[i] = j;
        j = table[i][j].unwrap().from;
    }
    Some((score, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(query: &str, text: &str) -> bool {
        search(query, text).is_some()
    }

    fn positions(query: &str, text: &str) -> Vec<usize> {
        search(query, text).unwrap().1
    }

    fn score(query: &str, text: &str) -> i64 {
        search(query, text).unwrap().0
    }

    #[test]
    fn fuzzy_terms() {
        assert!(matches("abc", "a_b_c"));
        assert!(!matches("abd", "abc"));
        assert!(matches("", "anything"));
        // Terms match anywhere, in any order.
        assert!(matches("web ser", "web-server"));
        assert!(matches("ser web", "web-server"));
        assert!(matches("tr bu", "tmux-replacement-build"));
        assert!(!matches("web xyz", "web-server"));
        assert_eq!(positions("ser web", "web-server"), [0, 1, 2, 4, 5, 6]);
    }

    #[test]
    fn smart_case() {
        assert!(matches("ne", "New"));
        assert!(matches("Ne", "New"));
        assert!(!matches("NE", "new"));
    }

    #[test]
    fn prefers_the_better_alignment() {
        // Greedy matching would take the first 'b'.
        assert_eq!(positions("ab", "a-xb-b"), [0, 5]);
        assert_eq!(positions("bar", "b-foo-bar"), [6, 7, 8]);
        assert_eq!(positions("fb", "foo bar"), [0, 4]);
        assert!(score("web", "web-server") > score("web", "wild-ember"));
        assert!(score("ws", "web-server") > score("ws", "awesome"));
        assert!(score("tr", "tmux-replacement") > score("tr", "attrs"));
    }

    #[test]
    fn extended_syntax() {
        assert!(matches("'serv", "web-server"));
        assert!(!matches("'sver", "web-server"));
        assert!(matches("^web", "web-server"));
        assert!(!matches("^ser", "web-server"));
        assert!(matches("ver$", "web-server"));
        assert!(!matches("web$", "web-server"));
        assert!(matches("^web$", "web"));
        assert!(!matches("^web$", "web-server"));
        assert!(!matches("!serv", "web-server"));
        assert!(matches("!xyz", "web-server"));
        assert!(matches("web !xyz", "web-server"));
        assert!(matches("xyz | web", "web-server"));
        assert!(matches("ser xyz | web", "web-server"));
        assert!(!matches("xyz | abc", "web-server"));
        // Operators alone are ignored.
        assert!(matches("' ^ !", "web"));
        assert_eq!(positions("^we ver$", "web-server"), [0, 1, 7, 8, 9]);
    }
}
