from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]
if str(WORKSPACE) not in sys.path:
    sys.path.insert(0, str(WORKSPACE))

from tools.luau_corpus import (
    COMPATIBILITY_PROFILES,
    PRIMARY_PROFILES,
    RunResult,
    SECONDARY_PROFILES,
)
from tools.luau_corpus.process import run_corpus
from tools.luau_corpus.report import write_json_summary, write_markdown_summary
from tools.luau_corpus.semantic import TRUSTED_SEMANTIC_PROBES


def _arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compile, decompile, and validate the Luau truth corpus."
    )
    parser.add_argument(
        "--profiles",
        choices=("primary", "secondary", "compatibility", "all"),
        default="primary",
    )
    parser.add_argument("--case", dest="case_filter")
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("tests/luau_corpus/results/current"),
    )
    parser.add_argument(
        "--decompiler",
        type=Path,
        default=Path("target/debug/luau-lifter.exe"),
    )
    parser.add_argument(
        "--runtime",
        type=Path,
        default=Path(".tools/luau-windows/luau.exe"),
    )
    parser.add_argument("--semantic", action="store_true")
    parser.add_argument("--no-build", action="store_true")
    return parser.parse_args()


def select_profiles(name: str) -> tuple:
    return {
        "primary": PRIMARY_PROFILES,
        "secondary": SECONDARY_PROFILES,
        "compatibility": COMPATIBILITY_PROFILES,
        "all": PRIMARY_PROFILES + SECONDARY_PROFILES + COMPATIBILITY_PROFILES,
    }[name]


def run_failed(
    result: RunResult,
    semantic: bool,
    expected_cases: frozenset[tuple[str, str]] | None = None,
    expected_semantic: frozenset[tuple[str, str]] | None = None,
) -> bool:
    if expected_cases is not None:
        actual_cases = [
            (case.case_name, case.profile)
            for case in result.cases
        ]
        if (
            not expected_cases
            or len(actual_cases) != len(expected_cases)
            or frozenset(actual_cases) != expected_cases
        ):
            return True

    if semantic and expected_semantic is not None:
        actual_semantic = frozenset(
            (case.case_name, case.profile)
            for case in result.cases
            if (
                case.source_runtime is not None
                or case.generated_runtime is not None
                or case.semantic_match is not None
            )
        )
        if actual_semantic != expected_semantic:
            return True

    static_failure = any(
        case.compile_exit != 0
        or case.decompile_exit != 0
        or case.recompile_exit != 0
        for case in result.cases
    )
    if static_failure or not semantic:
        return static_failure

    return any(
        (
            case.source_runtime is not None
            or case.generated_runtime is not None
            or case.semantic_match is not None
        )
        and case.semantic_match is not True
        for case in result.cases
    )


def main() -> int:
    args = _arguments()
    if not args.no_build:
        build = subprocess.run(
            ("cargo", "+nightly", "build", "-p", "luau-lifter"),
            cwd=WORKSPACE,
            check=False,
        )
        if build.returncode != 0:
            return build.returncode

    profiles = select_profiles(args.profiles)
    case_paths = sorted((WORKSPACE / "tests/luau_corpus/cases").glob("*.luau"))
    if args.case_filter:
        folded_filter = args.case_filter.casefold()
        case_paths = [
            path for path in case_paths if folded_filter in path.stem.casefold()
        ]
    expected_cases = frozenset(
        (source.stem, profile.name)
        for profile in profiles
        for source in case_paths
    )
    expected_semantic = frozenset(
        (source.stem, profile.name)
        for profile in profiles
        for source in case_paths
        if source.stem in TRUSTED_SEMANTIC_PROBES
    )
    output = args.output if args.output.is_absolute() else WORKSPACE / args.output
    decompiler = (
        args.decompiler
        if args.decompiler.is_absolute()
        else WORKSPACE / args.decompiler
    )
    runtime = (
        args.runtime
        if args.runtime.is_absolute()
        else WORKSPACE / args.runtime
    )
    result = run_corpus(
        workspace=WORKSPACE,
        output_root=output,
        profiles=profiles,
        case_filter=args.case_filter,
        decompiler=decompiler,
        semantic=args.semantic,
        runtime=runtime,
    )
    json_path = write_json_summary(result)
    markdown_path = write_markdown_summary(result)
    print(f"Wrote {json_path}")
    print(f"Wrote {markdown_path}")

    return (
        1
        if run_failed(
            result,
            args.semantic,
            expected_cases=expected_cases,
            expected_semantic=expected_semantic,
        )
        else 0
    )


if __name__ == "__main__":
    raise SystemExit(main())
