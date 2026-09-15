//! Deterministic SVG serialization (Task 5.2, codex M3 + grok B2).
//!
//! **Byte identity is the contract**: the same [`RenderSpec`] serializes to
//! the same bytes on every renderer instance, forever — attribute order is
//! the template literal order below, newlines are `\n`, geometry is integer,
//! colors are the spec's literal strings, and NO escaping happens here (the
//! spec builders escaped every text run at build time; this layer trusts the
//! spec by contract). Rasterized appearance may vary by viewer fonts — only
//! generic families are referenced, nothing is embedded.

use std::fmt::Write as _;

use application::model::RenderedArtifact;
use application::video::artifacts::SVG_MEDIA_TYPE;
use domain::render_spec::RenderSpec;

/// Serializes a spec to its canonical SVG byte form.
#[must_use]
pub(super) fn svg_bytes(spec: &RenderSpec) -> Vec<u8> {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" data-spec-version=\"{}\">",
        spec.width, spec.height, spec.width, spec.height, spec.version,
    );
    let _ = writeln!(
        out,
        "<rect width=\"{}\" height=\"{}\" fill=\"{}\"/>",
        spec.width, spec.height, spec.background,
    );
    for run in &spec.runs {
        let _ = writeln!(
            out,
            "<text x=\"{}\" y=\"{}\" font-family=\"sans-serif\" font-size=\"{}\" fill=\"{}\">{}</text>",
            run.x, run.y, run.size, run.color, run.text,
        );
    }
    out.push_str("</svg>\n");
    out.into_bytes()
}

pub(super) fn artifact(spec: &RenderSpec) -> RenderedArtifact {
    RenderedArtifact {
        bytes: svg_bytes(spec),
        media_type: SVG_MEDIA_TYPE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use domain::render_spec::{poster_spec, share_card_spec, video_spec, ShareCardInput};

    /// Golden byte + digest test across fresh renderer state: the exact
    /// serialized form is pinned, not just self-consistency.
    #[test]
    fn golden_bytes_and_digest_for_a_pinned_poster() {
        let spec = poster_spec("Will BTC close above $100k?");
        let bytes = svg_bytes(&spec);
        let expected = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1080\" height=\"1350\" viewBox=\"0 0 1080 1350\" data-spec-version=\"1\">\n\
             <rect width=\"1080\" height=\"1350\" fill=\"#0B0B0F\"/>\n\
             <text x=\"64\" y=\"120\" font-family=\"sans-serif\" font-size=\"40\" fill=\"#7C5CFF\">OPINIONS</text>\n\
             <text x=\"64\" y=\"300\" font-family=\"sans-serif\" font-size=\"72\" fill=\"#FFFFFF\">Will BTC close above</text>\n\
             <text x=\"64\" y=\"390\" font-family=\"sans-serif\" font-size=\"72\" fill=\"#FFFFFF\">$100k?</text>\n\
             <text x=\"64\" y=\"1290\" font-family=\"sans-serif\" font-size=\"32\" fill=\"#9A9AA5\">opinions.market</text>\n\
             </svg>\n";
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), expected);
        assert_eq!(
            domain::render_spec::sha256_hex(&bytes),
            domain::render_spec::sha256_hex(expected.as_bytes()),
        );
        // Fresh call, identical bytes: no hidden state, no wall clock.
        assert_eq!(
            svg_bytes(&poster_spec("Will BTC close above $100k?")),
            bytes
        );
    }

    #[test]
    fn adversarial_question_stays_inert_through_serialization() {
        let spec = poster_spec("</text><script>alert('x')</script>");
        let svg = String::from_utf8(svg_bytes(&spec)).unwrap();
        assert!(!svg.contains("<script"));
        assert!(svg.contains("&lt;/text&gt;&lt;script&gt;"));
        // The document structure survives: exactly one closing </svg> and no
        // premature </text> from the payload.
        assert_eq!(svg.matches("</svg>").count(), 1);
        assert_eq!(svg.matches("</text>").count(), spec.runs.len());
    }

    #[test]
    fn video_and_share_card_specs_serialize_deterministically() {
        let video = video_spec("Will it rain?");
        assert_eq!(svg_bytes(&video), svg_bytes(&video_spec("Will it rain?")));
        let card = share_card_spec(&ShareCardInput {
            handle: "alice",
            question: "Will it rain?",
            side_yes: true,
            payout_micro: 12_500_000,
            return_bps: 2_150,
        });
        let bytes = svg_bytes(&card);
        assert_eq!(
            bytes,
            svg_bytes(&share_card_spec(&ShareCardInput {
                handle: "alice",
                question: "Will it rain?",
                side_yes: true,
                payout_micro: 12_500_000,
                return_bps: 2_150,
            }))
        );
        let svg = String::from_utf8(bytes).unwrap();
        assert!(svg.contains("@alice"));
        assert!(svg.contains("Payout $12.50"));
        assert!(svg.contains("Return +21.5%"));
        assert!(svg.contains("viewBox=\"0 0 1200 630\""));
    }

    #[test]
    fn artifact_carries_the_svg_media_type() {
        let artifact = artifact(&poster_spec("q"));
        assert_eq!(artifact.media_type, "image/svg+xml");
        assert!(!artifact.bytes.is_empty());
    }
}
