from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

from .model import (
    CaseResult,
    CompileProfile,
    ReadabilityMetrics,
    RunResult,
    RuntimeResult,
)
from .semantic import (
    TRUSTED_SEMANTIC_PROBES,
    run_semantic_probe,
)


def _executable_prefix(executable: Path) -> tuple[str, ...]:
    if executable.suffix.casefold() == ".py":
        return (sys.executable, str(executable))
    return (str(executable),)


def compiler_command(
    compiler: Path,
    source: Path,
    profile: CompileProfile,
    mode: str,
) -> tuple[str, ...]:
    if mode not in {"binary", "null"}:
        raise ValueError(f"unsupported compiler mode: {mode}")
    fast_flags = (
        (f"--fflags={','.join(profile.fast_flags)}",)
        if profile.fast_flags
        else ()
    )
    return (
        *_executable_prefix(compiler),
        f"--{mode}",
        f"-O{profile.optimization}",
        f"-g{profile.debug}",
        *fast_flags,
        str(source),
    )


def decompiler_command(decompiler: Path, bytecode: Path) -> tuple[str, ...]:
    return (*_executable_prefix(decompiler), str(bytecode))


def _run(command: tuple[str, ...], workspace: Path) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        command,
        cwd=workspace,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def _diagnostic_section(stage: str, exit_code: int, stderr: bytes) -> str:
    detail = stderr.decode("utf-8", errors="replace").strip()
    suffix = f"\n{detail}" if detail else ""
    return f"[{stage}]\nexit={exit_code}{suffix}\n"


def _runtime_diagnostic_section(stage: str, result: RuntimeResult) -> str:
    lines = [f"[{stage}]", f"exit={result.exit_code}"]
    if result.normalized_result is not None:
        lines.append(f"result={result.normalized_result}")
    if result.stderr:
        lines.append(result.stderr)
    return "\n".join(lines) + "\n"


def _output_metrics(output: str) -> tuple[int, int, int]:
    nonblank = [line for line in output.splitlines() if line.strip()]
    local_count = sum(line.lstrip().startswith("local ") for line in nonblank)
    goto_count = sum(line.lstrip().startswith("goto ") for line in nonblank)
    return len(nonblank), local_count, goto_count


COLUMN_BUDGET = 120

_PLACEHOLDER_LOCAL = re.compile(r"^[vp]\d+$")
_SLOT_ASSIGNMENT = re.compile(
    r"^[A-Za-z_][A-Za-z0-9_]*\[(?:\d+|\"[^\"]*\")\]\s*="
)


def readability_metrics(output: str) -> ReadabilityMetrics:
    """Count the output properties this work is trying to move.

    These are diagnostics, not gates. A run is never failed for them; they
    exist so "more readable" can be answered with a number.
    """
    blank_lines = 0
    placeholder_locals = 0
    slot_assignments = 0
    long_lines = 0

    for line in output.splitlines():
        if not line.strip():
            blank_lines += 1
            continue
        if len(line) > COLUMN_BUDGET:
            long_lines += 1
        stripped = line.strip()
        if stripped.startswith("local "):
            declared = stripped.removeprefix("local ").split("=", maxsplit=1)[0]
            placeholder_locals += sum(
                bool(_PLACEHOLDER_LOCAL.fullmatch(name.strip()))
                for name in declared.split(",")
            )
        if _SLOT_ASSIGNMENT.match(stripped):
            slot_assignments += 1

    return ReadabilityMetrics(
        blank_lines=blank_lines,
        generated_placeholder_locals=placeholder_locals,
        slot_assignments=slot_assignments,
        long_lines=long_lines,
    )


def _is_identifier(value: str) -> bool:
    return value.isidentifier()


def count_trivial_aliases(source: str) -> int:
    count = 0
    for line in source.splitlines():
        stripped = line.strip()
        if not stripped.startswith("local "):
            continue
        assignment = stripped.removeprefix("local ").split(" = ", maxsplit=1)
        if len(assignment) == 2 and all(_is_identifier(part) for part in assignment):
            count += 1
    return count


def run_corpus(
    workspace: Path,
    output_root: Path,
    profiles: tuple[CompileProfile, ...],
    case_filter: str | None = None,
    compiler: Path | None = None,
    decompiler: Path | None = None,
    semantic: bool = False,
    runtime: Path | None = None,
) -> RunResult:
    workspace = workspace.resolve()
    output_root = output_root.resolve()
    compiler = (compiler or workspace / ".tools/luau-windows/luau-compile.exe").resolve()
    decompiler = (decompiler or workspace / "target/debug/luau-lifter.exe").resolve()
    runtime = (runtime or workspace / ".tools/luau-windows/luau.exe").resolve()
    semantic_runner = (
        workspace / "tests/luau_corpus/probes/runner.luau"
    ).resolve()
    case_paths = sorted((workspace / "tests/luau_corpus/cases").glob("*.luau"))
    if case_filter:
        folded_filter = case_filter.casefold()
        case_paths = [
            path for path in case_paths if folded_filter in path.stem.casefold()
        ]

    output_root.mkdir(parents=True, exist_ok=True)
    results: list[CaseResult] = []

    for profile in profiles:
        profile_root = output_root / profile.name
        profile_root.mkdir(parents=True, exist_ok=True)

        for source in case_paths:
            bytecode_path = profile_root / f"{source.stem}.bc"
            output_path = profile_root / f"{source.stem}.luau"
            diagnostic_path = profile_root / f"{source.stem}.log"
            diagnostic_parts: list[str] = []

            try:
                compiled = _run(
                    compiler_command(compiler, source, profile, "binary"),
                    workspace,
                )
                compile_exit = compiled.returncode
                diagnostic_parts.append(
                    _diagnostic_section("compile", compile_exit, compiled.stderr)
                )
            except OSError as error:
                compile_exit = -1
                compiled = None
                diagnostic_parts.append(
                    _diagnostic_section(
                        "compile",
                        compile_exit,
                        str(error).encode("utf-8", errors="replace"),
                    )
                )

            decompile_exit: int | None = None
            recompile_exit: int | None = None
            output_text = ""
            saved_bytecode: Path | None = None
            saved_output: Path | None = None
            bytecode_version: int | None = None
            source_runtime = None
            generated_runtime = None
            semantic_match: bool | None = None

            if compiled is not None and compile_exit == 0:
                if compiled.stdout:
                    bytecode_version = compiled.stdout[0]
                bytecode_path.write_bytes(compiled.stdout)
                saved_bytecode = bytecode_path
                try:
                    decompiled = _run(
                        decompiler_command(decompiler, bytecode_path),
                        workspace,
                    )
                    decompile_exit = decompiled.returncode
                    diagnostic_parts.append(
                        _diagnostic_section(
                            "decompile", decompile_exit, decompiled.stderr
                        )
                    )
                except OSError as error:
                    decompiled = None
                    decompile_exit = -1
                    diagnostic_parts.append(
                        _diagnostic_section(
                            "decompile",
                            decompile_exit,
                            str(error).encode("utf-8", errors="replace"),
                        )
                    )

                if decompiled is not None and decompile_exit == 0:
                    output_text = decompiled.stdout.decode(
                        "utf-8", errors="replace"
                    )
                    output_path.write_text(output_text, encoding="utf-8")
                    saved_output = output_path
                    try:
                        recompiled = _run(
                            compiler_command(compiler, output_path, profile, "null"),
                            workspace,
                        )
                        recompile_exit = recompiled.returncode
                        diagnostic_parts.append(
                            _diagnostic_section(
                                "recompile", recompile_exit, recompiled.stderr
                            )
                        )
                    except OSError as error:
                        recompile_exit = -1
                        diagnostic_parts.append(
                            _diagnostic_section(
                                "recompile",
                                recompile_exit,
                                str(error).encode("utf-8", errors="replace"),
                            )
                        )

            probe = TRUSTED_SEMANTIC_PROBES.get(source.stem)
            if (
                semantic
                and probe is not None
                and compile_exit == 0
                and decompile_exit == 0
                and recompile_exit == 0
                and saved_output is not None
            ):
                probe_path = (workspace / probe.probe_path).resolve()
                source_runtime = run_semantic_probe(
                    runtime,
                    semantic_runner,
                    source.resolve(),
                    probe_path,
                    workspace,
                )
                generated_runtime = run_semantic_probe(
                    runtime,
                    semantic_runner,
                    saved_output.resolve(),
                    probe_path,
                    workspace,
                )
                semantic_match = (
                    source_runtime.exit_code == 0
                    and generated_runtime.exit_code == 0
                    and source_runtime.normalized_result is not None
                    and source_runtime.normalized_result
                    == generated_runtime.normalized_result
                )
                diagnostic_parts.append(
                    _runtime_diagnostic_section(
                        "source-runtime",
                        source_runtime,
                    )
                )
                diagnostic_parts.append(
                    _runtime_diagnostic_section(
                        "generated-runtime",
                        generated_runtime,
                    )
                )

            diagnostic_path.write_text(
                "\n".join(diagnostic_parts),
                encoding="utf-8",
            )
            statements, locals_count, gotos = _output_metrics(output_text)
            aliases = count_trivial_aliases(output_text)
            readability = readability_metrics(output_text)
            results.append(
                CaseResult(
                    case_name=source.stem,
                    profile=profile.name,
                    compile_exit=compile_exit,
                    decompile_exit=decompile_exit,
                    recompile_exit=recompile_exit,
                    bytecode_path=saved_bytecode,
                    output_path=saved_output,
                    diagnostic_path=diagnostic_path,
                    generated_statements=statements,
                    generated_locals=locals_count,
                    generated_aliases=aliases,
                    generated_gotos=gotos,
                    bytecode_version=bytecode_version,
                    source_runtime=source_runtime,
                    generated_runtime=generated_runtime,
                    semantic_match=semantic_match,
                    blank_lines=readability.blank_lines,
                    generated_placeholder_locals=(
                        readability.generated_placeholder_locals
                    ),
                    slot_assignments=readability.slot_assignments,
                    long_lines=readability.long_lines,
                )
            )

    return RunResult(output_root=output_root, cases=tuple(results))
