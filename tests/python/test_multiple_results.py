"""Multiple-result round trips using tools configured through environment variables."""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

# Each tuple records the result count, including nil slots, then typed values.
CASES = (
    (
        "pcall_fixed",
        'local a, b = pcall(type, "value"); return a, b',
        "2|boolean:true|string:string",
    ),
    (
        "nil_packet",
        'local a, b, c = select(2, "a", nil, "c", nil); return a, b, c',
        "3|nil:nil|string:c|nil:nil",
    ),
    (
        "vararg_fixed",
        "local a, b, c, d = ...; return a, b, c, d",
        "4|string:left|nil:nil|string:right|nil:nil",
    ),
    (
        "call_final",
        'return 9, select(2, "a", nil, "c", nil)',
        "4|number:9|nil:nil|string:c|nil:nil",
    ),
    ("call_nonfinal", 'return select(2, "a", nil, "c", nil), 9', "2|nil:nil|number:9"),
    ("call_scalar", 'return (pcall(type, "value"))', "1|boolean:true"),
    (
        "call_scalar_padding",
        'local a, b = (pcall(type, "value")); return a, b',
        "2|boolean:true|nil:nil",
    ),
    (
        "vararg_final",
        "return 9, ...",
        "5|number:9|string:left|nil:nil|string:right|nil:nil",
    ),
    ("vararg_nonfinal", "return ..., 9", "2|string:left|number:9"),
    ("vararg_scalar", "return (...)", "1|string:left"),
    (
        "vararg_scalar_padding",
        "local a, b = (...); return a, b",
        "2|string:left|nil:nil",
    ),
    (
        "call_argument_final",
        'return select("#", 9, pcall(type, "value"))',
        "1|number:3",
    ),
    (
        "call_argument_nonfinal",
        'return select("#", pcall(type, "value"), 9)',
        "1|number:2",
    ),
    (
        "call_argument_scalar",
        'return select("#", (pcall(type, "value")))',
        "1|number:1",
    ),
    ("vararg_argument_final", 'return select("#", 9, ...)', "1|number:5"),
    ("vararg_argument_nonfinal", 'return select("#", ..., 9)', "1|number:2"),
    ("vararg_argument_scalar", 'return select("#", (...))', "1|number:1"),
    (
        "call_table_final",
        'local t = {pcall(type, "value")}; return t[1], t[2]',
        "2|boolean:true|string:string",
    ),
    (
        "call_table_scalar",
        'local t = {(pcall(type, "value"))}; return t[1], t[2]',
        "2|boolean:true|nil:nil",
    ),
    (
        "method_fixed",
        'local object = {produce = function(self) return nil, "middle", nil end}; local a, b, c = object:produce(); return a, b, c',
        "3|nil:nil|string:middle|nil:nil",
    ),
    (
        "method_scalar",
        'local object = {produce = function(self) return nil, "middle", nil end}; return (object:produce())',
        "1|nil:nil",
    ),
    (
        "nested_closure_method_packet",
        """local receiver = {
    count = 5,
    bump = function(self, amount)
        self.count = self.count + amount
        return self.count
    end,
}
local a, b, c = (function(...)
    local values = {...}
    local first, second = (function(value)
        return value, value + 2
    end)(values[1])
    return receiver:bump(first), second, #values
end)(4, 6, 8)
return a, b, c, receiver.count""",
        "4|number:9|number:6|number:3|number:9",
    ),
    (
        "nested_closure_nil_packet",
        """local a, b, c = (function(...)
    local first, middle, last = (function(...) return ... end)(...)
    return first, middle, last
end)(nil, "middle", nil)
return a, b, c""",
        "3|nil:nil|string:middle|nil:nil",
    ),
    (
        "closure_scalar_packet",
        "local a, b = ((function() return 7, 11 end)()); return a, b",
        "2|number:7|nil:nil",
    ),
)


def configured_tools(*names: str) -> tuple[Path, ...]:
    missing = [name for name in names if not os.environ.get(name)]
    if missing:
        raise unittest.SkipTest("configure native tools: " + ", ".join(missing))
    paths = tuple(Path(os.environ[name]).resolve() for name in names)
    for path in paths:
        if not path.is_file():
            raise FileNotFoundError(path)
    return paths


def run(*command: str | Path, cwd: Path) -> bytes:
    result = subprocess.run(
        [str(arg) for arg in command],
        cwd=cwd,
        capture_output=True,
        timeout=30,
        check=False,
    )
    if result.returncode:
        raise AssertionError(
            f"{command!r} exited {result.returncode}: "
            + result.stderr.decode("utf-8", errors="replace")
        )
    return result.stdout


def source(body: str) -> str:
    return f"""local function subject(...)
{body}
end
local function observe(...)
    local text = tostring(select("#", ...))
    for i = 1, select("#", ...) do
        local value = select(i, ...)
        text = text .. "|" .. type(value) .. ":" .. tostring(value)
    end
    print(text)
end
observe(subject("left", nil, "right", nil))
"""


class Lua51MultipleResultsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.medal, cls.compiler, cls.runtime = configured_tools(
            "MEDAL_LUA51",
            "LUAC51",
            "LUA51",
        )

    def test_native_roundtrips(self) -> None:
        with tempfile.TemporaryDirectory(prefix="medal-multireturn-lua51-") as tmp:
            root = Path(tmp)
            for name, body, expected in CASES:
                with self.subTest(case=name):
                    original = root / f"{name}.lua"
                    original.write_text(source(body), encoding="utf-8")
                    chunk = root / f"{name}.luac"
                    run(self.compiler, "-s", "-o", chunk, original, cwd=root)
                    self.assertEqual(
                        run(self.runtime, chunk, cwd=root).decode().strip(), expected
                    )
                    run(self.medal, "--file", chunk, cwd=root)
                    generated = root / f"{name}.dec.51.lua"
                    rebuilt = root / f"{name}.roundtrip.luac"
                    run(self.compiler, "-o", rebuilt, generated, cwd=root)
                    self.assertEqual(
                        run(self.runtime, rebuilt, cwd=root).decode().strip(), expected
                    )


class LuauMultipleResultsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.medal, cls.compiler, cls.runtime = configured_tools(
            "MEDAL_LUAU",
            "LUAU_COMPILE",
            "LUAU",
        )

    def test_native_roundtrips(self) -> None:
        with tempfile.TemporaryDirectory(prefix="medal-multireturn-luau-") as tmp:
            root = Path(tmp)
            for name, body, expected in CASES:
                with self.subTest(case=name):
                    original = root / f"{name}.luau"
                    original.write_text(source(body), encoding="utf-8")
                    self.assertEqual(
                        run(self.runtime, original, cwd=root).decode().strip(), expected
                    )
                    chunk = root / f"{name}.luac"
                    chunk.write_bytes(
                        run(self.compiler, "--binary", "-O1", "-g0", original, cwd=root)
                    )
                    generated = root / f"{name}.roundtrip.luau"
                    generated.write_bytes(run(self.medal, chunk, cwd=root))
                    run(self.compiler, "--null", generated, cwd=root)
                    self.assertEqual(
                        run(self.runtime, generated, cwd=root).decode().strip(),
                        expected,
                    )


if __name__ == "__main__":
    unittest.main()
