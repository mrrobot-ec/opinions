#!/usr/bin/env python3
"""Regression tests for the LCOV union coverage gate."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
GATE = ROOT / "scripts" / "coverage_gate.py"


class CoverageGateTests(unittest.TestCase):
    def run_gate(self, lcov: str) -> subprocess.CompletedProcess[str]:
        with tempfile.NamedTemporaryFile(mode="w", suffix=".lcov") as fixture:
            fixture.write(lcov)
            fixture.flush()
            return subprocess.run(
                [sys.executable, str(GATE), fixture.name, "fixture"],
                check=False,
                capture_output=True,
                text=True,
            )

    def test_rejects_empty_lcov(self) -> None:
        result = self.run_gate("")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("no executable lines", result.stdout)

    def test_accepts_fully_covered_lcov(self) -> None:
        result = self.run_gate("SF:/repo/src/lib.rs\nDA:1,1\nend_of_record\n")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_rejects_an_uncovered_line(self) -> None:
        result = self.run_gate("SF:/repo/src/lib.rs\nDA:1,0\nend_of_record\n")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 uncovered lines of 1", result.stdout)


if __name__ == "__main__":
    unittest.main()
