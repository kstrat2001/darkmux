"""Baseline tests for src.parser. Operator-curated set."""

import os
import sys
import unittest

# Make `src/` importable without setup.py.
sys.path.insert(0, os.path.join(os.path.dirname(__file__), os.pardir))

from src.parser import parse_line  # noqa: E402


class TestParseLine(unittest.TestCase):
    def test_basic_well_formed_line(self):
        result = parse_line("2026-05-29T14:30:01  INFO  user login succeeded")
        self.assertEqual(
            result,
            {
                "timestamp": "2026-05-29T14:30:01",
                "level": "INFO",
                "message": "user login succeeded",
            },
        )

    def test_blank_line_returns_none(self):
        self.assertIsNone(parse_line(""))
        self.assertIsNone(parse_line("   "))

    def test_message_can_contain_spaces(self):
        result = parse_line("2026-05-29T14:30:01  WARN  retry 2 of 5")
        self.assertEqual(result["message"], "retry 2 of 5")

    def test_unparseable_line_returns_none(self):
        self.assertIsNone(parse_line("not a log line"))

    def test_single_space_separation_fails(self):
        # Parser requires 2+ spaces between fields.
        self.assertIsNone(parse_line("2026-05-29T14:30:01 INFO user login"))


if __name__ == "__main__":
    unittest.main()
