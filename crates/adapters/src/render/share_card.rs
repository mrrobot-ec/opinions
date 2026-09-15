//! Content-addressed share-card cache (Task 5.2, codex B5 + grok B2): cards
//! live under the `cards/` subroot of the render dir, named by the
//! `sha256(user, market, realization digest, spec version)` address computed
//! in the application layer. Same escape/serve posture as job artifacts.

use std::path::{Path, PathBuf};

use application::error::StoreError;
use domain::render_spec::RenderSpec;

use super::artifacts::{read_under_root, write_atomic};
use super::svg::svg_bytes;

const CARDS_SUBROOT: &str = "cards";

fn cards_root(render_dir: &Path) -> PathBuf {
    render_dir.join(CARDS_SUBROOT)
}

fn valid_address(address: &str) -> bool {
    address.len() == 64 && address.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Cache hit serves the stored bytes; miss renders, persists atomically, and
/// serves the fresh bytes. Content addressing makes the write idempotent.
pub(super) fn serve_or_render(
    render_dir: &Path,
    address: &str,
    spec: &RenderSpec,
) -> Result<Vec<u8>, StoreError> {
    if !valid_address(address) {
        return Err(StoreError::Invariant(
            "share-card address must be hex sha256",
        ));
    }
    let root = cards_root(render_dir);
    let name = format!("{address}.svg");
    if let Some(bytes) = read_under_root(&root, &name)? {
        return Ok(bytes);
    }
    let bytes = svg_bytes(spec);
    write_atomic(&root, &name, &bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use domain::render_spec::{sha256_hex, share_card_spec, ShareCardInput};

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir()
            .join("opinions-w2-cards")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec() -> domain::render_spec::RenderSpec {
        share_card_spec(&ShareCardInput {
            handle: "alice",
            question: "q",
            side_yes: true,
            payout_micro: 1_000_000,
            return_bps: 100,
        })
    }

    #[test]
    fn renders_once_then_serves_the_cached_bytes() {
        let dir = scratch();
        let address = sha256_hex(b"card-1");
        let first = serve_or_render(&dir, &address, &spec()).unwrap();
        let card_path = dir.join("cards").join(format!("{address}.svg"));
        assert_eq!(std::fs::read(&card_path).unwrap(), first);
        // Poison the cache file to PROVE the second call reads, not renders.
        std::fs::write(&card_path, b"cached-bytes").unwrap();
        assert_eq!(
            serve_or_render(&dir, &address, &spec()).unwrap(),
            b"cached-bytes"
        );
    }

    #[test]
    fn distinct_addresses_are_distinct_files() {
        let dir = scratch();
        serve_or_render(&dir, &sha256_hex(b"a"), &spec()).unwrap();
        serve_or_render(&dir, &sha256_hex(b"b"), &spec()).unwrap();
        assert_eq!(std::fs::read_dir(dir.join("cards")).unwrap().count(), 2);
    }

    #[test]
    fn malformed_addresses_are_refused() {
        let dir = scratch();
        for bad in ["", "abc", "../../etc/passwd", &"g".repeat(64), "café"] {
            assert!(matches!(
                serve_or_render(&dir, bad, &spec()),
                Err(StoreError::Invariant(_))
            ));
        }
    }
}
