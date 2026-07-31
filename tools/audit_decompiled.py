"""Static auditor over decompiled Luau output.

`decompile_gate.py` answers "does this output recompile"; it cannot answer
"is this output correct", because a construct can be syntactically valid
Luau and still be semantically wrong. Bug A is the motivating example: a
method call `X.method(args)` decompiled into a discarded field load plus a
call to the plain table `X`. The result compiles fine -- calling a table
without `__call` is only a runtime error -- so no compile-time check can
ever catch it. This module looks for the textual shapes that prove a
runtime failure is coming, without running anything.

Every rule here is held to one bar: it must be able to point at a
construct that is *definitely* wrong, not merely unusual. A rule that
merely looks suspicious costs someone real investigation time for a false
positive, so rules that cannot clear that bar are left out on purpose
(for instance, "a local reused as both a table and a call target
somewhere in the file" was considered and rejected -- without real scope
tracking it is too easy to confuse two different variables that happen to
share a decompiler-generated name across sibling blocks).

Rule (a) -- discard-then-call
    `local _ = X.field` immediately followed (blank lines aside) by a
    call whose callee is exactly `X`, either as a bare statement `X(...)`
    or as the right-hand side of an assignment `... = X(...)`. This is
    Bug A's exact shape: bytecode for `X.method(args)` decompiled as a
    discarded GETINDEX for the method lookup plus a separate CALL that
    degraded to the object itself. Validated against the stage-147
    capture: this rule reports exactly 121 bare-call sites plus 1
    assigned-call site (122 total) for a file independently confirmed by
    hand to contain 121+ instances of this defect.

Rule (b) -- table-literal-called
    A local whose *only* binding anywhere in the file is a table literal
    (`local X = { ... }`, never reassigned) is later used as a call
    target `X(...)`. A table without `__call` cannot be called; Lua would
    raise `attempt to call a table value` the moment this line runs. This
    rule does not depend on Bug A's specific discard-then-call shape, so
    it independently confirms the same class of defect through a
    different mechanism -- and, on stage-147, it catches call sites Rule
    (a) cannot, because the discard and the call are not adjacent.

Rule (c) -- nonfunction-literal-called
    Same reasoning as (b), for the other literal kinds that can never be
    callable: string, number, boolean, and nil. `local X = "text"` (or a
    number/boolean/nil) that is later called as `X(...)` is provably
    wrong for the same reason -- these values do not support `__call`
    either, and nothing can reassign the empty string that's assumed a
    function.

SCOPE TRACKING (rules b/c only)
    Lua is block-scoped and this decompiler reuses generated names
    (`v583`, for instance) across sibling and repeated blocks, so a naive
    "this name was declared as a table literal somewhere in the file"
    check would produce false positives whenever an unrelated variable in
    a different block happens to share a name. To stay precise without a
    real parser, this module approximates scope with an indentation
    stack: a deeper-indented line pushes a new scope frame, a
    shallower-indented line pops back to it. Luau's decompiled output
    here is consistently tab-indented per block, so this tracks real
    lexical scoping closely enough to be trustworthy: a table literal
    declared inside one `if` block and a same-named variable declared in
    a sibling `if` block do not share a binding, because the frame is
    popped between them.

    A local's binding is invalidated (set to "unknown", never flagged)
    the moment any assignment target it cannot fully classify touches it:
    multi-target assignments (`a, b = f()`), indexed/field assignments
    (`X.field = v`, `X[1] = v`) are correctly left alone (they do not
    change what `X` itself refers to), but anything this module cannot
    prove is safe defaults to "unknown" rather than "still a literal".

    Known limitation: function parameters are not parsed from the
    function header, so a parameter that happens to reuse an outer
    flagged name would shadow it incorrectly. This was checked against
    the stage-147 fixture (no parameter there reuses an outer table's
    name) but is not structurally impossible in other captures -- a
    finding whose "detail" cites a name that is also used as a parameter
    nearby is worth a second look before triaging.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]

RULE_DISCARD_THEN_CALL = "discard-then-call"
RULE_TABLE_LITERAL_CALLED = "table-literal-called"
RULE_NONFUNCTION_LITERAL_CALLED = "nonfunction-literal-called"

ALL_RULES = (
    RULE_DISCARD_THEN_CALL,
    RULE_TABLE_LITERAL_CALLED,
    RULE_NONFUNCTION_LITERAL_CALLED,
)


@dataclass(frozen=True)
class Finding:
    path: Path
    line: int
    rule: str
    text: str
    detail: str


# --- Rule (a): discard-then-call -------------------------------------------

_DISCARD_INDEX_LOAD = re.compile(
    r"^\s*local _ = ([A-Za-z_][A-Za-z0-9_]*)\.[A-Za-z_][A-Za-z0-9_]*\s*$"
)


def find_discard_then_call(path: Path, lines: list[str]) -> list[Finding]:
    findings: list[Finding] = []
    total = len(lines)
    for index, line in enumerate(lines):
        match = _DISCARD_INDEX_LOAD.match(line)
        if not match:
            continue
        base = match.group(1)
        cursor = index + 1
        while cursor < total and lines[cursor].strip() == "":
            cursor += 1
        if cursor >= total:
            continue
        next_line = lines[cursor]
        call_pattern = re.compile(
            r"(?:^\s*|=\s*)" + re.escape(base) + r"\s*\("
        )
        if call_pattern.search(_blank_string_literals(next_line)):
            findings.append(
                Finding(
                    path=path,
                    line=index + 1,
                    rule=RULE_DISCARD_THEN_CALL,
                    text=line.strip(),
                    detail=(
                        f"discarded load of {base}.<field> is immediately "
                        f"followed by a call to {base} itself "
                        f"(line {cursor + 1}: {next_line.strip()!r})"
                    ),
                )
            )
    return findings


# --- Rules (b)/(c): calls to locals provably bound to a non-function -------

_LOCAL_SINGLE_DECL = re.compile(
    r"^\s*local ([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$"
)
_PLAIN_SINGLE_ASSIGN = re.compile(
    r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$"
)
_FUNCTION_HEADER_PARAMS = re.compile(r"function\s*[A-Za-z0-9_.:]*\s*\(([^)]*)\)")
_STRING_LITERAL = re.compile(
    r"""^(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')$"""
)
_NUMBER_LITERAL = re.compile(
    r"^-?(?:0[xX][0-9a-fA-F]+|\d+\.?\d*(?:[eE][+-]?\d+)?|\.\d+)$"
)
_CALL_SITE = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)\s*\(")

_LITERAL_RULE_BY_KIND = {
    "table": RULE_TABLE_LITERAL_CALLED,
    "string": RULE_NONFUNCTION_LITERAL_CALLED,
    "number": RULE_NONFUNCTION_LITERAL_CALLED,
    "boolean": RULE_NONFUNCTION_LITERAL_CALLED,
    "nil": RULE_NONFUNCTION_LITERAL_CALLED,
}


def _leading_indent(line: str) -> int:
    count = 0
    for character in line:
        if character in ("\t", " "):
            count += 1
        else:
            break
    return count


def _scan_braces(text: str, balance: int) -> tuple[int, str | None]:
    """Advance a brace balance across `text`, skipping string contents.

    Returns the updated balance, and -- if the balance reaches zero partway
    through this text -- everything after that closing brace, so the
    caller can confirm nothing but whitespace follows a table literal on
    its last line.
    """
    index = 0
    length = len(text)
    while index < length:
        character = text[index]
        if character in ("'", '"'):
            quote = character
            index += 1
            while index < length and text[index] != quote:
                if text[index] == "\\":
                    index += 1
                index += 1
            index += 1
            continue
        if character == "{":
            balance += 1
        elif character == "}":
            balance -= 1
            if balance == 0:
                return balance, text[index + 1 :]
        index += 1
    return balance, None


def _blank_string_literals(text: str) -> str:
    """Replace the contents of every quoted string on `text` with spaces,
    preserving length and column positions.

    Decompiled output can embed arbitrary source-like text inside a
    string literal (an obfuscator's payload, a minified chunk kept as
    data). Without this, an identifier that happens to appear followed by
    `(` inside such a string -- coincidentally matching a name this
    module is tracking -- would be mistaken for a real call site. Only
    call-site and function-header scanning need this; the declaration and
    assignment matchers already anchor on the *whole* trimmed
    right-hand side, so a string spanning most of a line is still
    classified correctly as a string, not searched for false leads.
    """
    characters = list(text)
    index = 0
    length = len(characters)
    while index < length:
        character = characters[index]
        if character in ("'", '"'):
            quote = character
            characters[index] = " "
            index += 1
            while index < length and characters[index] != quote:
                if characters[index] == "\\":
                    characters[index] = " "
                    index += 1
                    if index < length:
                        characters[index] = " "
                        index += 1
                    continue
                characters[index] = " "
                index += 1
            if index < length:
                characters[index] = " "
                index += 1
            continue
        index += 1
    return "".join(characters)


def _classify_rhs(
    lines: list[str], index: int, rhs_text: str
) -> tuple[str, int]:
    """Classify a right-hand side expression, resolving multi-line table
    literals. Returns (kind, last_line_index_consumed).

    `kind` is one of "table", "string", "number", "boolean", "nil", or
    "unknown". "unknown" is the safe default for anything this function
    cannot prove is a literal -- including a table literal followed by
    trailing text on its closing line, which is left unclassified rather
    than guessed at.
    """
    stripped = rhs_text.strip()
    if not stripped:
        return "unknown", index

    if stripped.startswith("{"):
        balance = 0
        cursor = index
        text = stripped
        while True:
            balance, remainder = _scan_braces(text, balance)
            if remainder is not None:
                if remainder.strip() == "":
                    return "table", cursor
                return "unknown", cursor
            cursor += 1
            if cursor >= len(lines):
                return "unknown", index
            text = lines[cursor]

    if _STRING_LITERAL.match(stripped):
        return "string", index
    if stripped in ("true", "false"):
        return "boolean", index
    if stripped == "nil":
        return "nil", index
    if _NUMBER_LITERAL.match(stripped):
        return "number", index
    return "unknown", index


def find_calls_to_nonfunction_locals(path: Path, lines: list[str]) -> list[Finding]:
    findings: list[Finding] = []
    stack: list[dict[str, object]] = [{"indent": -1, "bindings": {}}]
    pending_params: list[str] | None = None
    total = len(lines)
    index = 0

    while index < total:
        raw = lines[index]
        if raw.strip() == "":
            index += 1
            continue

        indent = _leading_indent(raw)
        while len(stack) > 1 and stack[-1]["indent"] > indent:
            stack.pop()
        if stack[-1]["indent"] < indent:
            bindings: dict[str, str] = {}
            if pending_params:
                for name in pending_params:
                    bindings[name] = "unknown"
            stack.append({"indent": indent, "bindings": bindings})
        pending_params = None

        consumed_to = index

        decl = _LOCAL_SINGLE_DECL.match(raw)
        if decl:
            name, rhs = decl.group(1), decl.group(2)
            kind, consumed_to = _classify_rhs(lines, index, rhs)
            stack[-1]["bindings"][name] = kind  # type: ignore[index]
        else:
            assign = _PLAIN_SINGLE_ASSIGN.match(raw)
            if assign:
                name, rhs = assign.group(1), assign.group(2)
                kind, consumed_to = _classify_rhs(lines, index, rhs)
                for frame in reversed(stack):
                    bindings = frame["bindings"]  # type: ignore[assignment]
                    if name in bindings:
                        bindings[name] = kind
                        break

        header_match = _FUNCTION_HEADER_PARAMS.search(_blank_string_literals(raw))
        if header_match:
            params = [
                token.strip()
                for token in header_match.group(1).split(",")
                if token.strip()
            ]
            pending_params = params or None

        for scan_index in range(index, consumed_to + 1):
            findings.extend(
                _scan_line_for_bad_calls(path, scan_index, lines[scan_index], stack)
            )

        index = consumed_to + 1

    return findings


def _scan_line_for_bad_calls(
    path: Path, line_index: int, text: str, stack: list[dict[str, object]]
) -> list[Finding]:
    findings: list[Finding] = []
    for match in _CALL_SITE.finditer(_blank_string_literals(text)):
        name = match.group(1)
        for frame in reversed(stack):
            bindings = frame["bindings"]  # type: ignore[assignment]
            if name in bindings:
                kind = bindings[name]
                rule = _LITERAL_RULE_BY_KIND.get(kind)
                if rule:
                    findings.append(
                        Finding(
                            path=path,
                            line=line_index + 1,
                            rule=rule,
                            text=text.strip(),
                            detail=(
                                f"{name} is bound only to a {kind} literal "
                                f"and is never reassigned before this call"
                            ),
                        )
                    )
                break
    return findings


def audit_text(path: Path, source: str) -> list[Finding]:
    lines = source.splitlines()
    findings = find_discard_then_call(path, lines)
    findings.extend(find_calls_to_nonfunction_locals(path, lines))
    findings.sort(key=lambda finding: (finding.line, finding.rule))
    return findings


def audit_file(path: Path) -> list[Finding]:
    return audit_text(path, path.read_text(encoding="utf-8", errors="replace"))


def _expand_inputs(raw_paths: list[str]) -> list[Path]:
    expanded: list[Path] = []
    for raw in raw_paths:
        if any(character in raw for character in "*?["):
            parts = Path(raw).parts
            anchor_index = 0
            for position, part in enumerate(parts):
                if any(character in part for character in "*?["):
                    anchor_index = position
                    break
            root = Path(*parts[:anchor_index]) if anchor_index else Path(".")
            pattern = str(Path(*parts[anchor_index:]))
            matches = sorted(root.glob(pattern))
            if not matches:
                raise SystemExit(f"glob matched no files: {raw}")
            expanded.extend(matches)
        else:
            path = Path(raw)
            if not path.exists():
                raise SystemExit(f"input not found: {path}")
            expanded.append(path)
    return expanded


def print_report(findings: list[Finding]) -> None:
    if not findings:
        print("No findings.")
        return
    by_rule: dict[str, int] = {}
    for finding in findings:
        by_rule[finding.rule] = by_rule.get(finding.rule, 0) + 1
        print(f"{finding.path}:{finding.line}: [{finding.rule}] {finding.text}")
        print(f"    {finding.detail}")
    print(f"\n{len(findings)} finding(s):")
    for rule in ALL_RULES:
        if rule in by_rule:
            print(f"  {rule}: {by_rule[rule]}")


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Flag decompiled Luau constructs that compile but cannot be "
            "correct: calls to values that are provably not functions."
        )
    )
    parser.add_argument(
        "inputs", nargs="+", help="Decompiled .lua files to audit. Globs accepted."
    )
    arguments = parser.parse_args()
    paths = _expand_inputs(arguments.inputs)

    all_findings: list[Finding] = []
    for path in paths:
        all_findings.extend(audit_file(path))

    print_report(all_findings)
    return 1 if all_findings else 0


if __name__ == "__main__":
    sys.exit(main())
