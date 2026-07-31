from __future__ import annotations

import os
import textwrap
import unittest
from pathlib import Path

from tools.audit_decompiled import (
    RULE_DISCARD_THEN_CALL,
    RULE_NONFUNCTION_LITERAL_CALLED,
    RULE_TABLE_LITERAL_CALLED,
    audit_text,
)

DUMMY_PATH = Path("dummy.lua")


def _rules(source: str) -> list[str]:
    return [finding.rule for finding in audit_text(DUMMY_PATH, textwrap.dedent(source))]


class DiscardThenCallTests(unittest.TestCase):
    """Rule (a): a discarded field load immediately
    followed by a call to the same base name."""

    def test_bare_call_is_flagged(self) -> None:
        source = """
            local _ = v4.Start
            v4(v4)
        """
        findings = audit_text(DUMMY_PATH, textwrap.dedent(source))

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].rule, RULE_DISCARD_THEN_CALL)
        self.assertEqual(findings[0].line, 2)
        self.assertEqual(findings[0].text, "local _ = v4.Start")

    def test_assigned_call_is_flagged(self) -> None:
        source = """
            local _ = v584.GetAttribute
            local v585 = v584(v584, "key")
        """
        self.assertEqual(_rules(source), [RULE_DISCARD_THEN_CALL])

    def test_blank_lines_between_discard_and_call_are_skipped(self) -> None:
        source = """
            local _ = v4.Start

            v4(v4)
        """
        self.assertEqual(_rules(source), [RULE_DISCARD_THEN_CALL])

    def test_correct_method_call_is_not_flagged(self) -> None:
        # This is what the decompiler should emit: no discarded load at
        # all, just a direct method-shaped call.
        source = """
            v4.Start(v4)
        """
        self.assertEqual(_rules(source), [])

    def test_discard_not_immediately_followed_by_matching_call_is_not_flagged(
        self,
    ) -> None:
        # The discard is real, but the very next statement calls a
        # different name -- this is not the defect's shape, and the file's own
        # ground truth (v4's alias-then-call sequence) shows the discard
        # here can be an unrelated, harmless leftover.
        source = """
            local _ = v4.Stop
            v234 = v234
            v4(v234, v242)
        """
        self.assertEqual(_rules(source), [])

    def test_call_to_a_different_base_is_not_flagged(self) -> None:
        source = """
            local _ = v4.Start
            v5(v5)
        """
        self.assertEqual(_rules(source), [])

    def test_call_text_inside_a_string_literal_is_not_flagged(self) -> None:
        # The follow-up line calls nothing -- it merely contains the text
        # "v4(" as part of an unrelated string payload.
        source = """
            local _ = v4.Start
            local msg = "error near v4(v4)"
        """
        self.assertEqual(_rules(source), [])

    def test_discard_of_named_local_other_than_underscore_is_not_flagged(
        self,
    ) -> None:
        # Only the exact "_" discard idiom is the defect's signature; a real
        # named local holding a field is a legitimate value, not a
        # discard, even if something calls the same base right after.
        source = """
            local callee = v4.Start
            v4(v4)
        """
        self.assertEqual(_rules(source), [])


class TableLiteralCalledTests(unittest.TestCase):
    """Rule (b): a local whose only binding anywhere is a table literal,
    later used as a call target."""

    def test_empty_table_literal_called_is_flagged(self) -> None:
        source = """
            local v4 = {}
            v4.Start = function() end
            v4(v4)
        """
        self.assertEqual(_rules(source), [RULE_TABLE_LITERAL_CALLED])

    def test_multiline_table_literal_called_is_flagged(self) -> None:
        source = """
            local v5 = {
                name = "warn",
                value = 1,
            }
            v5()
        """
        self.assertEqual(_rules(source), [RULE_TABLE_LITERAL_CALLED])

    def test_single_element_box_literal_called_is_flagged(self) -> None:
        # An upvalue-style box `{ x }` called directly instead of indexed:
        # the call should have gone through the boxed value, not the box.
        source = """
            local v573 = { v571 }
            v573(p1)
        """
        self.assertEqual(_rules(source), [RULE_TABLE_LITERAL_CALLED])

    def test_reassigned_local_is_not_flagged(self) -> None:
        source = """
            local t = {}
            t = getFunction()
            t()
        """
        self.assertEqual(_rules(source), [])

    def test_field_assignment_does_not_clear_the_table_binding(self) -> None:
        # `t.field = x` mutates the table `t` points to; it does not
        # change what `t` itself refers to, so `t` is still a table and
        # calling it is still wrong.
        source = """
            local t = {}
            t.field = 5
            t()
        """
        self.assertEqual(_rules(source), [RULE_TABLE_LITERAL_CALLED])

    def test_indexed_assignment_does_not_clear_the_table_binding(self) -> None:
        source = """
            local t = {}
            t[1] = 5
            t()
        """
        self.assertEqual(_rules(source), [RULE_TABLE_LITERAL_CALLED])

    def test_sibling_blocks_reusing_a_name_do_not_cross_contaminate(self) -> None:
        # `t` in the first `if` is a table and is (correctly) flagged.
        # `t` in the second `if` is a distinct variable bound to a real
        # function-returning call and must not be flagged just because an
        # unrelated sibling block used the same generated name.
        source = """
            if a then
                local t = {}
                t()
            end
            if b then
                local t = getFunction()
                t()
            end
        """
        self.assertEqual(_rules(source), [RULE_TABLE_LITERAL_CALLED])

    def test_calling_a_field_of_a_table_literal_local_is_not_flagged(self) -> None:
        # `t.Start(t)` is exactly the correct shape the defect should have
        # produced; the call target here is the field, not `t` itself.
        source = """
            local t = {}
            t.Start = function() end
            t.Start(t)
        """
        self.assertEqual(_rules(source), [])

    def test_identifier_inside_an_unrelated_string_literal_is_not_flagged(
        self,
    ) -> None:
        # A decompiled string can embed source-like text (an obfuscator's
        # payload, for instance). "q(" appearing inside that string must
        # not be mistaken for a real call to a tracked local named q.
        source = """
            local q = {}
            v3[15] = "local n,o,p,q,r,s = 1 q(2) end"
        """
        self.assertEqual(_rules(source), [])

    def test_table_literal_with_trailing_expression_is_not_classified(self) -> None:
        # Not valid Lua on its own, but defensively: if a table literal's
        # closing brace is not the end of the statement, this module must
        # not guess that the whole thing is still "just a table".
        source = """
            local t = {} or fallback()
            t()
        """
        self.assertEqual(_rules(source), [])


class NonfunctionLiteralCalledTests(unittest.TestCase):
    """Rule (c): a local bound only to a string/number/boolean/nil
    literal, later called."""

    def test_string_literal_called_is_flagged(self) -> None:
        source = """
            local s = "not a function"
            s()
        """
        self.assertEqual(_rules(source), [RULE_NONFUNCTION_LITERAL_CALLED])

    def test_number_literal_called_is_flagged(self) -> None:
        source = """
            local n = 42
            n()
        """
        self.assertEqual(_rules(source), [RULE_NONFUNCTION_LITERAL_CALLED])

    def test_boolean_literal_called_is_flagged(self) -> None:
        source = """
            local b = false
            b()
        """
        self.assertEqual(_rules(source), [RULE_NONFUNCTION_LITERAL_CALLED])

    def test_nil_literal_called_is_flagged(self) -> None:
        source = """
            local n = nil
            n()
        """
        self.assertEqual(_rules(source), [RULE_NONFUNCTION_LITERAL_CALLED])

    def test_alias_of_a_literal_is_not_flagged(self) -> None:
        # `b` is bound to the *identifier* `a`, not to a literal directly.
        # This module does not chase aliases -- it only proves a binding
        # wrong when the right-hand side is a literal token it can read
        # right there, so this is correctly left unflagged rather than
        # guessed at.
        source = """
            local a = "text"
            local b = a
            b()
        """
        self.assertEqual(_rules(source), [])

    def test_function_call_result_is_not_flagged(self) -> None:
        source = """
            local f = getCallback()
            f()
        """
        self.assertEqual(_rules(source), [])

    def test_function_literal_is_not_flagged(self) -> None:
        source = """
            local f = function() end
            f()
        """
        self.assertEqual(_rules(source), [])


class FunctionParameterShadowTests(unittest.TestCase):
    def test_parameter_reusing_an_outer_flagged_name_is_not_flagged(self) -> None:
        source = """
            local v4 = {}
            v4.Run = function(v4)
                v4()
            end
        """
        self.assertEqual(_rules(source), [])


class RealFixtureRegressionTest(unittest.TestCase):
    """Checks the auditor against decompiled output known to contain the
    discarded-lookup defect, to confirm the rule fires at scale and not
    just on the hand-written shapes above. Skipped unless that output has
    been staged locally, since the capture lives outside the repository."""

    def test_known_bad_output_reports_many_discard_then_call_sites(self) -> None:
        path_text = os.environ.get("MEDAL_DISCARDED_LOOKUP_SAMPLE")
        if not path_text:
            self.skipTest(
                "set MEDAL_DISCARDED_LOOKUP_SAMPLE to decompiled .lua output "
                "containing the defect to run this check"
            )
        path = Path(path_text)
        findings = audit_text(path, path.read_text(encoding="utf-8"))
        discard_then_call = [
            finding for finding in findings if finding.rule == RULE_DISCARD_THEN_CALL
        ]

        self.assertGreaterEqual(len(discard_then_call), 121)


if __name__ == "__main__":
    unittest.main()
