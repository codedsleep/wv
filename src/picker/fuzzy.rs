//! Subsequence matching with a score, for the goto picker's filter line.
//!
//! Small on purpose. The picker filters tens of rows of a few dozen characters
//! each, so this optimises for *ranking that feels right* rather than for
//! throughput, and takes the straightforward `O(query × candidate)` dynamic
//! program that finds the genuinely best alignment. A greedy walk is cheaper
//! but picks the leftmost match, which ranks `dev` inside `deploy-review` above
//! the `dev-server` window the user was actually reaching for.

/// Where a query matched, and how well.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    /// Indices into the candidate's `char`s, ascending. Used to embolden the
    /// matched characters when the row is drawn.
    pub positions: Vec<usize>,
}

const SCORE_START: i32 = 12;
const SCORE_BOUNDARY: i32 = 8;
const SCORE_CONSECUTIVE: i32 = 6;
const SCORE_CASE_MATCH: i32 = 2;
const PENALTY_GAP: i32 = -1;
/// A match that starts further in is worth less, so `main` beats `:1 main`.
const PENALTY_LEADING: i32 = -1;

/// Unreachable, and low enough that no run of penalties can climb out of it.
const UNREACHABLE: i32 = i32::MIN / 4;

/// A character that ends a word, so the character after it starts one.
fn is_separator(ch: char) -> bool {
    matches!(ch, '-' | '_' | ':' | '.' | '/' | ' ')
}

/// Score `query` against `candidate`, or `None` if it is not a subsequence.
///
/// An empty query matches everything at score 0, which is what leaves the
/// picker's natural order alone until the user types. Whitespace in the query
/// is dropped rather than matched: it is what a person types by accident, and
/// the rows are already full of it.
pub fn match_query(query: &str, candidate: &str) -> Option<Match> {
    let query: Vec<char> = query.chars().filter(|ch| !ch.is_whitespace()).collect();
    let candidate: Vec<char> = candidate.chars().collect();

    if query.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }

    if query.len() > candidate.len() {
        return None;
    }

    // `best[i][j]`: the score of the best alignment of `query[..=i]` that puts
    // `query[i]` at `candidate[j]`. `parent` remembers which `j` the previous
    // query character sat at, so the winning alignment can be walked back out.
    let width = candidate.len();
    let mut best = vec![UNREACHABLE; query.len() * width];
    let mut parent = vec![usize::MAX; query.len() * width];

    for (j, &ch) in candidate.iter().enumerate() {
        if eq_ignore_case(ch, query[0]) {
            best[j] = position_bonus(&candidate, j)
                + case_bonus(ch, query[0])
                + PENALTY_LEADING * clamp_index(j);
        }
    }

    for i in 1..query.len() {
        for j in i..width {
            if !eq_ignore_case(candidate[j], query[i]) {
                continue;
            }

            let landing = position_bonus(&candidate, j) + case_bonus(candidate[j], query[i]);
            for k in (i - 1)..j {
                let previous = best[(i - 1) * width + k];
                if previous == UNREACHABLE {
                    continue;
                }

                let gap = j - k - 1;
                let run = if gap == 0 { SCORE_CONSECUTIVE } else { 0 };
                let candidate_score =
                    previous + landing + run + PENALTY_GAP * clamp_index(gap);

                if candidate_score > best[i * width + j] {
                    best[i * width + j] = candidate_score;
                    parent[i * width + j] = k;
                }
            }
        }
    }

    let last = query.len() - 1;
    let (end, score) = (0..width)
        .map(|j| (j, best[last * width + j]))
        .filter(|(_, score)| *score != UNREACHABLE)
        .max_by_key(|(j, score)| (*score, std::cmp::Reverse(*j)))?;

    let mut positions = vec![0usize; query.len()];
    let mut at = end;
    for i in (0..query.len()).rev() {
        positions[i] = at;
        if i > 0 {
            at = parent[i * width + at];
        }
    }

    Some(Match { score, positions })
}

/// What landing on `candidate[at]` is worth before any run or gap is counted.
fn position_bonus(candidate: &[char], at: usize) -> i32 {
    if at == 0 {
        SCORE_START
    } else if is_separator(candidate[at - 1]) {
        SCORE_BOUNDARY
    } else {
        0
    }
}

fn case_bonus(found: char, wanted: char) -> i32 {
    if found == wanted {
        SCORE_CASE_MATCH
    } else {
        0
    }
}

/// Row labels are short; this only exists so a pathological one cannot overflow.
fn clamp_index(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

fn eq_ignore_case(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{match_query, Match};

    fn score_of(query: &str, candidate: &str) -> i32 {
        match_query(query, candidate)
            .unwrap_or_else(|| panic!("`{query}` should match `{candidate}`"))
            .score
    }

    #[test]
    fn an_empty_query_matches_everything_without_reordering() {
        assert_eq!(
            match_query("", "main:1 editor"),
            Some(Match {
                score: 0,
                positions: Vec::new()
            })
        );
    }

    #[test]
    fn a_query_that_is_not_a_subsequence_does_not_match() {
        assert_eq!(match_query("zz", "main:1 editor"), None);
        // Order matters: the characters are all there, backwards.
        assert_eq!(match_query("de", "ed"), None);
        // One `n` cannot serve two.
        assert_eq!(match_query("nn", "main"), None);
    }

    #[test]
    fn matching_is_case_insensitive_but_prefers_the_exact_case() {
        assert!(match_query("ed", "EDITOR").is_some());
        assert!(
            score_of("ed", "editor") > score_of("ed", "EDitor"),
            "same positions, so the case bonus is the only difference"
        );
    }

    #[test]
    fn positions_point_at_the_matched_characters() {
        let matched = match_query("mn", "main").expect("m…n is in main");

        assert_eq!(matched.positions, vec![0, 3]);
    }

    #[test]
    fn a_run_at_a_word_start_beats_scattered_letters() {
        // The picker's whole job: typing `dev` should find the dev-server row,
        // not a row that merely happens to contain d, e and v in order.
        assert!(score_of("dev", "main:2 dev-server") > score_of("dev", "deploy-review"));
    }

    #[test]
    fn a_prefix_beats_a_match_buried_further_in() {
        assert!(score_of("main", "main") > score_of("main", "scratch:1 main"));
    }

    #[test]
    fn a_match_after_a_separator_beats_one_mid_word() {
        assert!(score_of("s", "dev-server") > score_of("s", "passthrough"));
    }

    #[test]
    fn a_consecutive_run_beats_the_same_letters_spread_out() {
        assert!(score_of("abc", "xabc") > score_of("abc", "xaxbxc"));
    }

    #[test]
    fn positions_are_always_ascending() {
        for candidate in ["a b c a", "aabbcc", "cba-abc", "abcabcabc"] {
            let matched =
                match_query("abc", candidate).unwrap_or_else(|| panic!("a…b…c is in {candidate}"));

            assert!(
                matched.positions.windows(2).all(|pair| pair[0] < pair[1]),
                "positions must stay ascending for {candidate}: {:?}",
                matched.positions
            );
        }
    }

    #[test]
    fn whitespace_in_the_query_is_ignored() {
        assert_eq!(
            match_query("de v", "dev-server"),
            match_query("dev", "dev-server")
        );
    }

    #[test]
    fn a_query_longer_than_the_candidate_cannot_match() {
        assert_eq!(match_query("mainmain", "main"), None);
    }
}
