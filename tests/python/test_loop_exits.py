"""Native nested-loop exit checks with explicitly configured tools."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from tests.python.test_multiple_results import configured_tools, source

NESTED_EXITS = """local function nested(n, early)
    local exit
    while true do
        if n >= 3 then exit = 1; break end
        local inner_exit
        while true do
            if n * 10 >= 30 then inner_exit = 2; break end
            n = n + 1
            if early then inner_exit = 1; break end
        end
        if inner_exit == 1 then exit = 2; break end
    end
    if exit == 1 then return n, nil, "normal" end
    if exit == 2 then return n, nil, "early" end
end
local a, b, c = nested(1, false)
local d, e, f = nested(1, true)
return a, b, c, d, e, f"""

CAPTURED_EXITS = """local saved = {}
local function prepare(n) return n * 10 end
local function step(n) return n + 1 end
local function inner(n) return n < 30 end
local function outer(n) return n < 3 end
local function stop(early) return early end
local function nested(n, early)
    local exit
    while true do
        if not outer(n) then exit = 1; break end
        local inner_exit
        while true do
            local condition
            do
                local n = prepare(n)
                saved[#saved + 1] = function() return n end
                condition = inner(n)
            end
            if not condition then inner_exit = 2; break end
            n = step(n)
            if stop(early) then inner_exit = 1; break end
        end
        if inner_exit == 1 then exit = 2; break end
    end
    if exit == 1 then return n, nil, "normal" end
    if exit == 2 then return n, nil, "early" end
end
local a, b, c = nested(1, false)
local first = saved[1]
local d, e, f = nested(1, true)
return a, b, c, d, e, f, saved[1](), saved[2](), saved[3](), saved[4](), first == saved[1], first ~= saved[4], #saved"""

CASES = (
    (
        "nested_exits",
        NESTED_EXITS,
        "6|number:3|nil:nil|string:normal|number:2|nil:nil|string:early",
    ),
    (
        "captured_exits",
        CAPTURED_EXITS,
        "13|number:3|nil:nil|string:normal|number:2|nil:nil|string:early|number:10|number:20|number:30|number:10|boolean:true|boolean:true|number:4",
    ),
    (
        "inner_break",
        "local n = 0; while n < 3 do while true do n = n + 1; break end end; return n",
        "1|number:3",
    ),
)


def run(*command: str | Path, cwd: Path, runtime: bool = False) -> bytes:
    result = subprocess.run(
        [str(arg) for arg in command],
        cwd=cwd,
        capture_output=True,
        timeout=3 if runtime else 5,
        check=False,
    )
    if result.returncode:
        raise AssertionError(result.stderr.decode("utf-8", errors="replace"))
    return result.stdout


class LoopExitTests(unittest.TestCase):
    def test_luau_roundtrips(self) -> None:
        medal, compiler, runtime = configured_tools(
            "MEDAL_LUAU", "LUAU_COMPILE", "LUAU"
        )
        with tempfile.TemporaryDirectory(prefix="medal-loop-exits-") as tmp:
            root = Path(tmp)
            for name, body, expected in CASES:
                with self.subTest(case=name):
                    original = root / f"{name}.luau"
                    original.write_text(source(body), encoding="utf-8")
                    self.assertEqual(
                        run(runtime, original, cwd=root, runtime=True).decode().strip(),
                        expected,
                    )
                    chunk = root / f"{name}.luac"
                    chunk.write_bytes(
                        run(
                            compiler,
                            "--binary",
                            "-O0",
                            "-g2",
                            "--fflags=LuauBytecodeCostModel=false,LuauEmitCallFeedback=false,DebugLuauUserDefinedClasses=false",
                            original,
                            cwd=root,
                        )
                    )
                    generated = root / f"{name}.roundtrip.luau"
                    generated.write_bytes(run(medal, chunk, cwd=root))
                    run(compiler, "--null", generated, cwd=root)
                    self.assertEqual(
                        run(runtime, generated, cwd=root, runtime=True)
                        .decode()
                        .strip(),
                        expected,
                    )

    def test_lua51_roundtrips(self) -> None:
        medal, compiler, runtime = configured_tools("MEDAL_LUA51", "LUAC51", "LUA51")
        with tempfile.TemporaryDirectory(prefix="medal-loop-exits-") as tmp:
            root = Path(tmp)
            for name, body, expected in CASES:
                with self.subTest(case=name):
                    original = root / f"{name}.lua"
                    original.write_text(source(body), encoding="utf-8")
                    chunk = root / f"{name}.luac"
                    run(compiler, "-s", "-o", chunk, original, cwd=root)
                    self.assertEqual(
                        run(runtime, chunk, cwd=root, runtime=True).decode().strip(),
                        expected,
                    )
                    run(medal, "--file", chunk, cwd=root)
                    generated = root / f"{name}.dec.51.lua"
                    rebuilt = root / f"{name}.roundtrip.luac"
                    run(compiler, "-o", rebuilt, generated, cwd=root)
                    self.assertEqual(
                        run(runtime, rebuilt, cwd=root, runtime=True).decode().strip(),
                        expected,
                    )


if __name__ == "__main__":
    unittest.main()
