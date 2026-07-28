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
        "bytecode_version": case.bytecode_version,
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
        "| profile | case | version | compile | decompile | recompile | statements | locals | aliases | gotos |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
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
            f"{case.generated_statements} | {case.generated_locals} | "
            f"{case.generated_aliases} | "
            f"{case.generated_gotos} |"
        )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path
