from __future__ import annotations

import json
from pathlib import Path

from .model import CaseResult, RunResult


def _relative(path: Path | None, root: Path) -> str | None:
    if path is None:
        return None
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return path.as_posix()


def _case_payload(case: CaseResult, root: Path) -> dict[str, object]:
    return {
        "case_name": case.case_name,
        "profile": case.profile,
        "compile_exit": case.compile_exit,
        "decompile_exit": case.decompile_exit,
        "recompile_exit": case.recompile_exit,
        "bytecode_path": _relative(case.bytecode_path, root),
        "output_path": _relative(case.output_path, root),
        "diagnostic_path": _relative(case.diagnostic_path, root),
        "generated_statements": case.generated_statements,
        "generated_locals": case.generated_locals,
        "generated_aliases": case.generated_aliases,
        "generated_gotos": case.generated_gotos,
        "blank_lines": case.blank_lines,
        "generated_placeholder_locals": case.generated_placeholder_locals,
        "slot_assignments": case.slot_assignments,
        "long_lines": case.long_lines,
        "bytecode_version": case.bytecode_version,
        "source_runtime_exit": (
            case.source_runtime.exit_code
            if case.source_runtime is not None
            else None
        ),
        "source_runtime_result": (
            case.source_runtime.normalized_result
            if case.source_runtime is not None
            else None
        ),
        "generated_runtime_exit": (
            case.generated_runtime.exit_code
            if case.generated_runtime is not None
            else None
        ),
        "generated_runtime_result": (
            case.generated_runtime.normalized_result
            if case.generated_runtime is not None
            else None
        ),
        "semantic_match": case.semantic_match,
    }


def _totals(result: RunResult) -> dict[str, int]:
    return {
        "cases": len(result.cases),
        "compile_failed": sum(case.compile_exit != 0 for case in result.cases),
        "decompile_failed": sum(
            case.compile_exit == 0 and case.decompile_exit != 0
            for case in result.cases
        ),
        "recompile_failed": sum(
            case.decompile_exit == 0 and case.recompile_exit != 0
            for case in result.cases
        ),
        "semantic_checked": sum(
            case.semantic_match is not None for case in result.cases
        ),
        "semantic_mismatched": sum(
            case.semantic_match is False for case in result.cases
        ),
        "source_runtime_failed": sum(
            case.source_runtime is not None
            and (
                case.source_runtime.exit_code != 0
                or case.source_runtime.normalized_result is None
            )
            for case in result.cases
        ),
        "generated_runtime_failed": sum(
            case.generated_runtime is not None
            and (
                case.generated_runtime.exit_code != 0
                or case.generated_runtime.normalized_result is None
            )
            for case in result.cases
        ),
    }


def write_json_summary(result: RunResult) -> Path:
    path = result.output_root / "summary.json"
    payload = {
        "totals": _totals(result),
        "cases": [
            _case_payload(case, result.output_root)
            for case in sorted(
                result.cases,
                key=lambda item: (item.profile, item.case_name),
            )
        ],
    }
    path.write_text(
        json.dumps(payload, indent=2) + "\n",
        encoding="utf-8",
    )
    return path


def write_markdown_summary(result: RunResult) -> Path:
    path = result.output_root / "summary.md"
    totals = _totals(result)
    lines = [
        "# Luau Corpus Run",
        "",
        (
            f"Cases: {totals['cases']}; compile failures: "
            f"{totals['compile_failed']}; decompile failures: "
            f"{totals['decompile_failed']}; recompile failures: "
            f"{totals['recompile_failed']}."
        ),
        "",
        "| profile | case | version | compile | decompile | recompile | source run | generated run | semantic | statements | locals | aliases | gotos | blank | vN | slots | wide |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for case in sorted(
        result.cases,
        key=lambda item: (item.profile, item.case_name),
    ):
        lines.append(
            f"| {case.profile} | {case.case_name} | "
            f"{case.bytecode_version if case.bytecode_version is not None else '-'} | "
            f"{case.compile_exit} | "
            f"{case.decompile_exit if case.decompile_exit is not None else '-'} | "
            f"{case.recompile_exit if case.recompile_exit is not None else '-'} | "
            f"{case.source_runtime.exit_code if case.source_runtime is not None else '-'} | "
            f"{case.generated_runtime.exit_code if case.generated_runtime is not None else '-'} | "
            f"{'pass' if case.semantic_match is True else 'fail' if case.semantic_match is False else '-'} | "
            f"{case.generated_statements} | {case.generated_locals} | "
            f"{case.generated_aliases} | "
            f"{case.generated_gotos} | "
            f"{case.blank_lines} | {case.generated_placeholder_locals} | "
            f"{case.slot_assignments} | {case.long_lines} |"
        )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path
