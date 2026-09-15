"""Static regressions for test constructs that can report green without testing."""

from __future__ import annotations

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOURCE_SUFFIXES = {".rs", ".py", ".ts", ".tsx"}
SKIP_PARTS = {"node_modules", ".next", ".venv", "__pycache__", "var"}


def sources() -> list[Path]:
    roots = [ROOT / "crates", ROOT / "services" / "converse", ROOT / "web"]
    return sorted(
        path
        for base in roots
        for path in base.rglob("*")
        if path.is_file()
        and path.suffix in SOURCE_SUFFIXES
        and not SKIP_PARTS.intersection(path.parts)
    )


class TestQualityTests(unittest.TestCase):
    def test_assertions_do_not_compare_an_expression_with_itself(self) -> None:
        matcher = re.compile(
            r"(?:prop_)?assert_eq!\(\s*([^,\n]+),\s*\1\s*\)|"
            r"assert!\(\s*true\s*\)|assert\s+True\b"
        )
        failures: list[str] = []
        for path in sources():
            for line_number, line in enumerate(path.read_text().splitlines(), 1):
                if matcher.search(line):
                    failures.append(
                        f"{path.relative_to(ROOT)}:{line_number}: {line.strip()}"
                    )
        self.assertEqual(failures, [], "\n" + "\n".join(failures))

    def test_integration_suites_do_not_silently_skip_when_unarmed(self) -> None:
        patterns = {
            "pytest.skip": re.compile(r"\bpytest\.skip\("),
            "pytest skipif": re.compile(r"pytest\.mark\.skipif\("),
            "Playwright test.skip": re.compile(r"\btest\.skip\("),
            "Rust ignored test": re.compile(r"#\s*\[\s*ignore\s*\]"),
        }
        failures: list[str] = []
        for path in sources():
            for line_number, line in enumerate(path.read_text().splitlines(), 1):
                for label, matcher in patterns.items():
                    if matcher.search(line):
                        failures.append(
                            f"{path.relative_to(ROOT)}:{line_number}: {label}"
                        )
        self.assertEqual(failures, [], "\n" + "\n".join(failures))

    def test_matchers_reject_synthetic_verification_theatre(self) -> None:
        self.assertRegex("assert_eq!(value, value);", r"assert_eq!\(([^,]+),\s*\1\)")
        self.assertRegex("pytest.skip('no db')", r"pytest\.skip\(")
        self.assertRegex("test.skip(true, 'no env')", r"test\.skip\(")


if __name__ == "__main__":
    unittest.main()
