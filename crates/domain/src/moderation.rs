//! Pure deterministic comment moderation primitives.

use sha2::{Digest, Sha256};
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModerationRules {
    pub max_comment_len_chars: u32,
    pub max_links: u8,
    pub max_comments_per_window: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Screen {
    Visible,
    Shadow(&'static str),
    Blocked(&'static str),
}

fn retained_control(character: char) -> bool {
    matches!(character, '\n' | '\t')
}

fn removed_character(character: char) -> bool {
    matches!(get_general_category(character), GeneralCategory::Format)
        || (character.is_control() && !retained_control(character))
}

#[must_use]
pub fn normalize_body(body: &str) -> String {
    let mut normalized = String::new();
    let mut pending_space = false;
    for character in body
        .nfkc()
        .flat_map(char::to_lowercase)
        .filter(|character| !removed_character(*character))
    {
        if character.is_whitespace() {
            pending_space = !normalized.is_empty();
        } else {
            if pending_space {
                normalized.push(' ');
                pending_space = false;
            }
            normalized.push(character);
        }
    }
    normalized
}

#[must_use]
pub fn sanitize_for_storage(body: &str) -> String {
    body.chars()
        .filter(|character| !removed_character(*character))
        .collect()
}

#[must_use]
pub fn body_hash(body: &str) -> String {
    format!("{:x}", Sha256::digest(normalize_body(body).as_bytes()))
}

#[must_use]
pub fn link_count(normalized: &str) -> u32 {
    let links =
        normalized.match_indices("http://").count() + normalized.match_indices("https://").count();
    u32::try_from(links).unwrap_or(u32::MAX)
}

#[must_use]
pub fn screen(
    body: &str,
    recent_same_hash: u32,
    author_posts_in_window: u32,
    rules: &ModerationRules,
) -> Screen {
    let normalized = normalize_body(body);
    if normalized.is_empty() {
        return Screen::Blocked("empty");
    }
    if u32::try_from(body.chars().count()).unwrap_or(u32::MAX) > rules.max_comment_len_chars {
        return Screen::Blocked("too_long");
    }
    if author_posts_in_window >= rules.max_comments_per_window {
        return Screen::Blocked("rate");
    }
    if link_count(&normalized) > u32::from(rules.max_links) {
        return Screen::Shadow("links");
    }
    if recent_same_hash > 0 {
        return Screen::Shadow("duplicate");
    }
    Screen::Visible
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> ModerationRules {
        ModerationRules {
            max_comment_len_chars: 32,
            max_links: 1,
            max_comments_per_window: 2,
        }
    }

    #[test]
    fn canonical_hash_vectors_collide() {
        let expected = body_hash("hello");
        assert!(!expected.is_empty());
        for equivalent in ["hello ", "hello\u{200b}", "HELLO", "ℌELLO"] {
            assert_eq!(body_hash(equivalent), expected);
        }
        assert_eq!(normalize_body("  A\n\tB  "), "a b");
        assert_eq!(sanitize_for_storage("A\u{200b}\u{0}B\n"), "AB\n");
    }

    #[test]
    fn blocked_rules_precede_shadow_rules() {
        assert_eq!(
            screen(" \u{200b} ", 0, 0, &rules()),
            Screen::Blocked("empty")
        );
        assert_eq!(
            screen("123456789012345678901234567890123", 0, 0, &rules()),
            Screen::Blocked("too_long")
        );
        assert_eq!(screen("https://a", 1, 2, &rules()), Screen::Blocked("rate"));
    }

    #[test]
    fn links_duplicates_and_visible_text_are_distinct() {
        assert_eq!(link_count("http://a https://b"), 2);
        assert_eq!(
            screen("http://a https://b", 0, 0, &rules()),
            Screen::Shadow("links")
        );
        assert_eq!(screen("hello", 1, 0, &rules()), Screen::Shadow("duplicate"));
        assert_eq!(screen("hello", 0, 0, &rules()), Screen::Visible);
    }

    #[test]
    fn scalar_length_is_not_utf8_byte_length() {
        let unicode = ModerationRules {
            max_comment_len_chars: 2,
            ..rules()
        };
        assert_eq!(screen("éé", 0, 0, &unicode), Screen::Visible);
        assert_eq!(screen("ééé", 0, 0, &unicode), Screen::Blocked("too_long"));
    }
}
