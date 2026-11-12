//! Budget fitting: keep the most relevant whole items that fit; never cut
//! inside an item.

use hm_proto::budget::estimate_tokens;

/// One output line competing for the budget.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    /// Higher = more relevant; dropped last.
    pub score: u32,
    /// Tie-break among equal scores: higher = newer = kept first.
    pub recency: u64,
    /// Output position (lower = earlier in the text).
    pub pos: u64,
    pub line: String,
}

/// Result of [`fit`].
#[derive(Debug, Clone)]
pub(crate) struct Fitted {
    pub text: String,
    pub shown: usize,
    pub omitted: usize,
}

fn render(head: &[String], kept: &[&Item], omitted: usize) -> String {
    let mut lines: Vec<&str> = head.iter().map(String::as_str).collect();
    let mut kept: Vec<&&Item> = kept.iter().collect();
    kept.sort_by_key(|i| i.pos);
    lines.extend(kept.iter().map(|i| i.line.as_str()));
    let footer;
    if omitted > 0 {
        footer = format!("(+{omitted} less relevant omitted)");
        lines.push(&footer);
    }
    lines.join("\n")
}

/// Head lines always come first (dropped from the end only if they alone
/// exceed `max_tokens`); then items are taken in relevance order until the
/// next one would not fit. Output order is by `pos`.
pub(crate) fn fit(head: &[String], mut items: Vec<Item>, max_tokens: usize) -> Fitted {
    let mut head = head.to_vec();
    while !head.is_empty() && estimate_tokens(&head.join("\n")) > max_tokens {
        head.pop();
    }
    items.sort_by(|a, b| (b.score, b.recency, a.pos).cmp(&(a.score, a.recency, b.pos)));
    let total = items.len();
    let mut kept: Vec<&Item> = Vec::new();
    for item in &items {
        kept.push(item);
        let text = render(&head, &kept, total - kept.len());
        if estimate_tokens(&text) > max_tokens {
            kept.pop();
            break;
        }
    }
    // The footer itself may not fit next to the head: drop it then.
    let mut text = render(&head, &kept, total - kept.len());
    if estimate_tokens(&text) > max_tokens {
        text = render(&head, &kept, 0);
    }
    Fitted {
        text,
        shown: kept.len(),
        omitted: total - kept.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(score: u32, pos: u64, line: &str) -> Item {
        Item {
            score,
            recency: 0,
            pos,
            line: line.to_owned(),
        }
    }

    #[test]
    fn drops_least_relevant_first_and_keeps_position_order() {
        let items = vec![
            item(10, 0, "low relevance line that is fairly long indeed"),
            item(90, 2, "most relevant"),
            item(50, 1, "middle relevance"),
        ];
        let all = fit(&["head".into()], items.clone(), 1000);
        assert_eq!(all.shown, 3);
        assert_eq!(
            all.text,
            "head\nlow relevance line that is fairly long indeed\nmiddle relevance\nmost relevant"
        );
        let budget =
            estimate_tokens("head\nmiddle relevance\nmost relevant\n(+1 less relevant omitted)");
        let some = fit(&["head".into()], items, budget);
        assert_eq!(some.shown, 2);
        assert_eq!(some.omitted, 1);
        assert!(!some.text.contains("low relevance"));
        assert!(estimate_tokens(&some.text) <= budget);
    }

    #[test]
    fn never_exceeds_budget() {
        let items: Vec<Item> = (0..50).map(|i| item(i, u64::from(i), "x y z w")).collect();
        for max in [0, 1, 3, 10, 40] {
            let f = fit(&["a header line".into()], items.clone(), max);
            assert!(estimate_tokens(&f.text) <= max, "{max}: {}", f.text);
        }
    }
}
