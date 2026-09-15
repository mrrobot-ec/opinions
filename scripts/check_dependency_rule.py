#!/usr/bin/env python3
"""Fail if any internal crate dependency violates the inward-only rule.

Checks BOTH normal and dev-dependencies (kind null/normal and kind=="dev").
Build-dependencies are also rejected if they form an illegal internal edge.
"""
import json
import subprocess
import sys

ALLOWED = {
    "domain": set(),
    "application": {"domain"},
    "adapters": {"domain", "application"},
    "main": {"domain", "application", "adapters"},
    # Outer test harness: drives the system via public REST/WS only (Phase 6 D28).
    "simswarm": set(),
}

# Kinds that count as graph edges we enforce (Phase 1 review amendment).
CHECKED_KINDS = {None, "normal", "dev", "build"}


def main() -> int:
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"]
        )
    )
    # Workspace members only (path packages in this repo).
    workspace_ids = set(meta.get("workspace_members", []))
    internal = {
        p["name"]: p
        for p in meta["packages"]
        if p["id"] in workspace_ids or p["name"] in ALLOWED
    }
    # Prefer workspace_members when present; fall back to name filter.
    if workspace_ids:
        internal = {p["name"]: p for p in meta["packages"] if p["id"] in workspace_ids}

    bad: list[str] = []
    for name, pkg in sorted(internal.items()):
        if name not in ALLOWED:
            bad.append(
                f"unknown internal crate {name}: add it to ALLOWED with its layer"
            )
            continue
        for d in pkg["dependencies"]:
            dep_name = d["name"]
            if dep_name not in internal:
                continue
            kind = d.get("kind")  # None or "normal" for normal; "dev"; "build"
            if kind not in CHECKED_KINDS:
                continue
            kind_label = kind or "normal"
            if dep_name not in ALLOWED[name]:
                bad.append(
                    f"{name} -> {dep_name} ({kind_label}) violates the dependency rule "
                    f"(allowed: {sorted(ALLOWED[name])})"
                )
    if bad:
        print("\n".join(bad))
        return 1
    print(f"dependency rule OK ({len(internal)} internal crates)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
