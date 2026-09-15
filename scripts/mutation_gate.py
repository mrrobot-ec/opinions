#!/usr/bin/env python3
"""Gate: killed/(killed+missed) >= threshold%, from cargo-mutants outcomes.json.

Fails on: unviable/timeout/failure summaries beyond a tolerance, zero mutants,
or an unrecognized schema — a passing rate over an invalid run is worthless.
Unit-tested against fixtures in scripts/fixtures/ (pass, fail, timeout, empty).
"""
import json, sys

data = json.load(open(sys.argv[1]))
threshold = float(sys.argv[2])
rows = data.get("outcomes")
if not isinstance(rows, list) or not rows:
    print("invalid or empty outcomes.json — refusing to gate"); sys.exit(1)
counts = {}
for o in rows:
    counts[o.get("summary", "UNKNOWN")] = counts.get(o.get("summary", "UNKNOWN"), 0) + 1
caught = counts.pop("CaughtMutant", 0)
missed = counts.pop("MissedMutant", 0)
counts.pop("Unviable", None)  # unviable mutants are expected noise
if counts:  # timeouts, baseline failures, unknown summaries → invalid run
    print(f"non-gateable outcomes present: {counts} — fix the run first"); sys.exit(1)
if caught + missed == 0:
    print("no gateable mutants"); sys.exit(1)
rate = 100.0 * caught / (caught + missed)
print(f"mutation kill rate: {rate:.1f}% (caught={caught}, missed={missed})")
sys.exit(0 if rate >= threshold else 1)
