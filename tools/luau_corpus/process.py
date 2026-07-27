from __future__ import annotations

import subprocess
import sys
from pathlib import Path

from .model import CaseResult, CompileProfile, RunResult


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


def _output_metrics(output: str) -> tuple[int, int, int]:
    nonblank = [line for line in output.splitlines() if line.strip()]
    local_count = sum(line.lstrip().startswith("local ") for line in nonblank)
    goto_count = sum(line.lstrip().startswith("goto ") for line in nonblank)
    return len(nonblank), local_count, goto_count


def run_corpus(
    workspace: Path,
    output_root: Path,
    profiles: tuple[CompileProfile, ...],
    case_filter: str | None = None,
    compiler: Path | None = None,
    decompiler: Path | None = None,
) -> RunResult:
    workspace = workspace.resolve()
    output_root = output_root.resolve()
    compiler = (compiler or workspace / ".tools/luau-windows/luau-compile.exe").resolve()
    decompiler = (decompiler or workspace / "target/debug/luau-lifter.exe").resolve()
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

            diagnostic_path.write_text(
                "\n".join(diagnostic_parts),
                encoding="utf-8",
            )
            statements, locals_count, gotos = _output_metrics(output_text)
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
                    generated_gotos=gotos,
                    bytecode_version=bytecode_version,
                )
            )

    return RunResult(output_root=output_root, cases=tuple(results))
