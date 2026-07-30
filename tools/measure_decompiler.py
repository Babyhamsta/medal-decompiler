"""Wall clock, peak heap, and per-phase cost for one decompiler fixture.

The release binary supplies wall clock. A second binary built with the
`profiling` feature supplies the phase table and peak live heap; that build
carries accounting overhead, so its total is not comparable to the release
timing and is not reported as such.

`--profiling-binary` has no default. A bare invocation therefore times only
the release binary and skips the phase table, rather than risking a
profiling-instrumented binary being timed and reported as clean wall clock.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]

_REQUIRED_REPORT_KEYS = ("phases", "peak_live_mb")


class NoPhaseReportError(ValueError):
    """stderr contains no profiling report JSON object at all."""


class MalformedPhaseReportError(ValueError):
    """stderr contains what looks like a profiling report, but it is not
    valid, well-formed JSON matching the expected shape."""


def parse_phase_report(stderr: str) -> dict:
    """Extract the profiling report JSON object from decompiler stderr.

    The binary writes the report to stderr and, on failure, appends
    `decompiler error: {...}` after it. A naive "first `{` to last `}`"
    scan can be fooled by braces in that trailing message (or in any other
    surrounding log noise), silently slicing out the wrong text. Instead,
    every `{` in the stream is tried in turn as a possible JSON object
    start via `json.JSONDecoder.raw_decode`, which parses exactly one
    object and ignores whatever follows it. The first candidate that both
    parses and has the expected top-level keys is the report.

    Raises `NoPhaseReportError` (a `ValueError`) when stderr contains no
    `{` at all, or none of the candidates parse. Raises
    `MalformedPhaseReportError` (also a `ValueError`, but distinguishable)
    when at least one candidate looked like the start of a JSON object but
    failed to parse -- this is the "report was attempted but is broken"
    case, which callers should treat as loud, not silent.
    """
    positions = [index for index, char in enumerate(stderr) if char == "{"]
    if not positions:
        raise NoPhaseReportError(
            "no profiling report found in decompiler stderr"
        )

    decoder = json.JSONDecoder()
    last_error: json.JSONDecodeError | None = None
    for start in positions:
        try:
            candidate, _ = decoder.raw_decode(stderr, start)
        except json.JSONDecodeError as exc:
            last_error = exc
            continue
        if isinstance(candidate, dict) and all(
            key in candidate for key in _REQUIRED_REPORT_KEYS
        ):
            return candidate

    if last_error is not None:
        raise MalformedPhaseReportError(
            f"profiling report present but failed to parse: {last_error}"
        ) from last_error
    raise NoPhaseReportError(
        "no profiling report found in decompiler stderr"
    )


def _run(binary: Path, fixture: Path) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        (str(binary), str(fixture)),
        cwd=WORKSPACE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
    )


def measure_wall_clock(binary: Path, fixture: Path, runs: int) -> list[float]:
    timings = []
    for _ in range(runs):
        started = time.perf_counter()
        completed = _run(binary, fixture)
        elapsed = time.perf_counter() - started
        if completed.returncode != 0:
            raise SystemExit(
                f"decompiler exited {completed.returncode}: "
                f"{completed.stderr.decode('utf-8', errors='replace')}"
            )
        timings.append(elapsed)
    return timings


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Measure decompiler wall clock and per-phase cost."
    )
    parser.add_argument(
        "--fixture",
        type=Path,
        default=(
            Path(os.environ["MEDAL_BIG_FIXTURE"])
            if os.environ.get("MEDAL_BIG_FIXTURE")
            else None
        ),
        help=(
            "Bytecode to measure. Defaults to $MEDAL_BIG_FIXTURE. The large "
            "capture lives outside the repository, so it is named by "
            "environment rather than committed."
        ),
    )
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument(
        "--release-binary",
        type=Path,
        default=Path("target/release/luau-lifter.exe"),
        help="Plain release build. This is what wall clock is measured from.",
    )
    parser.add_argument(
        "--profiling-binary",
        type=Path,
        default=None,
        help=(
            "Binary built with `cargo +nightly build --release "
            "-p luau-lifter --features profiling`, used only to produce "
            "the per-phase table and peak live heap. No default: if this "
            "is not supplied, the phase table is skipped rather than "
            "guessing a binary and risking a profiling build's overhead "
            "being reported as clean wall clock."
        ),
    )
    arguments = parser.parse_args()

    if arguments.fixture is None:
        raise SystemExit(
            "no fixture: pass --fixture or set MEDAL_BIG_FIXTURE"
        )
    fixture = arguments.fixture.resolve()
    if not fixture.exists():
        raise SystemExit(f"fixture not found: {fixture}")

    if (
        arguments.profiling_binary is not None
        and arguments.profiling_binary.resolve()
        == arguments.release_binary.resolve()
    ):
        raise SystemExit(
            "--profiling-binary and --release-binary resolve to the same "
            "file; timing it would include profiling accounting overhead "
            "and misreport it as clean wall clock. Build the profiling "
            "binary to a separate path and pass it explicitly."
        )

    timings = measure_wall_clock(
        arguments.release_binary, fixture, arguments.runs
    )
    print(f"# Decompiler measurement: {fixture.name}\n")
    print("| Run | Seconds |")
    print("| ---: | ---: |")
    for index, elapsed in enumerate(timings, start=1):
        print(f"| {index} | {elapsed:.2f} |")
    print(f"\nBest of {arguments.runs}: **{min(timings):.2f} s**\n")

    if arguments.profiling_binary is None:
        print(
            "No --profiling-binary supplied; phase table skipped. Build "
            "one with `cargo +nightly build --release -p luau-lifter "
            "--features profiling` and pass its path via "
            "--profiling-binary to get the per-phase table and peak live "
            "heap."
        )
        return 0

    completed = _run(arguments.profiling_binary, fixture)
    stderr_text = completed.stderr.decode("utf-8", errors="replace")
    if completed.returncode != 0:
        raise SystemExit(
            f"profiling binary exited {completed.returncode}: {stderr_text}"
        )

    try:
        report = parse_phase_report(stderr_text)
    except NoPhaseReportError:
        print(
            "No phase report in profiling binary output even though it "
            "exited 0. Confirm --profiling-binary was actually built with "
            "`--features profiling`."
        )
        return 0
    except MalformedPhaseReportError as exc:
        raise SystemExit(
            f"profiling binary produced a broken report: {exc}\n"
            f"stderr was:\n{stderr_text}"
        )

    print(f"Peak live heap: **{report['peak_live_mb']} MB**\n")
    print("| Phase | Seconds | Allocated MB |")
    print("| --- | ---: | ---: |")
    for phase in sorted(
        report["phases"], key=lambda entry: entry["seconds"], reverse=True
    ):
        print(
            f"| {phase['phase']} | {phase['seconds']:.3f} "
            f"| {phase['alloc_mb']:.1f} |"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
