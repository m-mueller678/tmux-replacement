//! Matching picker entries against a query the way fzf does, with helix's
//! nucleo matcher.
//!
//! The query is split at spaces into terms that must all match, in any
//! order. A term matches fuzzily, as a subsequence, unless written as
//! `'exact`, `^prefix`, `suffix$` or `^whole$`; `!term` excludes entries
//! containing it. Matching ignores case unless the query has an uppercase
//! letter.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

pub struct Query {
    pattern: Pattern,
    matcher: Matcher,
}

impl Query {
    pub fn new(query: &str) -> Query {
        Query {
            pattern: Pattern::parse(query, CaseMatching::Smart, Normalization::Smart),
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// Returns a score, higher is better, and the positions of matched
    /// chars in `text`, sorted.
    pub fn matches(&mut self, text: &str) -> Option<(u32, Vec<usize>)> {
        // By chars rather than grapheme clusters, as the menu draws them.
        let chars: Vec<char> = text.chars().collect();
        let mut indices = Vec::new();
        let score =
            self.pattern
                .indices(Utf32Str::Unicode(&chars), &mut self.matcher, &mut indices)?;
        indices.sort_unstable();
        indices.dedup();
        Some((score, indices.into_iter().map(|i| i as usize).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(query: &str, text: &str) -> Option<(u32, Vec<usize>)> {
        Query::new(query).matches(text)
    }

    fn matches(query: &str, text: &str) -> bool {
        search(query, text).is_some()
    }

    fn positions(query: &str, text: &str) -> Vec<usize> {
        search(query, text).unwrap().1
    }

    fn score(query: &str, text: &str) -> u32 {
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
        // Positions count chars, not bytes or grapheme clusters.
        assert_eq!(positions("b", "e\u{301}ab"), [3]);
    }

    #[test]
    fn smart_case() {
        assert!(matches("ne", "New"));
        assert!(matches("Ne", "New"));
        assert!(!matches("NE", "new"));
    }

    #[test]
    fn prefers_the_better_alignment() {
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
        assert_eq!(positions("^we ver$", "web-server"), [0, 1, 7, 8, 9]);
    }
}
