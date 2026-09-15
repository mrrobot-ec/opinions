import re
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
TEST_DIR = REPO / "crates" / "adapters" / "tests"


class PostgresContractSuiteTests(unittest.TestCase):
    def test_database_setup_cannot_silently_skip_a_test(self) -> None:
        failures: list[str] = []
        setup_option = re.compile(
            r"async\s+fn\s+(?:pg_store|pg_pool|isolated_[a-z0-9_]+|new)"
            r"[^\{]*->\s*Option\s*<",
            re.DOTALL,
        )
        silent_return = re.compile(
            r"let\s+Some\([^;]+?\)\s*=\s*"
            r"(?:pg_store\(\)|pg_pool\(\)|TestDb::new\([^;]+?\)|isolated_[a-z0-9_]+\(\))"
            r"\.await\s+else\s*\{\s*return;\s*\};",
            re.DOTALL,
        )

        for path in sorted(TEST_DIR.glob("*.rs")):
            source = path.read_text()
            for label, pattern in (
                ("fallible DB setup returns Option", setup_option),
                ("test silently returns when DB setup fails", silent_return),
            ):
                for match in pattern.finditer(source):
                    line = source.count("\n", 0, match.start()) + 1
                    failures.append(f"{path.relative_to(REPO)}:{line}: {label}")

        self.assertEqual(failures, [], "\n" + "\n".join(failures))


if __name__ == "__main__":
    unittest.main()
