"""Real-file correctness gate: decompile bytecode captures and confirm the
output actually recompiles.

Real captures reach shapes and sizes the corpus fixtures never do. Two
classes of defect illustrate why that gap matters:

  - A method call `X.method(args)` reconstructed as a discarded field load
    plus a call to the plain table `X`. Calling a table is a runtime error,
    not a syntax error, so a compile check passes it. Catching that class of
    defect is the job of `audit_decompiled.py`, not this gate.
  - More than 200 live locals in one function, past Luau's register limit.
    Only a function large enough to reach that limit exercises it, and the
    corpus fixtures are all far too small.

This tool's only job is: decompile a real capture, then recompile the
result, and report whether the round trip actually works. It is not a
semantic checker -- passing here means "the output is valid Luau", not
"the output means the same thing as the input".

The decompiler binary writes generated Lua to stdout. A profiling build
additionally writes a JSON phase report to stderr on success, and every
build writes `decompiler error: ...` to stderr on failure. Both streams
are captured separately so neither contaminates the other: stdout is
always treated purely as candidate Lua source, stderr purely as
diagnostic text.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]

DEFAULT_DECOMPILER = Path("target/release/luau-lifter.exe")
DEFAULT_RECOMPILER = Path(".tools/luau-windows/luau-compile.exe")


@dataclass(frozen=True)
class GateResult:
    capture: Path
    input_bytes: int
    bytecode_version: int | None
    decompile_exit: int | None
    decompile_error: str | None
    output_lines: int
    output_path: Path | None
    recompile_exit: int | None
    first_compile_error: str | None

    @property
    def recompiles(self) -> bool:
        """True only when decompile succeeded and the result recompiled.

        `False` covers both a decompile failure and a decompile success
        followed by a recompile failure -- callers that need to tell those
        two apart should look at `decompile_exit` and `recompile_exit`
        directly rather than at this summary flag.
        """
        return self.decompile_exit == 0 and self.recompile_exit == 0


def _first_nonblank_line(text: str) -> str | None:
    for line in text.splitlines():
        stripped = line.strip()
        if stripped:
            return stripped
    return None


def _executable_prefix(executable: Path) -> tuple[str, ...]:
    """Allow a `.py` stand-in to be used wherever a compiled binary is
    expected, so tests can exercise this module's control flow with a
    small fake script instead of the real decompiler/recompiler."""
    if executable.suffix.casefold() == ".py":
        return (sys.executable, str(executable))
    return (str(executable),)


def _run(command: tuple[str, ...], workspace: Path) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        command,
        cwd=workspace,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def run_gate_one(
    capture: Path,
    decompiler: Path,
    recompiler: Path,
    workspace: Path = WORKSPACE,
    output_path: Path | None = None,
) -> GateResult:
    """Decompile one bytecode capture and confirm the output recompiles.

    `output_path`, if given, is where the decompiled Lua is saved even on
    a recompile failure -- callers that want to inspect or hand the output
    to the static auditor need the file to exist regardless of outcome.
    When omitted, a throwaway temp file is used for the recompile step and
    discarded afterward.
    """
    data = capture.read_bytes()
    input_bytes = len(data)
    bytecode_version = data[0] if data else None

    try:
        decompiled = _run(
            (*_executable_prefix(decompiler), str(capture)), workspace
        )
    except OSError as error:
        return GateResult(
            capture=capture,
            input_bytes=input_bytes,
            bytecode_version=bytecode_version,
            decompile_exit=None,
            decompile_error=str(error),
            output_lines=0,
            output_path=None,
            recompile_exit=None,
            first_compile_error=None,
        )

    decompile_exit = decompiled.returncode
    lua_source = decompiled.stdout.decode("utf-8", errors="replace")
    output_lines = len(lua_source.splitlines())

    saved_output: Path | None = None
    if output_path is not None:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(lua_source, encoding="utf-8")
        saved_output = output_path

    if decompile_exit != 0:
        decompile_stderr = decompiled.stderr.decode("utf-8", errors="replace")
        return GateResult(
            capture=capture,
            input_bytes=input_bytes,
            bytecode_version=bytecode_version,
            decompile_exit=decompile_exit,
            decompile_error=_first_nonblank_line(decompile_stderr),
            output_lines=output_lines,
            output_path=saved_output,
            recompile_exit=None,
            first_compile_error=None,
        )

    temp_handle = None
    lua_path = saved_output
    if lua_path is None:
        temp_handle = tempfile.NamedTemporaryFile(
            mode="w", suffix=".lua", delete=False, encoding="utf-8"
        )
        temp_handle.write(lua_source)
        temp_handle.close()
        lua_path = Path(temp_handle.name)

    try:
        recompiled = _run(
            (*_executable_prefix(recompiler), "--null", str(lua_path)),
            workspace,
        )
    finally:
        if temp_handle is not None:
            Path(temp_handle.name).unlink(missing_ok=True)

    recompile_stderr = recompiled.stderr.decode("utf-8", errors="replace")
    return GateResult(
        capture=capture,
        input_bytes=input_bytes,
        bytecode_version=bytecode_version,
        decompile_exit=decompile_exit,
        decompile_error=None,
        output_lines=output_lines,
        output_path=saved_output,
        recompile_exit=recompiled.returncode,
        first_compile_error=_first_nonblank_line(recompile_stderr),
    )


def _expand_captures(raw_paths: list[str]) -> list[Path]:
    """Resolve CLI arguments to concrete files, expanding glob patterns.

    Windows shells (PowerShell, cmd) do not expand `*` themselves the way
    POSIX shells do, so a bare glob argument like `captures/**/*.luac`
    would otherwise arrive here as a single literal, non-existent path.
    """
    expanded: list[Path] = []
    for raw in raw_paths:
        if any(character in raw for character in "*?["):
            base = Path(raw)
            # Split into the fixed prefix directory and the glob tail so
            # relative and absolute patterns both work under Path.glob.
            parts = Path(raw).parts
            anchor_index = 0
            for index, part in enumerate(parts):
                if any(character in part for character in "*?["):
                    anchor_index = index
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
                raise SystemExit(f"capture not found: {path}")
            expanded.append(path)
    return expanded


def _format_result_row(result: GateResult) -> str:
    version = (
        str(result.bytecode_version)
        if result.bytecode_version is not None
        else "-"
    )
    decompile = (
        str(result.decompile_exit) if result.decompile_exit is not None else "ERR"
    )
    recompile = (
        str(result.recompile_exit) if result.recompile_exit is not None else "-"
    )
    error = result.first_compile_error or result.decompile_error or ""
    status = "OK" if result.recompiles else "FAIL"
    return (
        f"| {result.capture} | {result.input_bytes} | {version} | "
        f"{decompile} | {result.output_lines} | {recompile} | {status} | {error} |"
    )


def print_report(results: list[GateResult]) -> None:
    print("| capture | bytes | bc version | decompile exit | output lines | "
          "recompile exit | status | first error |")
    print("| --- | ---: | ---: | ---: | ---: | ---: | --- | --- |")
    for result in results:
        print(_format_result_row(result))
    total = len(results)
    failed = sum(not result.recompiles for result in results)
    print(f"\n{total} capture(s), {failed} failing the recompile gate.")


def write_json_report(results: list[GateResult], path: Path) -> None:
    payload = {
        "totals": {
            "captures": len(results),
            "failed": sum(not result.recompiles for result in results),
        },
        "results": [
            {
                "capture": str(result.capture),
                "input_bytes": result.input_bytes,
                "bytecode_version": result.bytecode_version,
                "decompile_exit": result.decompile_exit,
                "decompile_error": result.decompile_error,
                "output_lines": result.output_lines,
                "output_path": (
                    str(result.output_path)
                    if result.output_path is not None
                    else None
                ),
                "recompile_exit": result.recompile_exit,
                "first_compile_error": result.first_compile_error,
                "recompiles": result.recompiles,
            }
            for result in results
        ],
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Decompile one or more bytecode captures and confirm the "
            "output actually recompiles. Never stops at the first "
            "failure -- every capture given is decompiled and reported."
        )
    )
    parser.add_argument(
        "captures",
        nargs="+",
        help="Bytecode files to gate. Glob patterns are accepted.",
    )
    parser.add_argument("--decompiler", type=Path, default=DEFAULT_DECOMPILER)
    parser.add_argument("--recompiler", type=Path, default=DEFAULT_RECOMPILER)
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help=(
            "Save each capture's decompiled Lua here as "
            "NNN_<stem>.lua, in the order given. Omit to discard output "
            "after the recompile check."
        ),
    )
    parser.add_argument(
        "--json-out",
        type=Path,
        default=None,
        help="Write a machine-readable summary to this path.",
    )
    arguments = parser.parse_args()

    captures = _expand_captures(arguments.captures)

    decompiler = (
        arguments.decompiler
        if arguments.decompiler.is_absolute()
        else WORKSPACE / arguments.decompiler
    ).resolve()
    recompiler = (
        arguments.recompiler
        if arguments.recompiler.is_absolute()
        else WORKSPACE / arguments.recompiler
    ).resolve()
    if not decompiler.exists():
        raise SystemExit(f"decompiler not found: {decompiler}")
    if not recompiler.exists():
        raise SystemExit(f"recompiler not found: {recompiler}")

    output_dir = arguments.output_dir
    if output_dir is not None:
        output_dir = (
            output_dir if output_dir.is_absolute() else WORKSPACE / output_dir
        )

    results: list[GateResult] = []
    for index, capture in enumerate(captures, start=1):
        output_path = (
            output_dir / f"{index:03d}_{capture.stem}.lua"
            if output_dir is not None
            else None
        )
        results.append(
            run_gate_one(
                capture.resolve(),
                decompiler,
                recompiler,
                WORKSPACE,
                output_path,
            )
        )

    print_report(results)
    if arguments.json_out is not None:
        json_path = (
            arguments.json_out
            if arguments.json_out.is_absolute()
            else WORKSPACE / arguments.json_out
        )
        write_json_report(results, json_path)
        print(f"\nWrote {json_path}")

    return 1 if any(not result.recompiles for result in results) else 0


if __name__ == "__main__":
    sys.exit(main())
