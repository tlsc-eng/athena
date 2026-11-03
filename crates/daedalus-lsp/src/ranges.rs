use serde_json::Value;

use crate::protocol::Range;

/// Ranges that are edited together, such as a JSX element's opening and closing tag names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkedRanges {
    pub ranges: Vec<Range>,
    /// A regex the text of every range must keep matching for the edit to stay linked.
    pub word_pattern: Option<String>,
}

/// Each `SelectionRange` of a reply as its chain of ranges, innermost first.
pub(crate) fn parse_selection_ranges(result: &Value) -> Vec<Vec<Range>> {
    result
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|mut node| {
            let mut chain: Vec<Range> = Vec::new();
            while let Some(range) = node
                .get("range")
                .and_then(|r| serde_json::from_value::<Range>(r.clone()).ok())
            {
                // A parent must contain its child; a server breaking that ends the chain.
                if chain
                    .last()
                    .is_some_and(|inner| range.start > inner.start || range.end < inner.end)
                {
                    break;
                }
                chain.push(range);
                match node.get("parent") {
                    Some(parent) if parent.is_object() => node = parent,
                    _ => break,
                }
            }
            chain.dedup();
            chain
        })
        .collect()
}

pub(crate) fn parse_linked_ranges(result: &Value) -> LinkedRanges {
    LinkedRanges {
        ranges: result
            .get("ranges")
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default(),
        word_pattern: result
            .get("wordPattern")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn range(a: (u32, u32), b: (u32, u32)) -> Value {
        json!({"start": {"line": a.0, "character": a.1}, "end": {"line": b.0, "character": b.1}})
    }

    #[test]
    fn selection_ranges_unroll_innermost_first_and_stop_at_a_parent_that_does_not_contain() {
        let reply = json!([
            {"range": range((2, 4), (2, 9)), "parent": {"range": range((2, 0), (2, 20)),
                "parent": {"range": range((0, 0), (5, 0)), "parent": {"range": range((2, 5), (2, 6))}}}},
            {"range": range((1, 0), (1, 0))},
            {"nope": 1}
        ]);
        let chains = parse_selection_ranges(&reply);
        assert_eq!(chains.len(), 3);
        let lines: Vec<(u32, u32, u32, u32)> = chains[0]
            .iter()
            .map(|r| (r.start.line, r.start.character, r.end.line, r.end.character))
            .collect();
        assert_eq!(lines, [(2, 4, 2, 9), (2, 0, 2, 20), (0, 0, 5, 0)]);
        assert_eq!(chains[1].len(), 1);
        assert!(chains[2].is_empty());
        assert!(parse_selection_ranges(&json!(null)).is_empty());
    }

    #[test]
    fn linked_ranges_read_their_word_pattern() {
        let linked = parse_linked_ranges(&json!({
            "ranges": [range((0, 1), (0, 4)), range((0, 10), (0, 13))],
            "wordPattern": "[a-z]+"
        }));
        assert_eq!(linked.ranges.len(), 2);
        assert_eq!(linked.ranges[1].start.character, 10);
        assert_eq!(linked.word_pattern.as_deref(), Some("[a-z]+"));
        assert_eq!(parse_linked_ranges(&json!(null)), LinkedRanges::default());
    }
}
