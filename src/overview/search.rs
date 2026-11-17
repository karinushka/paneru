//! Fuzzy matching for the overview's window search (`/`).
//!
//! Pure: takes the query and a window's searchable text, returns a score.
//! The query is split on whitespace and every term has to match one of the
//! fields (title, app name, workspace), so "brave github" finds a Brave
//! window titled GitHub. A term matches a field when its characters appear
//! in it in order, case-insensitively, without being strewn across it: after
//! skipping characters the match has to pick up again at a word start
//! (`vsc` → Visual Studio Code), unless its characters are all close together
//! (`gthb` → GitHub). Without that, a long title holds the letters of almost
//! any word somewhere ("youtube" in "marY's nOtes abOUT the cUBE…").
//! Matches at word starts and runs of consecutive characters score higher.

/// Per matched character.
const MATCH: i32 = 16;
/// Matched right after the previous match.
const CONSECUTIVE: i32 = 12;
/// Matched at the start of a word (or of the text).
const WORD_START: i32 = 14;
/// The term starts at the very beginning of the field.
const PREFIX: i32 = 8;
/// Per skipped character between two matches, capped per gap.
const GAP: i32 = 1;
const MAX_GAP_PENALTY: i32 = 8;
/// A match skipping into the middle of words still counts when it spans at
/// most this many times the term's length.
const COMPACT_SPAN: usize = 2;

/// How a window matched: its score (higher is better) and, per field, the
/// character positions the query's terms landed on (for highlighting).
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    pub score: i32,
    pub hits: Vec<Vec<usize>>,
}

/// Matches `query` against a window's `fields`. `None` when some term
/// matches none of the fields. An empty query matches everything with
/// score 0 and no hits.
pub fn find(query: &str, fields: &[&str]) -> Option<Found> {
    let chars: Vec<Vec<char>> = fields.iter().map(|f| f.chars().collect()).collect();
    let mut found = Found {
        score: 0,
        hits: vec![Vec::new(); fields.len()],
    };
    for term in query.split_whitespace() {
        let term: Vec<char> = term.chars().collect();
        let (field, (score, positions)) = chars
            .iter()
            .enumerate()
            .filter_map(|(i, field)| Some((i, score_term(&term, field)?)))
            .max_by_key(|(_, (score, _))| *score)?;
        found.score += score;
        found.hits[field].extend(positions);
    }
    for hits in &mut found.hits {
        hits.sort_unstable();
        hits.dedup();
    }
    Some(found)
}

fn same(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

fn word_start(text: &[char], at: usize) -> bool {
    let Some(prev) = at.checked_sub(1).map(|i| text[i]) else {
        return true;
    };
    let current = text[at];
    !prev.is_alphanumeric() || (prev.is_lowercase() && current.is_uppercase())
}

/// Best score of `term` as a subsequence of `text`, with the positions it
/// matched: every place its first character occurs is tried as a start,
/// matching the rest greedily (with a preference for word starts) from
/// there.
fn score_term(term: &[char], text: &[char]) -> Option<(i32, Vec<usize>)> {
    let first = *term.first()?;
    (0..text.len())
        .filter(|&start| same(text[start], first))
        .filter_map(|start| score_from(term, text, start))
        // The earliest of equally good starts.
        .rev()
        .max_by_key(|(score, _)| *score)
}

fn score_from(term: &[char], text: &[char], start: usize) -> Option<(i32, Vec<usize>)> {
    let mut total = MATCH + if start == 0 { PREFIX } else { 0 };
    if word_start(text, start) {
        total += WORD_START;
    }
    let mut positions = Vec::with_capacity(term.len());
    positions.push(start);
    let mut last = start;
    // Every gap so far ended on a word start.
    let mut gaps_at_words = true;
    for &wanted in &term[1..] {
        let next = last + 1;
        // Right after the previous match beats anything later.
        let at = if text.get(next).is_some_and(|&c| same(c, wanted)) {
            next
        } else {
            let mut candidates = (next..text.len()).filter(|&i| same(text[i], wanted));
            let first = candidates.next()?;
            // A later word start is worth jumping to; a mid-word hit isn't.
            std::iter::once(first)
                .chain(candidates)
                .find(|&i| word_start(text, i))
                .unwrap_or(first)
        };
        total += MATCH;
        if at == next {
            total += CONSECUTIVE;
        } else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let gap = (at - next) as i32;
            total -= (gap * GAP).min(MAX_GAP_PENALTY);
        }
        let at_word = word_start(text, at);
        if at_word {
            total += WORD_START;
        } else if at != next {
            gaps_at_words = false;
        }
        positions.push(at);
        last = at;
    }
    let compact = last + 1 - start <= term.len() * COMPACT_SPAN;
    (gaps_at_words || compact).then_some((total, positions))
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
        // Holds y…o…u…t…u…b…e, each in the middle of a word.
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
}
