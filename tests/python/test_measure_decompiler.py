from __future__ import annotations

import unittest

from tools.measure_decompiler import (
    MalformedPhaseReportError,
    NoPhaseReportError,
    parse_phase_report,
)


class ParsePhaseReportTests(unittest.TestCase):
    def test_extracts_the_json_object_from_surrounding_output(self) -> None:
        stderr = (
            "warning: something\n"
            '{\n  "phases": [\n'
            '    {"phase": "format", "seconds": 0.205, "alloc_mb": 12.7}\n'
            '  ],\n  "peak_live_mb": 1622.5\n}\n'
            "trailing noise\n"
        )

        report = parse_phase_report(stderr)

        self.assertEqual(report["peak_live_mb"], 1622.5)
        self.assertEqual(report["phases"][0]["phase"], "format")

    def test_missing_report_is_an_error_not_an_empty_result(self) -> None:
        with self.assertRaises(NoPhaseReportError):
            parse_phase_report("no json here")

    def test_extracts_the_report_when_noise_before_it_also_has_braces(
        self,
    ) -> None:
        # Mirrors the real failure mode: a stray `{...}` ahead of the
        # actual report (e.g. a log line) must not make the parser pick
        # the wrong opening brace and choke, or worse, slice out garbage.
        stderr = (
            "warning: {threshold exceeded}\n"
            '{\n  "phases": [\n'
            '    {"phase": "format", "seconds": 0.205, "alloc_mb": 12.7}\n'
            '  ],\n  "peak_live_mb": 1622.5\n}\n'
            "decompiler error: {something failed}\n"
        )

        report = parse_phase_report(stderr)

        self.assertEqual(report["peak_live_mb"], 1622.5)
        self.assertEqual(report["phases"][0]["phase"], "format")

    def test_malformed_report_is_distinguishable_from_missing_report(
        self,
    ) -> None:
        # A `{` is present -- this is not "no report" -- but the JSON
        # itself is broken (trailing comma). This must be loud, not
        # silently treated as "binary lacks the profiling feature".
        stderr = (
            '{"phases": [{"phase": "format", "seconds": 0.2,}],'
            ' "peak_live_mb": 10}\n'
        )

        with self.assertRaises(MalformedPhaseReportError) as context:
            parse_phase_report(stderr)

        self.assertNotIsInstance(context.exception, NoPhaseReportError)
        # Still a ValueError, per the documented interface.
        self.assertIsInstance(context.exception, ValueError)


if __name__ == "__main__":
    unittest.main()
