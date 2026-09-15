//! Deterministic extraction of bounded ASCII-style handles.

use std::collections::HashSet;

#[must_use]
pub fn parse_mentions(body: &str, max: u8) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let bytes = body.as_bytes();
    let mut in_code = false;
    let mut index = 0;
    let mut seen = HashSet::new();
    let mut mentions = Vec::new();
    while index < bytes.len() {
        if bytes[index] == b'`' {
            in_code = !in_code;
            index += 1;
            continue;
        }
        if in_code || bytes[index] != b'@' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let length = end - start;
        if (3..=32).contains(&length) {
            let handle = body[start..end].to_ascii_lowercase();
            if seen.insert(handle.clone()) {
                mentions.push(handle);
                if mentions.len() == usize::from(max) {
                    break;
                }
            }
        }
        index = end.max(index + 1);
    }
    mentions
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_normalizes_deduplicates_and_bounds() {
        assert_eq!(
            parse_mentions("@Alice hi @bob_2 and @ALICE then @charlie", 3),
            vec![
                "alice".to_string(),
                "bob_2".to_string(),
                "charlie".to_string()
            ]
        );
        assert_eq!(parse_mentions("@alice @bob @charlie", 2).len(), 2);
        assert!(parse_mentions("@ab @abcdefghijklmnopqrstuvwxyzabcdefghi @no-dash", 5).is_empty());
    }

    #[test]
    fn code_spans_and_zero_limit_are_ignored() {
        assert_eq!(
            parse_mentions("`@hidden` @shown ``` @also_hidden ``` @last", 8),
            vec!["shown".to_string(), "last".to_string()]
        );
        assert!(parse_mentions("@shown", 0).is_empty());
        assert!(parse_mentions("unmatched ` @hidden", 8).is_empty());
    }

    proptest! {
        #[test]
        fn adversarial_unicode_never_panics(body in any::<String>(), max in any::<u8>()) {
            let mentions = parse_mentions(&body, max);
            prop_assert!(mentions.len() <= usize::from(max));
        }
    }
}
