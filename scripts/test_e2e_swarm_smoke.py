#!/usr/bin/env python3
"""Regression tests for the Phase 7 red-team section of e2e_swarm_smoke.sh.

These pin the defects confirmed in var/p9-smoke-refutation.md: the section used
to report green against absent routes (404), hardcoded stubs (503), discarded
statuses, and SQL self-assertions that asserted nothing but their own INSERT.

The tests are deliberately behavioural where they can be: the shell helpers that
decide pass/fail are extracted from the script and EXECUTED with representative
inputs, so a future rewrite that keeps the helper name but loosens the predicate
still fails. The remaining tests parse the Phase 7 section for required
executable actions (request fields, positive controls, real races) rather than
for comments, so restating an intention in prose cannot satisfy them.

Run: python3 -m unittest scripts/test_e2e_swarm_smoke.py
"""

from __future__ import annotations

import os
import re
import subprocess
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
# SMOKE_SCRIPT lets the pre-fix copy under var/backup/ be re-checked, so the red
# baseline for these tests stays reproducible:
#   SMOKE_SCRIPT=var/backup/scripts/e2e_swarm_smoke.sh \
#     python3 -m unittest scripts/test_e2e_swarm_smoke.py
SCRIPT = Path(os.environ.get("SMOKE_SCRIPT", REPO / "scripts" / "e2e_swarm_smoke.sh"))

# The red-team section runs from its own `section` banner to end of file.
PHASE7_BANNER = 'section "Phase 7 red-team'

BASE58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

# Statuses that must never be accepted as "the attack failed" on a money or
# compliance route: 404 is an unmounted route, 503 is an unimplemented stub.
UNMOUNTED = "404"
STUB = "503"


def script_text() -> str:
    return SCRIPT.read_text(encoding="utf-8")


def phase7_text() -> str:
    text = script_text()
    index = text.find(PHASE7_BANNER)
    if index < 0:
        raise AssertionError(f"{SCRIPT} no longer contains {PHASE7_BANNER!r}")
    return text[index:]


def strip_comments(text: str) -> str:
    """Drop whole-line and trailing `#` comments so prose cannot satisfy a test.

    Conservative: a `#` inside single or double quotes is left alone.
    """
    out = []
    for line in text.splitlines():
        quote = None
        cut = len(line)
        for pos, char in enumerate(line):
            if quote:
                if char == quote:
                    quote = None
            elif char in "'\"":
                quote = char
            elif char == "#" and (pos == 0 or line[pos - 1] in " \t"):
                cut = pos
                break
        out.append(line[:cut])
    return "\n".join(out)


def base58_decode(value: str) -> bytes | None:
    number = 0
    for char in value:
        if char not in BASE58_ALPHABET:
            return None
        number = number * 58 + BASE58_ALPHABET.index(char)
    body = number.to_bytes((number.bit_length() + 7) // 8, "big") if number else b""
    leading_zeros = len(value) - len(value.lstrip("1"))
    return b"\x00" * leading_zeros + body


def extract_function(name: str) -> str:
    """Return the source of a top-level `name() { ... }` shell function."""
    text = script_text()
    match = re.search(rf"^{re.escape(name)}\(\) \{{$", text, re.MULTILINE)
    if not match:
        raise AssertionError(f"{SCRIPT} defines no shell function {name}()")
    lines = text[match.start():].splitlines()
    body = [lines[0]]
    for line in lines[1:]:
        body.append(line)
        if line == "}":
            return "\n".join(body)
    raise AssertionError(f"{name}() is not terminated by a line-anchored '}}'")


def run_helper(name: str, args: list[str], extra: str = "") -> int:
    """Execute one extracted helper in a real bash with the script's `fail`."""
    harness = "\n".join(
        [
            "set -uo pipefail",
            'fail() { echo "FAIL: $*" >&2; exit 1; }',
            "ARTIFACT_DIR=$(mktemp -d)",
            "prereq_missing() { echo \"PREREQ: $*\" >&2; }",
            extra,
            extract_function(name),
            f'{name} {" ".join(subprocess_quote(a) for a in args)}',
        ]
    )
    completed = subprocess.run(
        ["bash", "-c", harness], capture_output=True, text=True, timeout=60
    )
    return completed.returncode


def subprocess_quote(value: str) -> str:
    return "'" + value.replace("'", "'\\''") + "'"



class ShellCase(unittest.TestCase):
    """Assertions that report a short reason instead of dumping the script."""

    def has(self, haystack: str, needle: str, msg: str) -> None:
        self.assertTrue(needle in haystack, f"{msg} (missing: {needle!r})")

    def lacks(self, haystack: str, needle: str, msg: str) -> None:
        self.assertFalse(needle in haystack, f"{msg} (still present: {needle!r})")

    def matches(self, haystack: str, pattern: str, msg: str) -> None:
        self.assertTrue(
            re.search(pattern, haystack) is not None, f"{msg} (no match: {pattern!r})"
        )

    def not_matches(self, haystack: str, pattern: str, msg: str) -> None:
        self.assertTrue(
            re.search(pattern, haystack) is None, f"{msg} (matched: {pattern!r})"
        )


class BashSyntax(unittest.TestCase):
    def test_script_parses(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(SCRIPT)], capture_output=True, text=True, timeout=60
        )
        self.assertEqual(
            completed.returncode, 0, f"bash -n failed:\n{completed.stderr}"
        )


class MoneyStatusHelper(ShellCase):
    """A missing or stubbed money route must FAIL the run, never pass it."""

    def test_helper_exists(self) -> None:
        self.has(script_text(), "expect_money_status() {",
            "the script has no expect_money_status helper, so money legs still "
            "decide pass/fail with ad-hoc predicates",
        )

    def test_unmounted_route_fails(self) -> None:
        self.assertNotEqual(
            0,
            run_helper("expect_money_status", ["403", "probe", UNMOUNTED]),
            "expect_money_status accepted 404: an unmounted route would pass",
        )

    def test_stub_route_fails(self) -> None:
        self.assertNotEqual(
            0,
            run_helper("expect_money_status", ["403", "probe", STUB]),
            "expect_money_status accepted 503: a hardcoded stub would pass",
        )

    def test_wrong_status_fails(self) -> None:
        self.assertNotEqual(
            0,
            run_helper("expect_money_status", ["403", "probe", "200"]),
            "expect_money_status accepted a status other than the expected one",
        )

    def test_expected_status_passes(self) -> None:
        self.assertEqual(
            0,
            run_helper("expect_money_status", ["403", "probe", "403"]),
            "expect_money_status rejected the exact expected status",
        )

    def test_unmounted_is_rejected_even_when_expected_is_404(self) -> None:
        """404 must never be spelled as the expected outcome of a money leg."""
        self.assertNotEqual(
            0,
            run_helper("expect_money_status", [UNMOUNTED, "probe", UNMOUNTED]),
            "expect_money_status let a leg declare 404 its expected outcome",
        )


class BroadPredicates(ShellCase):
    """No leg may accept 'anything that is not 200'."""

    def test_no_not_200_predicates(self) -> None:
        offenders = [
            line.strip()
            for line in strip_comments(phase7_text()).splitlines()
            if re.search(r'!=\s*"?200"?', line)
        ]
        self.assertEqual(
            [],
            offenders,
            "Phase 7 legs still accept any non-200 status:\n  "
            + "\n  ".join(offenders),
        )

    def test_no_discarded_http_status(self) -> None:
        """`-w '%{http_code}'` piped to /dev/null throws the verdict away."""
        section = strip_comments(phase7_text())
        offenders = [
            block
            for block in re.split(r"\n(?=\S)", section)
            if "%{http_code}" in block
            and re.search(r"\"\$BASE/(withdrawals|webhooks|trades)[^\"]*\"\s*>\s*/dev/null", block)
        ]
        self.assertEqual(
            [], offenders, "a Phase 7 money request still discards its status code"
        )


class WithdrawalRequests(ShellCase):
    """Every /withdrawals body must reach the handler, not axum's rejector."""

    def bodies(self) -> list[str]:
        text = script_text()
        return re.findall(r"jq -cn[^\n]*\n?[^\n]*?\$BASE/withdrawals", text) or re.findall(
            r"withdraw_request[^\n]*", text
        )

    def test_withdraw_helper_sends_confirm_dest(self) -> None:
        self.has(script_text(), "confirm_dest",
            "no /withdrawals body sends confirm_dest; WithdrawRequestDto requires "
            "it, so every request dies as a 422 deserialization rejection before "
            "geo, sanctions, warmth, limits or self-exclusion run",
        )

    def test_withdraw_helper_sends_idempotency_key(self) -> None:
        self.has(self.withdraw_helper(), "idempotency_key",
            "the withdrawal helper sends no idempotency_key, so a settle-then-"
            "retry leg cannot be replay-stable",
        )

    def test_withdraw_helper_sends_an_untrusted_client_ip(self) -> None:
        self.has(
            self.withdraw_helper(),
            'x-forwarded-for: 203.0.113.9',
            "the trusted local proxy needs an untrusted X-Forwarded-For hop; "
            "without it every withdrawal is refused as geo_missing_ip before "
            "warmth, limits, AML or self-exclusion can run",
        )

    def withdraw_helper(self) -> str:
        return extract_function("withdraw_request")

    def test_single_withdraw_helper_is_used(self) -> None:
        section = strip_comments(phase7_text())
        raw = re.findall(r'"\$BASE/withdrawals"', section)
        self.assertEqual(
            [],
            raw,
            "Phase 7 still posts to /withdrawals with a hand-rolled curl instead "
            "of the helper that guarantees confirm_dest + idempotency_key",
        )

    def test_dest_literals_canonicalize_to_32_bytes(self) -> None:
        """canonicalize_dest accepts a base58 value that decodes to 32 bytes."""
        text = script_text()
        candidates = set(re.findall(r'"([1-9A-HJ-NP-Za-km-z]{40,50})"', text))
        self.assertTrue(candidates, "no withdrawal destination literals found")
        bad = {}
        for value in candidates:
            decoded = base58_decode(value)
            if decoded is None or len(decoded) != 32:
                bad[value] = "invalid base58" if decoded is None else len(decoded)
        self.assertEqual(
            {},
            bad,
            "destination literals do not satisfy canonicalize_dest (32 decoded "
            f"bytes): {bad}",
        )

    def test_dest_literals_are_validated_in_script(self) -> None:
        self.has(script_text(), "assert_dest_canonical",
            "the script never validates its destination literals against the "
            "canonicalizer contract",
        )


class ExactStatuses(ShellCase):
    def test_omitted_config_version_is_exactly_422(self) -> None:
        section = strip_comments(phase7_text())
        match = re.search(
            r"expect_money_status\s+422\s+[^\n]*omit", section, re.IGNORECASE
        )
        self.assertIsNotNone(
            match,
            "the omitted expected_config_version leg does not assert exactly 422",
        )

    def test_unsigned_webhook_asserts_authentication_rejection(self) -> None:
        section = strip_comments(phase7_text())
        self.matches(section, r"expect_money_status\s+403\s+[^\n]*(webhook|unsigned)",
            "the unsigned KYC webhook leg does not assert the documented 403 "
            "AdminForbidden authentication rejection",
        )

    def test_signed_webhook_positive_control_exists(self) -> None:
        section = strip_comments(phase7_text())
        self.has(section, "x-kyc-signature",
            "there is no signed KYC webhook positive control, so a rejection is "
            "indistinguishable from an absent route",
        )
        self.matches(section, r"expect_money_status\s+200\s+[^\n]*signed",
            "the signed KYC webhook control does not assert acceptance",
        )

    def test_webhook_body_is_well_formed(self) -> None:
        """event_id/provider_ref are validated BEFORE the signature check."""
        section = strip_comments(phase7_text())
        self.has(section, "event_id",
            "the webhook probe omits event_id, so ingest_kyc answers 422 "
            "InvalidWebhook and the signature gate is never reached",
        )
        self.assertIn("provider_ref", section, "the webhook probe omits provider_ref")


class NoVacuousSql(ShellCase):
    def test_no_self_inserted_alert_is_asserted(self) -> None:
        section = strip_comments(phase7_text())
        self.not_matches(section, r"insert into alert_outbox",
            "the reconciliation leg still inserts its own alert_outbox row and "
            "then asserts that row exists; no detector is exercised",
        )

    def test_reconciliation_alert_is_detector_written(self) -> None:
        section = strip_comments(phase7_text())
        self.matches(section, r"reconciliation_residual",
            "the reconciliation leg no longer references the detector incident key",
        )

    def test_no_self_asserted_sanction_verdict(self) -> None:
        section = strip_comments(phase7_text())
        self.not_matches(
            section,
            r"order by checked_at desc limit 1\"\)\"?\s*\]?\s*=\s*hit",
            "the leg still asserts the sanctions verdict it just inserted",
        )

    def test_no_raw_referral_bind_inserts(self) -> None:
        section = strip_comments(phase7_text())
        self.not_matches(section, r"insert into referral_binds",
            "the referral legs still write the rows they claim to test",
        )

    def test_no_observation_self_assertion(self) -> None:
        """Asserting the columns you omitted from your own INSERT proves nothing."""
        section = strip_comments(phase7_text())
        self.not_matches(section, r"admit_tx_id is null and user_id is null",
            "the no-KYC deposit leg still asserts its own INSERT's null columns",
        )


class PreservedControls(ShellCase):
    """The three genuine controls from the refutation must survive."""

    def test_partial_observation_rejection_kept(self) -> None:
        section = strip_comments(phase7_text())
        self.has(section, "p7-partial-observation.err",
            "the partial-observation CHECK rejection control was dropped",
        )

    def test_ledger_conservation_kept(self) -> None:
        section = strip_comments(phase7_text())
        self.has(section, "coalesce(sum(amount_micro),0) from ledger_entries",
            "the ledger conservation assertion was dropped",
        )
        self.has(section, "having sum(amount_micro) <> 0",
            "the per-transaction balance assertion was dropped",
        )
        self.matches(section, r"owner_type in \('user','withheld','deposit_suspense','bonus_reserve'\)",
            "the non-negative authority assertion was dropped",
        )

    def test_faucet_single_credit_kept(self) -> None:
        section = strip_comments(phase7_text())
        self.has(section, "conc-deposit-",
            "the concurrent faucet single-credit assertion was dropped",
        )

    def test_missing_version_control_kept(self) -> None:
        section = strip_comments(phase7_text())
        self.has(section, "expected_config_version",
            "the omitted-version control was dropped",
        )


class DepositObservationFixture(ShellCase):
    def test_complete_observation_supplies_rail_fingerprint(self) -> None:
        section = strip_comments(phase7_text())
        inserts = re.findall(
            r"insert into deposits\((.*?)\)\s*\n?\s*values", section, re.DOTALL
        )
        self.assertTrue(inserts, "the deposit observation fixture disappeared")
        complete = [cols for cols in inserts if "observed_slot" in cols]
        self.assertTrue(
            complete,
            "no complete observation fixture (one carrying observed_slot) remains",
        )
        for cols in complete:
            self.has(cols, "rail_fingerprint",
                "the complete deposit observation omits rail_fingerprint and so "
                "violates deposits_observation_identity; the run aborts there",
            )


class PositiveControls(ShellCase):
    """Each retained leg needs a case that fails if the code were deleted."""

    def test_wash_after_grant_places_a_trade(self) -> None:
        section = strip_comments(phase7_text())
        self.assertRegex(
            section,
            r"\bplace\s+\"\$GRANT_USER\"",
            "the wash-after-grant leg still only PREVIEWS; a preview converts "
            "nothing, so the no-convert assertion is vacuous",
        )

    def test_wash_after_grant_asserts_both_sides_of_paid(self) -> None:
        section = strip_comments(phase7_text())
        self.has(section, "converted_at is null",
            "the pre-Paid no-convert assertion is missing",
        )
        self.has(section, "converted_at is not null",
            "the post-Paid convert assertion is missing: without it the leg "
            "passes when conversion is deleted entirely",
        )

    def test_wash_after_grant_proves_fee_volume(self) -> None:
        section = strip_comments(phase7_text())
        self.has(section, "credit_fee_allocations",
            "the leg never proves fee volume was allocated against the lot",
        )

    def test_wash_grant_fixture_has_credit_and_cash_backing(self) -> None:
        section = strip_comments(phase7_text())
        fixture = re.search(
            r"BONUS_LOT=.*?prereq_missing \"credit grant issuance:[^\n]*",
            section,
            re.DOTALL,
        )
        self.assertIsNotNone(fixture, "the wash grant fixture disappeared")
        body = fixture.group(0)
        self.has(body, "insert into ledger_accounts",
            "the grant fixture creates a lot without its ledger accounts",
        )
        self.has(body, "insert into ledger_transactions",
            "the grant fixture creates no balanced ledger history",
        )
        self.has(body, "insert into ledger_entries",
            "the grant fixture creates transaction headers without entries",
        )
        self.has(body, "'usdc_credit'",
            "the grant fixture gives the user no credit balance",
        )
        self.has(body, "'bonus_reserve'",
            "the real-money grant fixture has no cash reserve backing",
        )
        self.has(body, "'credit_grant'",
            "the grant fixture does not transfer credit from House to the user",
        )

    def test_aml_has_in_band_positive_series(self) -> None:
        section = strip_comments(phase7_text())
        self.has(section, "499000000",
            "no in-band ($499) structuring series: the AML leg only asserts that "
            "nothing flags, which passes with no detector at all",
        )
        self.has(section, "25000000",
            "the below-floor ($25) negative structuring series was dropped",
        )

    def test_aml_positive_series_expects_an_open_flag(self) -> None:
        section = strip_comments(phase7_text())
        self.matches(section, r"aml_flags[^\n]*\n?[^\n]*status='open'",
            "the AML leg no longer reads open flags",
        )
        self.matches(section, r"structuring",
            "the AML positive control does not pin the structuring kind",
        )

    def test_settle_then_retry_issues_two_requests(self) -> None:
        section = strip_comments(phase7_text())
        retries = re.findall(r"RETRY|retry", section)
        self.assertGreaterEqual(
            len(retries),
            2,
            "the settle-then-retry leg still issues a single request, so there "
            "is no retry to block",
        )
        self.has(section, "IdempotencyConflict",
            "the retry leg does not pin the replay/idempotency contract",
        )

    def test_concurrency_runs_on_a_live_market(self) -> None:
        section = strip_comments(phase7_text())
        self.lacks(section, "MarketNotOpen",
            "the concurrency leg still expects the trade racer to bounce off a "
            "closed book; that proves nothing about the sorted-lock algorithm",
        )
        self.has(section, "P7_MARKET",
            "the concurrency leg does not race against a market created live "
            "for Phase 7",
        )

    def test_admit_refund_race_is_not_a_nil_uuid_probe(self) -> None:
        section = strip_comments(phase7_text())
        self.lacks(section, "00000000-0000-0000-0000-000000000001",
            "the admit/refund leg still probes a made-up deposit id",
        )
        self.matches(section, r"expect_money_status\s+409\s+[^\n]*(refund|admit)",
            "the admit/refund leg does not assert the real not-held precondition",
        )


class PrerequisiteReporting(ShellCase):
    """An absent production surface must fail the run, loudly and by name."""

    def test_prereq_missing_helper_exists(self) -> None:
        self.has(script_text(), "prereq_missing() {",
            "the script cannot record a missing cross-owner prerequisite",
        )

    def test_missing_prerequisites_fail_the_run(self) -> None:
        section = strip_comments(phase7_text())
        self.matches(section, r"PREREQ_FAILURES\[@\]",
            "recorded prerequisites are never checked, so the run could still "
            "print its GREEN line with money legs unproven",
        )
        self.assertRegex(
            section,
            r"fail \"[^\"]*prerequisit",
            "a missing prerequisite does not fail the run",
        )

    def test_green_line_is_gated(self) -> None:
        text = script_text()
        green = text.rfind("PHASE 7 W4 E2E GREEN")
        gate = text.rfind("PREREQ_FAILURES")
        self.assertNotEqual(-1, gate, "no prerequisite gate exists at all")
        self.assertNotEqual(-1, green, "the GREEN banner disappeared")
        self.assertLess(
            gate,
            green,
            "the GREEN banner is printed before the prerequisite gate runs",
        )


if __name__ == "__main__":
    unittest.main()
