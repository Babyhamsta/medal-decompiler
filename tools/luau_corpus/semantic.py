from __future__ import annotations

from dataclasses import dataclass
import os
from pathlib import Path
import subprocess
import sys
from types import MappingProxyType
from typing import Mapping

from .model import RuntimeResult


RESULT_PREFIX = "SEMANTIC_RESULT "
SEMANTIC_TIMEOUT_SECONDS = 10.0


@dataclass(frozen=True)
class SemanticProbe:
    case_name: str
    probe_path: Path


_PROBE_NAMES = (
    "04_calls_multireturn",
    "05_varargs",
    "13_repeat_until",
    "15_generic_for",
    "20_pcall_style_flow",
    "21_state_machine",
)

TRUSTED_SEMANTIC_PROBES: Mapping[str, SemanticProbe] = MappingProxyType(
    {
        name: SemanticProbe(
            case_name=name,
            probe_path=Path(f"tests/luau_corpus/probes/{name}.luau"),
        )
        for name in _PROBE_NAMES
    }
)


def _module_path(value: str) -> str:
    normalized = value.replace("\\", "/")
    if normalized.endswith(".luau"):
        normalized = normalized.removesuffix(".luau")
    if not normalized.startswith(("./", "../")):
        raise ValueError(f"module path must be relative: {value}")
    return normalized


def runtime_command(
    runtime: Path,
    runner: Path,
    subject_module: str,
    probe_module: str,
) -> tuple[str, ...]:
    return (
        *(
            (sys.executable, runtime.as_posix())
            if runtime.suffix.casefold() == ".py"
            else (runtime.as_posix(),)
        ),
        runner.as_posix(),
        "-a",
        _module_path(subject_module),
        _module_path(probe_module),
    )


def relative_module_path(runner: Path, module: Path) -> str:
    relative = os.path.relpath(module, runner.parent).replace("\\", "/")
    if not relative.startswith(("./", "../")):
        relative = f"./{relative}"
    return _module_path(relative)


def run_semantic_probe(
    runtime: Path,
    runner: Path,
    subject: Path,
    probe: Path,
    workspace: Path,
    timeout_seconds: float = SEMANTIC_TIMEOUT_SECONDS,
) -> RuntimeResult:
    command = runtime_command(
        runtime,
        runner,
        relative_module_path(runner, subject),
        relative_module_path(runner, probe),
    )

    try:
        completed = subprocess.run(
            command,
            cwd=workspace,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired:
        return RuntimeResult(
            exit_code=-1,
            normalized_result=None,
            stderr=f"semantic runtime timed out after {timeout_seconds:g} seconds",
        )
    except OSError as error:
        return RuntimeResult(
            exit_code=-1,
            normalized_result=None,
            stderr=str(error),
        )

    stdout = completed.stdout.decode("utf-8", errors="replace")
    result_lines = [
        line.removeprefix(RESULT_PREFIX)
        for line in stdout.splitlines()
        if line.startswith(RESULT_PREFIX)
    ]

    return RuntimeResult(
        exit_code=completed.returncode,
        normalized_result=result_lines[0] if len(result_lines) == 1 else None,
        stderr=completed.stderr.decode("utf-8", errors="replace").strip(),
    )
