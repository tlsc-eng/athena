/// Case-insensitive subsequence match; higher scores favour word starts and runs of adjacent hits.
///
/// Every place the first query character occurs is tried as a start, so `main` finds `cmd/main.go`
/// rather than settling for the `m` in `cmd`.
pub fn score(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| *c != ' ')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let cand: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = cand.iter().map(|c| c.to_ascii_lowercase()).collect();
    let name_start = candidate
        .rfind('/')
        .map_or(0, |i| candidate[..=i].chars().count());
    (0..lower.len())
        .filter(|&i| lower[i] == query[0])
        .filter_map(|start| greedy(&query, &cand, &lower, start, name_start))
        .max_by_key(|(s, _)| *s)
}

fn greedy(
    query: &[char],
    cand: &[char],
    lower: &[char],
    start: usize,
    name_start: usize,
) -> Option<(i32, Vec<usize>)> {
    let mut positions = Vec::with_capacity(query.len());
    let mut score = 0;
    let mut from = start;
    let mut prev: Option<usize> = None;
    for &q in query {
        let i = (from..lower.len()).find(|&i| lower[i] == q)?;
        score += 16;
        if prev.is_some_and(|p| p + 1 == i) {
            score += 12;
        }
        let boundary = i == 0
            || matches!(cand[i - 1], '/' | '_' | '-' | '.' | ' ')
            || (cand[i].is_uppercase() && cand[i - 1].is_lowercase());
        if boundary {
            score += 10;
        }
        score -= (i - prev.map_or(0, |p| p + 1)).min(8) as i32;
        positions.push(i);
        prev = Some(i);
        from = i + 1;
    }
    // Prefer hits in the file name over hits in its directories, and shorter paths overall.
    score += 2 * positions.iter().filter(|&&p| p >= name_start).count() as i32;
    score -= cand.len() as i32 / 8;
    Some((score, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank<'a>(q: &str, items: &[&'a str]) -> Vec<&'a str> {
        let mut hits: Vec<_> = items
            .iter()
            .filter_map(|c| score(q, c).map(|(s, _)| (s, *c)))
            .collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.0));
        hits.into_iter().map(|(_, c)| c).collect()
    }

    #[test]
    fn subsequence_required() {
        assert!(score("mgo", "main.go").is_some());
        assert!(score("xyz", "main.go").is_none());
    }

    #[test]
    fn prefers_file_names_and_word_starts() {
        let items = [
            "internal/server/handler.go",
            "cmd/main.go",
            "docs/maintenance.md",
        ];
        assert_eq!(rank("main", &items)[0], "cmd/main.go");
        assert_eq!(
            rank("sh", &["src/shell.rs", "internal/server/handler.go"])[0],
            "src/shell.rs"
        );
    }

    #[test]
    fn reports_match_positions() {
        assert_eq!(score("mg", "main.go").unwrap().1, vec![0, 5]);
    }
}
