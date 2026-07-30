from __future__ import annotations

import unittest

from tools.measure_decompiler import parse_phase_report


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
        with self.assertRaises(ValueError):
            parse_phase_report("no json here")


if __name__ == "__main__":
    unittest.main()
