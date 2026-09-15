//! Stable, renderer-neutral v1 vocabulary (Task 5.2, codex M3 + grok B2).
//!
//! Determinism rules pinned here: `Vec`-ordered runs (no maps anywhere),
//! integer geometry, literal color strings, our own money/percent formatters
//! (fixed decimals), our own XML escape applied to EVERY text run at spec
//! build time (the renderer never escapes), fixed char-count wrapping, and no
//! wall clock. Byte identity of the rendered artifact is the contract;
//! rasterized appearance may vary by viewer fonts (generic families only,
//! nothing embedded).

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// `RenderSpec` vocabulary version; part of the share-card content address.
pub const SPEC_VERSION: u8 = 1;

const BG: &str = "#0B0B0F";
const INK: &str = "#FFFFFF";
const ACCENT: &str = "#7C5CFF";
const MUTED: &str = "#9A9AA5";
const GREEN: &str = "#2ECC71";
const RED: &str = "#E74C3C";
const BRAND: &str = "OPINIONS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRun {
    pub x: i32,
    pub y: i32,
    pub size: u16,
    pub color: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSpec {
    pub version: u8,
    pub width: u32,
    pub height: u32,
    pub background: String,
    pub runs: Vec<TextRun>,
}

/// Inputs for the pinned share-card fields (grok B2): handle, question, side,
/// payout, return percent, brand mark — no referral code this phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareCardInput<'a> {
    pub handle: &'a str,
    pub question: &'a str,
    pub side_yes: bool,
    pub payout_micro: i64,
    pub return_bps: i64,
}

/// Our own XML escape. Applied at spec build to every text run; adversarial
/// input such as `"</text><script>"` becomes inert escaped bytes.
#[must_use]
pub fn escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Our own money formatter: micro-USD to `$D.CC` with exactly two decimals,
/// truncating sub-cent amounts toward zero. No locale, no floats.
#[must_use]
pub fn format_micro_usd(micro: i64) -> String {
    let cents_total = micro.unsigned_abs() / 10_000;
    let sign = if micro < 0 { "-" } else { "" };
    format!("{sign}${}.{:02}", cents_total / 100, cents_total % 100)
}

/// Fixed one-decimal percent from basis points, explicit sign for gains.
#[must_use]
pub fn format_return_bps(bps: i64) -> String {
    let tenths = bps.unsigned_abs() / 10;
    let sign = if bps < 0 { "-" } else { "+" };
    format!("{sign}{}.{}%", tenths / 10, tenths % 10)
}

/// Fixed char-count greedy word wrap; words longer than `max_chars` are
/// hard-split. Char counting (not bytes) keeps multibyte text deterministic.
///
/// # Panics
/// Panics when `max_chars` is zero.
#[must_use]
pub fn wrap_chars(text: &str, max_chars: usize) -> Vec<String> {
    assert!(max_chars > 0, "wrap width must be positive");
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize;
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if word_len > max_chars {
            if line_len > 0 {
                lines.push(std::mem::take(&mut line));
                line_len = 0;
            }
            let chars: Vec<char> = word.chars().collect();
            for chunk in chars.chunks(max_chars) {
                if chunk.len() == max_chars {
                    lines.push(chunk.iter().collect());
                } else {
                    line = chunk.iter().collect();
                    line_len = chunk.len();
                }
            }
            continue;
        }
        if line_len == 0 {
            line = word.to_string();
            line_len = word_len;
        } else if line_len + 1 + word_len <= max_chars {
            line.push(' ');
            line.push_str(word);
            line_len += 1 + word_len;
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
            line_len = word_len;
        }
    }
    if line_len > 0 {
        lines.push(line);
    }
    lines
}

/// Lowercase hex SHA-256; used for artifact digests and content addresses.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn run(x: i32, y: i32, size: u16, color: &str, text: &str) -> TextRun {
    TextRun {
        x,
        y,
        size,
        color: color.to_string(),
        text: escape_text(text),
    }
}

/// Deterministic branded poster (kind `poster`).
#[must_use]
pub fn poster_spec(question: &str) -> RenderSpec {
    let mut runs = vec![run(64, 120, 40, ACCENT, BRAND)];
    for (index, line) in wrap_chars(question, 24).iter().take(8).enumerate() {
        let offset = i32::try_from(index).unwrap_or(i32::MAX - 300) * 90;
        runs.push(run(64, 300 + offset, 72, INK, line));
    }
    runs.push(run(64, 1290, 32, MUTED, "opinions.market"));
    RenderSpec {
        version: SPEC_VERSION,
        width: 1080,
        height: 1350,
        background: BG.to_string(),
        runs,
    }
}

/// Deterministic branded video artifact (kind `market_video`; poster-form
/// SVG this phase — motion pipelines are explicitly deferred).
#[must_use]
pub fn video_spec(question: &str) -> RenderSpec {
    let mut runs = vec![run(64, 160, 44, ACCENT, BRAND)];
    for (index, line) in wrap_chars(question, 22).iter().take(10).enumerate() {
        let offset = i32::try_from(index).unwrap_or(i32::MAX - 400) * 100;
        runs.push(run(64, 420 + offset, 80, INK, line));
    }
    runs.push(run(64, 1840, 36, MUTED, "opinions.market"));
    RenderSpec {
        version: SPEC_VERSION,
        width: 1080,
        height: 1920,
        background: BG.to_string(),
        runs,
    }
}

/// Deterministic share card with the pinned field set.
#[must_use]
pub fn share_card_spec(input: &ShareCardInput<'_>) -> RenderSpec {
    let mut runs = vec![
        run(64, 90, 36, ACCENT, BRAND),
        run(64, 170, 44, INK, &format!("@{}", input.handle)),
    ];
    for (index, line) in wrap_chars(input.question, 38).iter().take(3).enumerate() {
        let offset = i32::try_from(index).unwrap_or(i32::MAX - 250) * 52;
        runs.push(run(64, 250 + offset, 40, MUTED, line));
    }
    let (side, side_color) = if input.side_yes {
        ("YES", GREEN)
    } else {
        ("NO", RED)
    };
    runs.push(run(64, 470, 48, side_color, side));
    runs.push(run(
        64,
        540,
        40,
        INK,
        &format!("Payout {}", format_micro_usd(input.payout_micro)),
    ));
    runs.push(run(
        64,
        600,
        40,
        INK,
        &format!("Return {}", format_return_bps(input.return_bps)),
    ));
    RenderSpec {
        version: SPEC_VERSION,
        width: 1200,
        height: 630,
        background: BG.to_string(),
        runs,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn escape_neutralizes_adversarial_question() {
        assert_eq!(
            escape_text("</text><script>alert('x')</script>"),
            "&lt;/text&gt;&lt;script&gt;alert(&apos;x&apos;)&lt;/script&gt;"
        );
    }

    #[test]
    fn escape_handles_every_special_and_passes_plain_text() {
        assert_eq!(escape_text(r#"&<>"'"#), "&amp;&lt;&gt;&quot;&apos;");
        assert_eq!(escape_text("plain text 123"), "plain text 123");
        assert_eq!(escape_text(""), "");
    }

    proptest! {
        #[test]
        fn escape_output_is_inert(input in ".*") {
            let escaped = escape_text(&input);
            prop_assert!(!escaped.contains('<'));
            prop_assert!(!escaped.contains('>'));
            prop_assert!(!escaped.contains('"'));
            prop_assert!(!escaped.contains('\''));
            let bytes = escaped.as_bytes();
            for (index, byte) in bytes.iter().enumerate() {
                if *byte == b'&' {
                    let rest = &escaped[index..];
                    prop_assert!(
                        rest.starts_with("&amp;")
                            || rest.starts_with("&lt;")
                            || rest.starts_with("&gt;")
                            || rest.starts_with("&quot;")
                            || rest.starts_with("&apos;"),
                        "bare ampersand in {escaped:?}"
                    );
                }
            }
        }

        #[test]
        fn spec_builders_are_deterministic(question in ".{0,200}") {
            prop_assert_eq!(poster_spec(&question), poster_spec(&question));
            prop_assert_eq!(video_spec(&question), video_spec(&question));
        }
    }

    #[test]
    fn money_formatter_is_fixed_decimal_and_truncating() {
        assert_eq!(format_micro_usd(0), "$0.00");
        assert_eq!(format_micro_usd(1_234_567), "$1.23");
        assert_eq!(format_micro_usd(999), "$0.00");
        assert_eq!(format_micro_usd(19_999), "$0.01");
        assert_eq!(format_micro_usd(-50_000), "-$0.05");
        assert_eq!(format_micro_usd(100_000_000), "$100.00");
        assert_eq!(format_micro_usd(i64::MIN), "-$9223372036854.77");
    }

    #[test]
    fn return_formatter_pins_sign_and_one_decimal() {
        assert_eq!(format_return_bps(0), "+0.0%");
        assert_eq!(format_return_bps(1_234), "+12.3%");
        assert_eq!(format_return_bps(-10_000), "-100.0%");
        assert_eq!(format_return_bps(5), "+0.0%");
        assert_eq!(format_return_bps(i64::MIN), "-92233720368547758.0%");
    }

    #[test]
    fn wrapping_is_fixed_char_count_and_word_aware() {
        assert_eq!(wrap_chars("", 10), Vec::<String>::new());
        assert_eq!(wrap_chars("hello world", 5), vec!["hello", "world"]);
        assert_eq!(wrap_chars("hello world", 11), vec!["hello world"]);
        assert_eq!(wrap_chars("a  b", 10), vec!["a b"]);
        assert_eq!(wrap_chars("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(
            wrap_chars("x abcdefghij y", 4),
            vec!["x", "abcd", "efgh", "ij y"]
        );
        assert_eq!(wrap_chars("ééééé", 2), vec!["éé", "éé", "é"]);
        assert_eq!(wrap_chars("aaaa bb", 4), vec!["aaaa", "bb"]);
    }

    #[test]
    #[should_panic(expected = "wrap width must be positive")]
    fn wrapping_rejects_zero_width() {
        let _ = wrap_chars("x", 0);
    }

    #[test]
    fn poster_spec_pins_layout_and_escapes_at_build() {
        let spec = poster_spec("Will BTC close above $100k on Friday?");
        assert_eq!(spec.version, 1);
        assert_eq!((spec.width, spec.height), (1080, 1350));
        assert_eq!(spec.background, "#0B0B0F");
        assert_eq!(spec.runs[0].text, "OPINIONS");
        assert_eq!(spec.runs[0].color, "#7C5CFF");
        assert_eq!(spec.runs[1].text, "Will BTC close above");
        assert_eq!(
            (spec.runs[1].x, spec.runs[1].y, spec.runs[1].size),
            (64, 300, 72)
        );
        assert_eq!(spec.runs[2].text, "$100k on Friday?");
        assert_eq!(spec.runs[2].y, 390);
        assert_eq!(spec.runs.last().unwrap().text, "opinions.market");

        let hostile = poster_spec("</text><script>");
        assert!(hostile.runs.iter().all(|r| !r.text.contains('<')));
        assert_eq!(hostile.runs[1].text, "&lt;/text&gt;&lt;script&gt;");
    }

    #[test]
    fn poster_spec_caps_lines_deterministically() {
        let long = "word ".repeat(120);
        let spec = poster_spec(&long);
        // brand + at most 8 question lines + footer.
        assert_eq!(spec.runs.len(), 10);
        assert_eq!(spec.runs[8].y, 300 + 7 * 90);
    }

    #[test]
    fn video_spec_pins_dimensions_and_wrap() {
        let spec = video_spec("Will it rain tomorrow in NYC?");
        assert_eq!((spec.width, spec.height), (1080, 1920));
        assert_eq!(spec.version, 1);
        assert_eq!(spec.runs[1].text, "Will it rain tomorrow");
        assert_eq!(spec.runs[1].y, 420);
        assert_eq!(spec.runs[2].text, "in NYC?");
        assert_eq!(spec.runs[2].y, 520);
        let long = video_spec(&"word ".repeat(200));
        assert_eq!(long.runs.len(), 12);
    }

    #[test]
    fn share_card_spec_pins_fields_and_sides() {
        let input = ShareCardInput {
            handle: "alice",
            question: "Will BTC close above $100k?",
            side_yes: true,
            payout_micro: 12_500_000,
            return_bps: 2_150,
        };
        let spec = share_card_spec(&input);
        assert_eq!((spec.width, spec.height), (1200, 630));
        assert_eq!(spec.runs[1].text, "@alice");
        let texts: Vec<&str> = spec.runs.iter().map(|r| r.text.as_str()).collect();
        assert!(texts.contains(&"YES"));
        assert!(texts.contains(&"Payout $12.50"));
        assert!(texts.contains(&"Return +21.5%"));
        let yes_run = spec.runs.iter().find(|r| r.text == "YES").unwrap();
        assert_eq!(yes_run.color, "#2ECC71");

        let no_spec = share_card_spec(&ShareCardInput {
            side_yes: false,
            ..input.clone()
        });
        let no_run = no_spec.runs.iter().find(|r| r.text == "NO").unwrap();
        assert_eq!(no_run.color, "#E74C3C");

        let hostile = share_card_spec(&ShareCardInput {
            handle: "<b>bad</b>",
            question: "</text><script>",
            side_yes: false,
            payout_micro: 0,
            return_bps: -10_000,
        });
        assert!(hostile.runs.iter().all(|r| !r.text.contains('<')));
        // Long questions cap at three lines.
        let long = share_card_spec(&ShareCardInput {
            question: &"word ".repeat(60),
            ..input
        });
        assert_eq!(long.runs.len(), 2 + 3 + 3);
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }
}
