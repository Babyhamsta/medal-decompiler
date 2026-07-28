from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType
from typing import Mapping


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
        runtime.as_posix(),
        runner.as_posix(),
        "-a",
        _module_path(subject_module),
        _module_path(probe_module),
    )
