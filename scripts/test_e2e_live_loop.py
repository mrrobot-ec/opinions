import re
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("e2e_live_loop.sh")


def run_latency_parser(lines: str) -> subprocess.CompletedProcess[str]:
    source = SCRIPT.read_text()
    match = re.search(
        r"PAYOUT_LATENCY_MS=\$\(python3 -c '\n(?P<program>.*?)\n' \"\$PROBE_OUT\"\)",
        source,
        re.DOTALL,
    )
    if match is None:
        raise AssertionError("could not find the live-loop payout latency parser")

    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as probe:
        probe.write(lines)
        probe.flush()
        return subprocess.run(
            ["python3", "-c", match.group("program"), probe.name],
            check=False,
            capture_output=True,
            text=True,
        )


def run_tally_parser(summary: str) -> subprocess.CompletedProcess[str]:
    source = SCRIPT.read_text()
    match = re.search(
        r"TALLY_AFTER=\$\(echo \"\$PROBE_SUMMARY_JSON\" \| jq -r '([^']+)'\)",
        source,
    )
    if match is None:
        raise AssertionError("could not find the live-loop tally parser")
    return subprocess.run(
        ["jq", "-r", match.group(1)],
        input=summary,
        check=False,
        capture_output=True,
        text=True,
    )


class LiveLoopLatencyParserTests(unittest.TestCase):
    def test_computes_closed_to_resolved_delta(self) -> None:
        result = run_latency_parser(
            '{"t_ms":1000,"frame":{"type":"lifecycle","state":"closed"}}\n'
            '{"t_ms":2750,"frame":{"type":"lifecycle","state":"resolved"}}\n'
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1750")

    def test_rejects_trace_without_resolved_frame(self) -> None:
        result = run_latency_parser(
            '{"t_ms":1000,"frame":{"type":"lifecycle","state":"closed"}}\n'
            '{"t_ms":1200,"frame":{"type":"lifecycle","state":"paid"}}\n'
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_rejects_summary_without_tally_counter(self) -> None:
        result = run_tally_parser('{"ordered":["snapshot","price"]}\n')
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_accepts_explicit_zero_tally_counter(self) -> None:
        result = run_tally_parser('{"tallyAfterClosing":0}\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "0")


if __name__ == "__main__":
    unittest.main()
