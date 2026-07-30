"""Wall clock, peak heap, and per-phase cost for one decompiler fixture.

The release binary supplies wall clock. A second binary built with the
`profiling` feature supplies the phase table and peak live heap; that build
carries accounting overhead, so its total is not comparable to the release
timing and is not reported as such.
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


def parse_phase_report(stderr: str) -> dict:
    start = stderr.find("{")
    end = stderr.rfind("}")
    if start == -1 or end == -1 or end < start:
        raise ValueError("no profiling report found in decompiler stderr")
    return json.loads(stderr[start : end + 1])


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
    )
    parser.add_argument(
        "--profiling-binary",
        type=Path,
        default=Path("target/release/luau-lifter.exe"),
    )
    arguments = parser.parse_args()

    if arguments.fixture is None:
        raise SystemExit(
            "no fixture: pass --fixture or set MEDAL_BIG_FIXTURE"
        )
    fixture = arguments.fixture.resolve()
    if not fixture.exists():
        raise SystemExit(f"fixture not found: {fixture}")

    timings = measure_wall_clock(
        arguments.release_binary, fixture, arguments.runs
    )
    print(f"# Decompiler measurement: {fixture.name}\n")
    print("| Run | Seconds |")
    print("| ---: | ---: |")
    for index, elapsed in enumerate(timings, start=1):
        print(f"| {index} | {elapsed:.2f} |")
    print(f"\nBest of {arguments.runs}: **{min(timings):.2f} s**\n")

    completed = _run(arguments.profiling_binary, fixture)
    try:
        report = parse_phase_report(
            completed.stderr.decode("utf-8", errors="replace")
        )
    except ValueError:
        print(
            "No phase report. Build with "
            "`cargo +nightly build --release -p luau-lifter "
            "--features profiling` and pass --profiling-binary."
        )
        return 0

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
