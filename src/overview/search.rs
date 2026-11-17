//! Fuzzy matching for the overview's window search (`/`).
//!
//! Pure: takes the query and a window's searchable text, returns a score.
//! Scoring is fzf's, from `nucleo-matcher` (word starts, camel humps and
//! runs of consecutive characters score higher; case and accents are
//! ignored), and each term can use fzf's syntax: `'exact`, `^prefix`,
//! `suffix$`, `!not`.
//!
//! The query is split on whitespace and every term has to match one of the
//! fields (title, app name, workspace), so "brave github" finds a Brave
//! window titled GitHub. On top of fzf, a fuzzy match mustn't be strewn
//! across the field: after skipping characters it has to pick up again at
//! a word start (`vsc` → Visual Studio Code), unless its characters are all
//! close together (`gthb` → GitHub). Without that, a long title holds the
//! letters of almost any word somewhere ("youtube" in "marY's nOtes
//! abOUT the cUBE…"), and fzf only ranks those low rather than dropping
//! them.

use std::cell::RefCell;

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use unicode_segmentation::UnicodeSegmentation as _;

/// A match skipping into the middle of words still counts when it spans at
/// most this many times the term's length.
const COMPACT_SPAN: usize = 2;

thread_local! {
    /// The matcher keeps scratch space between calls.
    static MATCHER: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT));
}

/// How a window matched: its score (higher is better) and, per field, the
/// character positions the query's terms landed on (for highlighting).
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    pub score: i32,
    pub hits: Vec<Vec<usize>>,
}

/// Matches `query` against a window's `fields`. `None` when some term
/// matches none of the fields (or a `!term` matches one). An empty query
/// matches everything with score 0 and no hits.
pub fn find(query: &str, fields: &[&str]) -> Option<Found> {
    MATCHER.with_borrow_mut(|matcher| {
        let mut found = Found {
            score: 0,
            hits: vec![Vec::new(); fields.len()],
        };
        for term in query.split_whitespace() {
            let atom = Atom::parse(term, CaseMatching::Ignore, Normalization::Smart);
            if atom.needle_text().is_empty() {
                continue;
            }
            if atom.negative {
                // `Atom` answers "doesn't contain" per field: all must agree.
                let mut buf = Vec::new();
                for field in fields {
                    atom.score(Utf32Str::new(field, &mut buf), matcher)?;
                }
                continue;
            }
            let (field, score, positions) = fields
                .iter()
                .enumerate()
                .filter_map(|(i, field)| {
                    let (score, positions) = match_field(&atom, field, matcher)?;
                    Some((i, score, positions))
                })
                .max_by_key(|&(i, score, _)| (score, std::cmp::Reverse(i)))?;
            found.score += i32::from(score);
            found.hits[field].extend(positions);
        }
        for hits in &mut found.hits {
            hits.sort_unstable();
            hits.dedup();
        }
        Some(found)
    })
}

/// Score and matched character positions of `atom` in `field`.
fn match_field(atom: &Atom, field: &str, matcher: &mut Matcher) -> Option<(u16, Vec<usize>)> {
    let mut buf = Vec::new();
    let haystack = Utf32Str::new(field, &mut buf);
    let mut indices = Vec::new();
    let score = atom.indices(haystack, matcher, &mut indices)?;
    indices.sort_unstable();
    indices.dedup();
    let indices: Vec<usize> = indices.into_iter().map(|i| i as usize).collect();
    if atom.kind == AtomKind::Fuzzy && !close_or_at_words(haystack, &indices) {
        return None;
    }
    Some((score, char_positions(field, haystack, indices)))
}

/// Whether a fuzzy match isn't strewn across the text: every gap ends at a
/// word start, or the whole match spans at most [`COMPACT_SPAN`] times its
/// length.
fn close_or_at_words(text: Utf32Str<'_>, positions: &[usize]) -> bool {
    let (Some(&first), Some(&last)) = (positions.first(), positions.last()) else {
        return true;
    };
    let compact = last + 1 - first <= positions.len() * COMPACT_SPAN;
    compact
        || positions
            .windows(2)
            .all(|pair| pair[1] == pair[0] + 1 || word_start(text, pair[1]))
}

fn word_start(text: Utf32Str<'_>, at: usize) -> bool {
    #[allow(clippy::cast_possible_truncation, reason = "titles are short")]
    let char_at = |i: usize| text.get(i as u32);
    let Some(prev) = at.checked_sub(1).map(char_at) else {
        return true;
    };
    let current = char_at(at);
    !prev.is_alphanumeric() || (prev.is_lowercase() && current.is_uppercase())
}

/// `positions` index what the matcher matched over; the highlighting
/// counts `char`s. That is the field's bytes when it is ASCII or folds to
/// it (`e` + a combining accent), else one character per grapheme.
fn char_positions(field: &str, haystack: Utf32Str<'_>, positions: Vec<usize>) -> Vec<usize> {
    if field.is_ascii() {
        return positions;
    }
    let starts: Vec<usize> = if haystack.is_ascii() {
        // Byte offset → char index, for the bytes that start a char.
        let mut starts = vec![usize::MAX; field.len()];
        for (char_index, (byte, _)) in field.char_indices().enumerate() {
            starts[byte] = char_index;
        }
        starts
    } else {
        field
            .graphemes(true)
            .scan(0, |at, grapheme| {
                let start = *at;
                *at += grapheme.chars().count();
                Some(start)
            })
            .collect()
    };
    positions
        .into_iter()
        .filter_map(|i| starts.get(i).copied().filter(|&c| c != usize::MAX))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, fields: &[&str]) -> Option<i32> {
        find(query, fields).map(|found| found.score)
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(score("", &["anything"]), Some(0));
        assert_eq!(score("   ", &[]), Some(0));
    }

    #[test]
    fn subsequence_matches_case_insensitively() {
        assert!(score("gthb", &["GitHub"]).is_some());
        assert!(score("GITHUB", &["github.com"]).is_some());
        assert_eq!(score("hubgit", &["GitHub"]), None);
    }

    #[test]
    fn every_term_has_to_match_some_field() {
        let fields = [
            "karinushka/paneru: pull request",
            "Brave Browser",
            "Workspace 2",
        ];
        assert!(score("brave paneru", &fields).is_some());
        assert!(score("work 2", &fields).is_some());
        assert_eq!(score("brave safari", &fields), None);
    }

    #[test]
    fn consecutive_and_word_starts_score_higher() {
        let exact = score("term", &["Terminal"]).unwrap();
        let scattered = score("term", &["tXeXrXm"]).unwrap();
        assert!(exact > scattered, "{exact} vs {scattered}");

        let initials = score("vsc", &["Visual Studio Code"]).unwrap();
        let mid_word = score("vsc", &["vasco"]).unwrap();
        assert!(initials > mid_word, "{initials} vs {mid_word}");
    }

    #[test]
    fn prefers_jumping_to_a_word_start() {
        // "pr" should land on "Pull Request", not on "p" + the "r" of "upr".
        let jumped = score("pr", &["upr Pull Request"]).unwrap();
        let plain = score("pr", &["xpyr"]).unwrap();
        assert!(jumped > plain, "{jumped} vs {plain}");
    }

    #[test]
    fn camel_case_humps_are_word_starts() {
        let humps = score("ow", &["OmniWM"]).unwrap();
        let inside = score("ow", &["oxxw"]).unwrap();
        assert!(humps > inside, "{humps} vs {inside}");
    }

    #[test]
    fn letters_strewn_across_a_title_dont_match() {
        // Holds y…o…u…t…u…b…e, each in the middle of a word: fzf alone
        // would match it.
        let title = "Mary's notes about the cube shapes";
        assert_eq!(score("youtube", &[title, "Notes"]), None);
        assert!(score("youtube", &["(3) YouTube - Brave"]).is_some());
        assert!(score("yt", &["YouTube"]).is_some(), "skips to a hump");
        // r…s…o…t: a subsequence, but strewn.
        assert!(score("rsot", &["release_notes"]).is_none());
        assert!(score("rn", &["release_notes"]).is_some(), "word starts");
        assert!(score("relnotes", &["release_notes"]).is_some());
    }

    #[test]
    fn reports_where_each_term_matched() {
        let found = find("brave gh", &["GitHub - paneru", "Brave Browser"]).unwrap();
        assert_eq!(found.hits, vec![vec![0, 3], vec![0, 1, 2, 3, 4]]);
        // Two terms on the same field merge.
        let found = find("pa ru", &["paneru"]).unwrap();
        assert_eq!(found.hits, vec![vec![0, 1, 4, 5]]);
        assert_eq!(find("", &["x"]).unwrap().hits, vec![Vec::<usize>::new()]);
    }

    #[test]
    fn non_ascii_titles_match() {
        assert!(score("café", &["Le Café Noir"]).is_some());
        assert!(score("ÉCOLE", &["école"]).is_some());
        assert!(score("日本", &["日本語のページ"]).is_some());
    }

    #[test]
    fn accents_fold_away() {
        assert!(score("cafe", &["Le Café Noir"]).is_some());
        assert!(score("zurich", &["Zürich weather"]).is_some());
    }

    #[test]
    fn fzf_syntax_works_per_term() {
        let fields = ["GitHub - paneru", "Brave Browser"];
        assert!(score("'paneru", &fields).is_some(), "exact substring");
        assert_eq!(score("'pnru", &fields), None, "exact needs it whole");
        assert!(score("^git", &fields).is_some(), "prefix");
        assert_eq!(score("^hub", &fields), None);
        assert!(score("browser$", &fields).is_some(), "suffix");
        assert_eq!(score("brave !github", &fields), None, "excluded");
        assert!(score("brave !safari", &fields).is_some());
    }

    #[test]
    fn hits_count_chars() {
        // "e" + a combining acute folds to ASCII: matched over bytes.
        let found = find("noir", &["Cafe\u{301} noir"]).unwrap();
        assert_eq!(found.hits, vec![vec![6, 7, 8, 9]]);
        // An emoji family is one grapheme of several chars.
        let found = find("noir", &["\u{1F468}\u{200D}\u{1F469} Café noir"]).unwrap();
        assert_eq!(found.hits, vec![vec![9, 10, 11, 12]]);
        let found = find("café", &["Le Café"]).unwrap();
        assert_eq!(found.hits, vec![vec![3, 4, 5, 6]]);
    }
}
