"""Native constructor write-order regressions with explicitly configured tools."""

import tempfile
import unittest
from pathlib import Path

from tests.python.test_multiple_results import configured_tools, run, source


def overwrite(initial: str, assignment: str, returned: str) -> str:
    return f"""local old = {{{initial}}}
local old_get = function() return old[1] end
local current = {{old[1]}}
local alias = current
{assignment}
local fresh_get = function() return current[1] end
return {returned}"""


EFFECT_WRITES = """local trace = ""
local function mark(tag, value)
    trace = trace .. tag
    return value
end
local current = {mark("A", "first"), mark("B", nil)}
current.label = mark("C", "named")
current[3] = mark("D", "third")
"""


CASES = (
    (
        "nil_overwrite",
        overwrite("nil", 'current[1] = "new"', "old_get(), current[1], fresh_get()"),
        "3|nil:nil|string:new|string:new",
    ),
    (
        "value_overwrite",
        overwrite('"old"', 'current[1] = "new"', "old_get(), current[1], fresh_get()"),
        "3|string:old|string:new|string:new",
    ),
    (
        "alias_overwrite",
        overwrite(
            "nil", 'alias[1] = "new"', "old_get(), current[1], alias[1], fresh_get()"
        ),
        "4|nil:nil|string:new|string:new|string:new",
    ),
    (
        "dynamic_overwrite",
        overwrite(
            '"old"',
            'current[tonumber("1")] = "new"',
            "old_get(), current[1], fresh_get()",
        ),
        "3|string:old|string:new|string:new",
    ),
    (
        "disjoint_append",
        overwrite('"old"', 'current[2] = "new"', "old_get(), current[1], current[2]"),
        "3|string:old|string:old|string:new",
    ),
    (
        "named_field",
        overwrite(
            "nil", 'current.label = "new"', "old_get(), current[1], current.label"
        ),
        "3|nil:nil|nil:nil|string:new",
    ),
    (
        "effect_before_return_snapshot",
        EFFECT_WRITES
        + "return trace, current[1], current[2], current[3], current.label",
        "5|string:ABCD|string:first|nil:nil|string:third|string:named",
    ),
    (
        "effect_before_argument_snapshot",
        EFFECT_WRITES
        + "return (function(before, value) return before, value end)(trace, current[3])",
        "2|string:ABCD|string:third",
    ),
    (
        "effect_after_literal_prefix",
        EFFECT_WRITES + 'return "constant", current[3], trace',
        "3|string:constant|string:third|string:ABCD",
    ),
)


class ConstructorWriteOrderTests(unittest.TestCase):
    def test_lua51_roundtrips(self) -> None:
        medal, compiler, runtime = configured_tools("MEDAL_LUA51", "LUAC51", "LUA51")
        with tempfile.TemporaryDirectory(prefix="medal-constructor-") as tmp:
            root = Path(tmp)
            for name, body, expected in CASES:
                with self.subTest(case=name):
                    original = root / f"{name}.lua"
                    original.write_text(source(body), encoding="utf-8")
                    chunk = root / f"{name}.luac"
                    run(compiler, "-s", "-o", chunk, original, cwd=root)
                    self.assertEqual(
                        run(runtime, chunk, cwd=root).decode().strip(), expected
                    )
                    run(medal, "--file", chunk, cwd=root)
                    rebuilt = root / f"{name}.roundtrip.luac"
                    run(compiler, "-o", rebuilt, root / f"{name}.dec.51.lua", cwd=root)
                    self.assertEqual(
                        run(runtime, rebuilt, cwd=root).decode().strip(), expected
                    )

    def test_luau_roundtrips(self) -> None:
        medal, compiler, runtime = configured_tools(
            "MEDAL_LUAU", "LUAU_COMPILE", "LUAU"
        )
        with tempfile.TemporaryDirectory(prefix="medal-constructor-") as tmp:
            root = Path(tmp)
            for name, body, expected in CASES:
                with self.subTest(case=name):
                    original = root / f"{name}.luau"
                    original.write_text(source(body), encoding="utf-8")
                    self.assertEqual(
                        run(runtime, original, cwd=root).decode().strip(), expected
                    )
                    chunk = root / f"{name}.luac"
                    chunk.write_bytes(
                        run(compiler, "--binary", "-O1", "-g0", original, cwd=root)
                    )
                    generated = root / f"{name}.roundtrip.luau"
                    generated.write_bytes(run(medal, chunk, cwd=root))
                    run(compiler, "--null", generated, cwd=root)
                    self.assertEqual(
                        run(runtime, generated, cwd=root).decode().strip(), expected
                    )


if __name__ == "__main__":
    unittest.main()
