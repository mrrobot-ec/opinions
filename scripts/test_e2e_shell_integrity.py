"""Regression tests against verification-theatre in the phase E2E scripts."""

from __future__ import annotations

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ECONOMY = (ROOT / "scripts" / "e2e_economy.sh").read_text()
SOCIAL = (ROOT / "scripts" / "e2e_social.sh").read_text()


class E2EShellIntegrityTests(unittest.TestCase):
    def test_flip_fee_assertion_is_not_unconditionally_masked(self) -> None:
        self.assertNotRegex(
            ECONOMY,
            re.compile(
                r"\[ \"\$SELL_FEE\" != \"\$DISC_FEE\" \].*\|\| true"
            ),
        )

    def test_database_fixture_writes_are_not_swallowed(self) -> None:
        for name, source in (("economy", ECONOMY), ("social", SOCIAL)):
            with self.subTest(script=name):
                self.assertNotRegex(
                    source,
                    re.compile(r"psql_demo \"insert into reputation[\s\S]{0,240}\|\| true"),
                )

    def test_young_brigade_requires_every_report_to_be_rejected(self) -> None:
        section = SOCIAL.split('echo "== [3] brigade:', 1)[1].split(
            'echo "SECTION 3 GREEN', 1
        )[0]
        self.assertIn('[ "$code" = "403" ] || fail', section)
        self.assertIn('[ "$YOUNG_OK" = "0" ] || fail', section)

    def test_mention_flood_requires_each_post_to_succeed(self) -> None:
        section = SOCIAL.split('echo "== [5] mention flood', 1)[1].split(
            'echo "SECTION 5 GREEN', 1
        )[0]
        self.assertNotIn('|| echo', section)
        self.assertIn("jq -er '.id'", section)

    def test_pagination_setup_requires_each_post_to_succeed(self) -> None:
        section = SOCIAL.split('echo "== [7] hot pagination', 1)[1].split(
            'PAGE1=', 1
        )[0]
        self.assertNotIn('|| true', section)


if __name__ == "__main__":
    unittest.main()
