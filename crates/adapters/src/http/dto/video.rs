//! Artifact wire shape (Task 5.2): raw SVG bodies with a pinned security
//! header posture (codex M4) — never JSON, never a path-derived body.

use axum::http::{header, StatusCode};
use axum::response::Response;

pub(crate) const SVG_CONTENT_TYPE: &str = "image/svg+xml; charset=utf-8";
pub(crate) const CSP_NONE: &str = "default-src 'none'";
/// Immutable cache is legal ONLY for finalized artifacts: `/assets/{job}`
/// bodies never change after `ready` (UUID-derived, atomically renamed).
pub(crate) const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Share-card URLs are stable per (user, market) while the underlying
/// content address could advance with spec versions: short public cache.
pub(crate) const CACHE_SHORT: &str = "public, max-age=300";

/// The one way an artifact leaves this server: `image/svg+xml` + `nosniff` +
/// `default-src 'none'` + inline disposition.
#[allow(clippy::expect_used)] // Every header name/value above is a compile-time constant.
pub(crate) fn svg_response(bytes: Vec<u8>, cache_control: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, SVG_CONTENT_TYPE)
        .header("x-content-type-options", "nosniff")
        .header(header::CONTENT_SECURITY_POLICY, CSP_NONE)
        .header(header::CONTENT_DISPOSITION, "inline")
        .header(header::CACHE_CONTROL, cache_control)
        .body(bytes.into())
        .expect("static header set is always a valid response")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn svg_response_pins_the_full_header_posture() {
        let response = svg_response(b"<svg/>".to_vec(), CACHE_IMMUTABLE);
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers.get("content-type").unwrap(),
            "image/svg+xml; charset=utf-8"
        );
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(
            headers.get("content-security-policy").unwrap(),
            "default-src 'none'"
        );
        assert_eq!(headers.get("content-disposition").unwrap(), "inline");
        assert_eq!(
            headers.get("cache-control").unwrap(),
            "public, max-age=31536000, immutable"
        );

        let short = svg_response(Vec::new(), CACHE_SHORT);
        assert_eq!(
            short.headers().get("cache-control").unwrap(),
            "public, max-age=300"
        );
    }
}
