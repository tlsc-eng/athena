//! Line and word diffs (Myers, linear space), and their alignment into rows for a diff view.

use std::collections::HashMap;
use std::hash::Hash;
use std::ops::Range;

/// Edit cost after which a bisection stops looking for the optimal split, as git's xdiff does.
const MIN_COST_LIMIT: usize = 256;
/// Lines longer than this are compared whole, not word by word.
const MAX_WORD_DIFF_LINE: usize = 2_000;

/// A block of lines that differ: `old` lines were replaced by `new` lines; either may be empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// Lines with their terminators kept, so joining them gives the text back byte for byte.
pub fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A line without its `\n` or `\r\n`, as it is drawn.
pub fn display(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// The changed blocks between two texts' lines, in order.
pub fn diff_lines(old: &[&str], new: &[&str]) -> Vec<Change> {
    let (a, b) = intern(old, new);
    changes(&a, &b)
}

/// Byte ranges that differ inside a pair of changed lines, on word boundaries.
pub fn diff_words(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (old, new) = (display(old), display(new));
    if old.len() > MAX_WORD_DIFF_LINE || new.len() > MAX_WORD_DIFF_LINE {
        return (
            std::iter::once(0..old.len()).collect(),
            std::iter::once(0..new.len()).collect(),
        );
    }
    let (ta, tb) = (words(old), words(new));
    let sa: Vec<&str> = ta.iter().map(|r| &old[r.clone()]).collect();
    let sb: Vec<&str> = tb.iter().map(|r| &new[r.clone()]).collect();
    let (a, b) = intern(&sa, &sb);
    let mut out = (Vec::new(), Vec::new());
    for c in changes(&a, &b) {
        if !c.old.is_empty() {
            push_merged(&mut out.0, ta[c.old.start].start..ta[c.old.end - 1].end);
        }
        if !c.new.is_empty() {
            push_merged(&mut out.1, tb[c.new.start].start..tb[c.new.end - 1].end);
        }
    }
    out
}

/// The old text with one change taken from the new one, as staging a hunk does.
pub fn apply_change(old: &[&str], new: &[&str], change: &Change) -> String {
    let mut out = old[..change.old.start].concat();
    out.push_str(&new[change.new.clone()].concat());
    out.push_str(&old[change.old.end..].concat());
    out
}

/// The new text with one change undone, as reverting a hunk does.
pub fn revert_change(old: &[&str], new: &[&str], change: &Change) -> String {
    let mut out = new[..change.new.start].concat();
    out.push_str(&old[change.old.clone()].concat());
    out.push_str(&new[change.new.end..].concat());
    out
}

/// One row of a diff view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The bar above a change that holds its actions.
    Header(usize),
    /// An old line, a new line, or both side by side; `change` is set inside a changed block.
    Line {
        old: Option<usize>,
        new: Option<usize>,
        change: Option<usize>,
    },
}

/// Rows with old and new lines side by side; a block's shorter side is padded with blanks.
pub fn side_by_side(changes: &[Change], old_len: usize, new_len: usize) -> Vec<Row> {
    align(changes, old_len, new_len, |rows, i, c| {
        for k in 0..c.old.len().max(c.new.len()) {
            rows.push(Row::Line {
                old: (k < c.old.len()).then(|| c.old.start + k),
                new: (k < c.new.len()).then(|| c.new.start + k),
                change: Some(i),
            });
        }
    })
}

/// Rows in one column: a block's old lines, then its new lines.
pub fn inline(changes: &[Change], old_len: usize, new_len: usize) -> Vec<Row> {
    align(changes, old_len, new_len, |rows, i, c| {
        rows.extend(c.old.clone().map(|o| Row::Line {
            old: Some(o),
            new: None,
            change: Some(i),
        }));
        rows.extend(c.new.clone().map(|n| Row::Line {
            old: None,
            new: Some(n),
            change: Some(i),
        }));
    })
}

fn align(
    changes: &[Change],
    old_len: usize,
    new_len: usize,
    block: impl Fn(&mut Vec<Row>, usize, &Change),
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(old_len.max(new_len) + changes.len());
    let (mut o, mut n) = (0, 0);
    let same = |rows: &mut Vec<Row>, o: &mut usize, n: &mut usize, until: usize| {
        while *o < until {
            rows.push(Row::Line {
                old: Some(*o),
                new: Some(*n),
                change: None,
            });
            *o += 1;
            *n += 1;
        }
    };
    for (i, c) in changes.iter().enumerate() {
        same(&mut rows, &mut o, &mut n, c.old.start);
        rows.push(Row::Header(i));
        block(&mut rows, i, c);
        (o, n) = (c.old.end, c.new.end);
    }
    same(&mut rows, &mut o, &mut n, old_len);
    debug_assert_eq!(n, new_len);
    rows
}

fn push_merged(out: &mut Vec<Range<usize>>, r: Range<usize>) {
    match out.last_mut() {
        Some(last) if last.end >= r.start => last.end = last.end.max(r.end),
        _ => out.push(r),
    }
}

/// Words, runs of spaces, and single other characters, as byte ranges.
fn words(s: &str) -> Vec<Range<usize>> {
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            2
        } else {
            3
        }
    };
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut prev = 0;
    for (i, c) in s.char_indices() {
        let k = class(c);
        match out.last_mut() {
            Some(last) if k == prev && k != 3 => last.end = i + c.len_utf8(),
            _ => out.push(i..i + c.len_utf8()),
        }
        prev = k;
    }
    out
}

/// Numbers equal items alike, so comparing them is one integer compare.
fn intern<T: Hash + Eq>(a: &[T], b: &[T]) -> (Vec<u32>, Vec<u32>) {
    let mut ids: HashMap<&T, u32> = HashMap::with_capacity(a.len() + b.len());
    let mut id = |x| {
        let next = ids.len() as u32;
        *ids.entry(x).or_insert(next)
    };
    (
        a.iter().map(&mut id).collect(),
        b.iter().map(&mut id).collect(),
    )
}

/// Changed blocks between `a` and `b`, from the common runs a Myers bisection finds.
fn changes(a: &[u32], b: &[u32]) -> Vec<Change> {
    let mut equal = Vec::new();
    // An explicit stack: heuristic splits can nest deeper than the thread's stack allows.
    let mut todo = vec![(0..a.len(), 0..b.len())];
    while let Some((mut ra, mut rb)) = todo.pop() {
        let prefix = common_prefix(&a[ra.clone()], &b[rb.clone()]);
        if prefix > 0 {
            equal.push((ra.start, rb.start, prefix));
            ra.start += prefix;
            rb.start += prefix;
        }
        let suffix = common_suffix(&a[ra.clone()], &b[rb.clone()]);
        if suffix > 0 {
            equal.push((ra.end - suffix, rb.end - suffix, suffix));
            ra.end -= suffix;
            rb.end -= suffix;
        }
        if ra.is_empty() || rb.is_empty() {
            continue;
        }
        if let Some((x, y)) = bisect(&a[ra.clone()], &b[rb.clone()]) {
            todo.push((ra.start + x..ra.end, rb.start + y..rb.end));
            todo.push((ra.start..ra.start + x, rb.start..rb.start + y));
        }
    }
    equal.sort_unstable();
    let mut out = Vec::new();
    let (mut o, mut n) = (0, 0);
    for (ea, eb, len) in equal.into_iter().chain([(a.len(), b.len(), 0)]) {
        if ea > o || eb > n {
            out.push(Change {
                old: o..ea,
                new: n..eb,
            });
        }
        (o, n) = (ea + len, eb + len);
    }
    out
}

fn common_prefix(a: &[u32], b: &[u32]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn common_suffix(a: &[u32], b: &[u32]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

/// A point on an optimal edit path splitting the problem in two (Myers' middle snake). Past the
/// cost limit it returns the furthest point reached instead; `None` means "replace it all".
fn bisect(a: &[u32], b: &[u32]) -> Option<(usize, usize)> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max_d = (n + m + 1) / 2;
    let limit = MIN_COST_LIMIT.max(((n + m) as f64).sqrt() as usize) as isize;
    let offset = max_d + 1;
    let len = (2 * offset + 1) as usize;
    let mut v1 = vec![-1isize; len];
    let mut v2 = vec![-1isize; len];
    v1[(offset + 1) as usize] = 0;
    v2[(offset + 1) as usize] = 0;
    let delta = n - m;
    let front = delta % 2 != 0;
    let (mut k1start, mut k1end, mut k2start, mut k2end) = (0, 0, 0, 0);
    for d in 0..max_d.min(limit) {
        let mut k1 = -d + k1start;
        while k1 <= d - k1end {
            let i = (offset + k1) as usize;
            let mut x1 = if k1 == -d || (k1 != d && v1[i - 1] < v1[i + 1]) {
                v1[i + 1]
            } else {
                v1[i - 1] + 1
            };
            let mut y1 = x1 - k1;
            while x1 < n && y1 < m && a[x1 as usize] == b[y1 as usize] {
                x1 += 1;
                y1 += 1;
            }
            v1[i] = x1;
            if x1 > n {
                k1end += 2;
            } else if y1 > m {
                k1start += 2;
            } else if front {
                let j = offset + delta - k1;
                if j >= 0 && (j as usize) < len && v2[j as usize] != -1 && x1 >= n - v2[j as usize]
                {
                    return Some((x1 as usize, y1 as usize));
                }
            }
            k1 += 2;
        }
        let mut k2 = -d + k2start;
        while k2 <= d - k2end {
            let i = (offset + k2) as usize;
            let mut x2 = if k2 == -d || (k2 != d && v2[i - 1] < v2[i + 1]) {
                v2[i + 1]
            } else {
                v2[i - 1] + 1
            };
            let mut y2 = x2 - k2;
            while x2 < n && y2 < m && a[(n - x2 - 1) as usize] == b[(m - y2 - 1) as usize] {
                x2 += 1;
                y2 += 1;
            }
            v2[i] = x2;
            if x2 > n {
                k2end += 2;
            } else if y2 > m {
                k2start += 2;
            } else if !front {
                let j = offset + delta - k2;
                if j >= 0 && (j as usize) < len && v1[j as usize] != -1 {
                    let x1 = v1[j as usize];
                    let y1 = offset + x1 - j;
                    if x1 >= n - x2 {
                        return Some((x1 as usize, y1 as usize));
                    }
                }
            }
            k2 += 2;
        }
    }
    furthest(&v1, offset, n, m)
}

/// The forward point closest to the end, used when the optimal split costs too much to find.
fn furthest(v1: &[isize], offset: isize, n: isize, m: isize) -> Option<(usize, usize)> {
    let (x, y) = v1
        .iter()
        .enumerate()
        .filter(|(_, x)| **x >= 0)
        .map(|(i, &x)| (x, x - (i as isize - offset)))
        .filter(|&(x, y)| x <= n && (0..=m).contains(&y))
        .max_by_key(|&(x, y)| x + y)?;
    (x + y > 0 && x + y < n + m).then_some((x as usize, y as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(old: &str, new: &str) -> Vec<Change> {
        diff_lines(&lines(old), &lines(new))
    }

    fn ch(old: Range<usize>, new: Range<usize>) -> Change {
        Change { old, new }
    }

    /// Applying every change to the old text must give the new text.
    fn round_trips(old: &str, new: &str) {
        let (a, b) = (lines(old), lines(new));
        let changes = diff_lines(&a, &b);
        let mut text = String::new();
        let mut o = 0;
        for c in &changes {
            text.push_str(&a[o..c.old.start].concat());
            text.push_str(&b[c.new.clone()].concat());
            o = c.old.end;
        }
        text.push_str(&a[o..].concat());
        assert_eq!(text, new);
    }

    #[test]
    fn an_inserted_line_is_one_change_with_no_old_lines() {
        assert_eq!(diff("a\nb\nc\n", "a\nb\nx\nc\n"), vec![ch(2..2, 2..3)]);
        assert_eq!(diff("", "a\n"), vec![ch(0..0, 0..1)]);
    }

    #[test]
    fn a_deleted_line_is_one_change_with_no_new_lines() {
        assert_eq!(diff("a\nb\nc\n", "a\nc\n"), vec![ch(1..2, 1..1)]);
        assert_eq!(diff("a\n", ""), vec![ch(0..1, 0..0)]);
    }

    #[test]
    fn a_replaced_line_pairs_old_and_new() {
        assert_eq!(diff("a\nb\nc\n", "a\nB\nc\n"), vec![ch(1..2, 1..2)]);
        assert_eq!(
            diff("1\n2\n3\n4\n5\n6\n", "1\nx\n3\n4\ny\nz\n6\n"),
            vec![ch(1..2, 1..2), ch(4..5, 4..6)]
        );
    }

    #[test]
    fn identical_texts_have_no_changes() {
        assert!(diff("a\nb\n", "a\nb\n").is_empty());
        assert!(diff("", "").is_empty());
    }

    #[test]
    fn a_missing_final_newline_is_a_change_to_the_last_line() {
        assert_eq!(diff("a\nb\n", "a\nb"), vec![ch(1..2, 1..2)]);
        round_trips("a\nb\n", "a\nb");
    }

    #[test]
    fn crlf_lines_compare_whole_and_display_without_the_terminator() {
        assert!(diff("a\r\nb\r\n", "a\r\nb\r\n").is_empty());
        assert_eq!(diff("a\r\nb\r\n", "a\nb\r\n"), vec![ch(0..1, 0..1)]);
        assert_eq!(display("x\r\n"), "x");
        assert_eq!(display("x\n"), "x");
        assert_eq!(display("x"), "x");
        round_trips("a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n");
    }

    #[test]
    fn unicode_word_ranges_fall_on_character_boundaries() {
        let (old, new) = diff_words("let café = \"naïve\";\n", "let café = \"naïf 🎉\";\n");
        let o = "let café = \"naïve\";";
        let n = "let café = \"naïf 🎉\";";
        for r in old.iter() {
            assert!(o.is_char_boundary(r.start) && o.is_char_boundary(r.end));
        }
        for r in new.iter() {
            assert!(n.is_char_boundary(r.start) && n.is_char_boundary(r.end));
        }
        assert_eq!(
            old.iter().map(|r| &o[r.clone()]).collect::<String>(),
            "naïve"
        );
        assert_eq!(
            new.iter().map(|r| &n[r.clone()]).collect::<String>(),
            "naïf 🎉"
        );
        round_trips("α\nβ\nγ\n", "α\nβ́\nγ\nδ\n");
    }

    #[test]
    fn word_diff_marks_only_the_changed_words() {
        let (old, new) = diff_words("fmt.Println(a, b)\n", "fmt.Printf(a, c)\n");
        let o = "fmt.Println(a, b)";
        let n = "fmt.Printf(a, c)";
        let pick = |s: &str, rs: &[Range<usize>]| -> Vec<String> {
            rs.iter().map(|r| s[r.clone()].to_string()).collect()
        };
        assert_eq!(pick(o, &old), vec!["Println", "b"]);
        assert_eq!(pick(n, &new), vec!["Printf", "c"]);
    }

    #[test]
    fn rows_pad_the_shorter_side_and_put_a_header_above_each_change() {
        let changes = vec![ch(1..2, 1..3)];
        let rows = side_by_side(&changes, 3, 4);
        let line = |old, new, change| Row::Line { old, new, change };
        assert_eq!(
            rows,
            vec![
                line(Some(0), Some(0), None),
                Row::Header(0),
                line(Some(1), Some(1), Some(0)),
                line(None, Some(2), Some(0)),
                line(Some(2), Some(3), None),
            ]
        );
        let rows = inline(&changes, 3, 4);
        assert_eq!(
            rows,
            vec![
                line(Some(0), Some(0), None),
                Row::Header(0),
                line(Some(1), None, Some(0)),
                line(None, Some(1), Some(0)),
                line(None, Some(2), Some(0)),
                line(Some(2), Some(3), None),
            ]
        );
    }

    #[test]
    fn applying_and_reverting_one_change_leaves_the_others() {
        let (old, new) = ("a\nb\nc\nd\n", "a\nB\nc\nD\ne\n");
        let (a, b) = (lines(old), lines(new));
        let changes = diff_lines(&a, &b);
        assert_eq!(changes.len(), 2);
        assert_eq!(apply_change(&a, &b, &changes[0]), "a\nB\nc\nd\n");
        assert_eq!(revert_change(&a, &b, &changes[1]), "a\nB\nc\nd\n");
        assert_eq!(apply_change(&a, &b, &changes[1]), "a\nb\nc\nD\ne\n");
    }

    #[test]
    fn random_edits_round_trip_with_a_minimal_edit_count() {
        let mut seed = 0x2545_f491_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let old: Vec<String> = (0..next() % 40)
                .map(|_| format!("{}\n", next() % 6))
                .collect();
            let mut new = old.clone();
            for _ in 0..next() % 8 {
                let at = (next() as usize) % (new.len() + 1);
                match next() % 3 {
                    0 => new.insert(at, format!("{}\n", next() % 6)),
                    1 if at < new.len() => drop(new.remove(at)),
                    _ if at < new.len() => new[at] = format!("{}\n", next() % 6),
                    _ => {}
                }
            }
            round_trips(&old.concat(), &new.concat());
            let changed: usize = diff(&old.concat(), &new.concat())
                .iter()
                .map(|c| c.old.len() + c.new.len())
                .sum();
            assert_eq!(changed, old.len() + new.len() - 2 * lcs(&old, &new));
        }
    }

    fn lcs(a: &[String], b: &[String]) -> usize {
        let mut t = vec![vec![0; b.len() + 1]; a.len() + 1];
        for i in 0..a.len() {
            for j in 0..b.len() {
                t[i + 1][j + 1] = if a[i] == b[j] {
                    t[i][j] + 1
                } else {
                    t[i][j + 1].max(t[i + 1][j])
                };
            }
        }
        t[a.len()][b.len()]
    }

    #[test]
    fn fifty_thousand_lines_diff_quickly() {
        let old: String = (0..50_000).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..50_000)
            .map(|i| {
                if i % 97 == 0 {
                    format!("changed {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        let started = std::time::Instant::now();
        let changes = diff(&old, &new);
        assert_eq!(changes.len(), 50_000usize.div_ceil(97));
        let unrelated: String = (0..50_000).map(|i| format!("other {i}\n")).collect();
        round_trips(&old, &unrelated);
        let elapsed = started.elapsed();
        // Debug builds are slow; this catches quadratic blow-ups, not small regressions.
        assert!(elapsed.as_secs() < 5, "took {elapsed:?}");
    }
}
