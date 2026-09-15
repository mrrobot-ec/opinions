#!/usr/bin/env python3
"""Create or verify Phase 5's no-VCS frozen-file manifest."""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INVENTORY_PATH = ROOT / "scripts/frozen_manifest.txt"
DIGEST_PATH = ROOT / "scripts/frozen_manifest.sha256"

FIXED = {
    "Cargo.lock",
    "Cargo.toml",
    "justfile",
    "migrations/0007_content.sql",
    "migrations/0008_ops.sql",
    "migrations/0009_request_fingerprints.sql",
    "scripts/check_frozen_manifest.py",
    "scripts/frozen_manifest.txt",
    "crates/application/src/error.rs",
    "crates/application/src/model.rs",
    "crates/application/src/ports/core.rs",
    "crates/application/src/ports/market.rs",
    "crates/application/src/ports/ops.rs",
    "crates/application/src/fakes/core.rs",
    "crates/application/src/fakes/market.rs",
    "crates/application/src/fakes/state.rs",
    "crates/application/src/seed_market.rs",
    "crates/adapters/src/pg/economy_tx.rs",
    "crates/adapters/src/pg/rows.rs",
    "crates/adapters/src/pg/store.rs",
    "crates/adapters/src/http/dto/core.rs",
    "crates/adapters/src/http/dto/market.rs",
    "crates/adapters/src/http/error.rs",
    "crates/adapters/src/http/middleware.rs",
    "crates/adapters/src/http/routes/core.rs",
    "crates/adapters/src/http/routes/mod.rs",
    "crates/adapters/src/http/ws.rs",
    "crates/adapters/tests/fixtures/llm/.keep",
    "crates/adapters/tests/fixtures/render/.keep",
    "crates/main/src/main.rs",
    "crates/simswarm/src/ports.rs",
}


def current_inventory() -> list[str]:
    paths = set(FIXED)
    paths.update(path.relative_to(ROOT).as_posix() for path in ROOT.glob("crates/*/Cargo.toml"))
    paths.update(path.relative_to(ROOT).as_posix() for path in ROOT.glob("crates/**/lib.rs"))
    paths.update(path.relative_to(ROOT).as_posix() for path in ROOT.glob("crates/**/mod.rs"))
    missing = sorted(path for path in paths if not (ROOT / path).is_file())
    if missing:
        raise SystemExit(f"frozen target missing: {', '.join(missing)}")
    return sorted(paths)


def digest(path: str) -> str:
    return hashlib.sha256((ROOT / path).read_bytes()).hexdigest()


def write_manifest() -> None:
    inventory = current_inventory()
    INVENTORY_PATH.write_text("".join(f"{path}\n" for path in inventory), encoding="utf-8")
    # Re-read after writing because the inventory is itself a frozen target.
    DIGEST_PATH.write_text(
        "".join(f"{digest(path)}  {path}\n" for path in inventory),
        encoding="utf-8",
    )


def verify_manifest() -> None:
    expected = INVENTORY_PATH.read_text(encoding="utf-8").splitlines()
    actual = current_inventory()
    if expected != actual:
        raise SystemExit("frozen path inventory changed; compare scripts/frozen_manifest.txt")
    recorded = {}
    for line in DIGEST_PATH.read_text(encoding="utf-8").splitlines():
        value, path = line.split("  ", 1)
        recorded[path] = value
    if sorted(recorded) != expected:
        raise SystemExit("frozen digest inventory does not match the path inventory")
    changed = [path for path in expected if recorded[path] != digest(path)]
    if changed:
        raise SystemExit(f"frozen file changed: {', '.join(changed)}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    if args.write:
        write_manifest()
    else:
        verify_manifest()


if __name__ == "__main__":
    main()
