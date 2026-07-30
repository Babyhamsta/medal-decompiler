from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class CompileProfile:
    name: str
    optimization: int
    debug: int
    fast_flags: tuple[str, ...] = ()


@dataclass(frozen=True)
class RuntimeResult:
    exit_code: int
    normalized_result: str | None
    stderr: str


@dataclass(frozen=True)
class ReadabilityMetrics:
    blank_lines: int
    generated_placeholder_locals: int
    slot_assignments: int
    long_lines: int


@dataclass(frozen=True)
class CaseResult:
    case_name: str
    profile: str
    compile_exit: int
    decompile_exit: int | None
    recompile_exit: int | None
    bytecode_path: Path | None
    output_path: Path | None
    diagnostic_path: Path
    generated_statements: int
    generated_locals: int
    generated_aliases: int
    generated_gotos: int
    bytecode_version: int | None
    source_runtime: RuntimeResult | None = None
    generated_runtime: RuntimeResult | None = None
    semantic_match: bool | None = None
    blank_lines: int = 0
    generated_placeholder_locals: int = 0
    slot_assignments: int = 0
    long_lines: int = 0


@dataclass(frozen=True)
class RunResult:
    output_root: Path
    cases: tuple[CaseResult, ...]


PRIMARY_PROFILES = (
    CompileProfile("O1_g1", 1, 1),
    CompileProfile("O2_g1", 2, 1),
    CompileProfile("O2_g0", 2, 0),
)

SECONDARY_PROFILES = (
    CompileProfile("O0_g1", 0, 1),
    CompileProfile("O1_g0", 1, 0),
    CompileProfile("O1_g2", 1, 2),
)

COMPATIBILITY_PROFILES = (
    CompileProfile(
        "V9",
        1,
        1,
        (
            "LuauBytecodeCostModel=false",
            "LuauEmitCallFeedback=false",
            "DebugLuauUserDefinedClasses=false",
        ),
    ),
    CompileProfile(
        "V10",
        1,
        1,
        (
            "LuauBytecodeCostModel=false",
            "LuauEmitCallFeedback=false",
            "DebugLuauUserDefinedClasses=true",
        ),
    ),
    CompileProfile(
        "V11",
        1,
        1,
        (
            "LuauBytecodeCostModel=false",
            "LuauEmitCallFeedback=true",
            "DebugLuauUserDefinedClasses=false",
        ),
    ),
    CompileProfile(
        "V12",
        1,
        1,
        ("LuauBytecodeCostModel=true",),
    ),
)
