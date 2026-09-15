#!/usr/bin/env python3
"""Assert 100% line coverage from the lcov UNION.

Why not `cargo llvm-cov --fail-under-lines 100` alone: llvm-cov's summary folds
the cfg(test) and non-test instantiation records of the same function with
max-of-covered instead of set-union, so a line executed only by one of the two
binaries is reported as missed. The lcov export unions correctly, so it is the
honest measure of "some test executed this line". This gate is therefore
STRICTER in intent (zero uncovered lines, no exclusions) and free of the
tool artifact. See docs-site/running/tests-and-gates.md.
"""
import sys

path, label = sys.argv[1], sys.argv[2]
cur = None
total = 0
missed = []
for line in open(path):
    line = line.strip()
    if line.startswith("SF:"):
        cur = line[3:]
    elif line.startswith("DA:"):
        n, h = line[3:].split(",")[:2]
        total += 1
        if h == "0":
            missed.append(f"{cur}:{n}")
if missed:
    print(f"{label}: {len(missed)} uncovered lines of {total}")
    for entry in missed[:40]:
        print(f"  {entry}")
    sys.exit(1)
print(f"{label}: 100% lines ({total} lines, 0 uncovered)")
