#!/usr/bin/env python3
"""
Black-box smoke fuzzer for the Lege CLI.

This intentionally treats the executable as the public interface: it mutates
argv and interactive stdin, then flags panics, crashes, and hangs. Invalid input
that exits cleanly is not a failure.
"""

from __future__ import annotations

import argparse
import base64
import os
import random
import string
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path


PANIC_MARKERS = (
    "panicked at",
    "internal error:",
    "fatal runtime error",
)

WINDOWS_CRASH_CODES = {
    0xC0000005,  # access violation
    0xC0000409,  # stack buffer overrun
    0xC0000374,  # heap corruption
    0xC00000FD,  # stack overflow
}


@dataclass
class FuzzCase:
    name: str
    args: list[str]
    stdin: str
    timeout: float


@dataclass
class Failure:
    case: FuzzCase
    seed: int
    returncode: int | None
    reason: str
    stdout: str
    stderr: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Smoke-fuzz the Lege CLI executable")
    parser.add_argument(
        "--exe",
        type=Path,
        default=Path("target/debug-fast/lege.exe" if os.name == "nt" else "target/debug-fast/lege"),
        help="Path to the Lege executable to fuzz",
    )
    parser.add_argument(
        "--runtime-dir",
        type=Path,
        default=None,
        help="Runtime asset directory. Defaults to the executable directory.",
    )
    parser.add_argument("--seconds", type=float, default=60.0, help="Fuzz duration")
    parser.add_argument("--iterations", type=int, default=None, help="Maximum cases to run")
    parser.add_argument("--timeout", type=float, default=8.0, help="Per-case timeout")
    parser.add_argument("--seed", type=int, default=None, help="Deterministic RNG seed")
    parser.add_argument(
        "--sample-pdf",
        type=Path,
        default=None,
        help="Optional real PDF for tiny 1-page processing/probe cases",
    )
    parser.add_argument(
        "--keep-going",
        action="store_true",
        help="Continue after failures instead of stopping at the first one",
    )
    parser.add_argument(
        "--failure-dir",
        type=Path,
        default=Path("target/fuzz-cli-smoke"),
        help="Directory for failure repro artifacts",
    )
    return parser.parse_args()


def random_token(rng: random.Random, max_len: int = 64) -> str:
    alphabet = string.ascii_letters + string.digits + " -_,.;:'\"\\/@#$%^&*()[]{}<>|`~"
    length = rng.randint(0, max_len)
    return "".join(rng.choice(alphabet) for _ in range(length))


def weird_path(rng: random.Random) -> str:
    candidates = [
        "",
        ".",
        "..",
        "NUL",
        "CON",
        "C:\\",
        "C:\\missing\\book.pdf",
        "C:\\path with spaces\\book.pdf",
        "\\\\?\\C:\\missing\\book.pdf",
        "\\\\server\\share\\missing.pdf",
        "relative missing.pdf",
        "emoji-🙂.pdf",
        "quote\"inside.pdf",
        "semi;colon.pdf",
        "tab\tinside.pdf",
        "newline\ninside.pdf",
    ]
    if rng.random() < 0.7:
        return rng.choice(candidates)
    suffix = rng.choice([".pdf", ".zip", ".png", ".jpg", ".djvu", "", ".PDF"])
    return random_token(rng, 80) + suffix


def weird_range(rng: random.Random) -> str:
    candidates = [
        "",
        "1",
        "1-1",
        "0",
        "0-0",
        "-1",
        "1-",
        "-10",
        "10-1",
        "1,,2",
        "1---10",
        "all",
        "full",
        "*",
        "999999999-1000000000",
        " 1 - 3 ",
    ]
    return rng.choice(candidates)


def weird_format(rng: random.Random) -> str:
    candidates = [
        "",
        "1",
        "2",
        "3",
        "1hw",
        "2 o1",
        "2 o2",
        "3hw",
        "2 o999",
        "4",
        "0",
        "999",
        "1 k=nan",
        "2 --bad",
        random_token(rng, 48),
    ]
    return rng.choice(candidates)


def weird_binarization(rng: random.Random) -> str:
    candidates = [
        "",
        "1",
        "2",
        "3",
        "2 200",
        "2 thr=255",
        "2 thr=-1",
        "1 k=0.25",
        "1 k=nan",
        "1 k=inf",
        "fixed",
        "adaptive",
        random_token(rng, 48),
    ]
    return rng.choice(candidates)


def weird_target(rng: random.Random) -> str:
    candidates = [
        "",
        "0",
        "1",
        "28",
        "29",
        "1200",
        "1440x1920",
        "1920x1440",
        "1600 1200",
        "0x0",
        "-1",
        "nan",
        "999999999x999999999",
        random_token(rng, 48),
    ]
    return rng.choice(candidates)


def random_cli_args(rng: random.Random, tmpdir: Path, sample_pdf: Path | None) -> list[str]:
    option_pool: list[list[str]] = [
        ["--text-format", rng.choice(["ccitt4", "jbig2", "jpeg", "djvu", "epub", "", "bad"])],
        ["--cover-format", rng.choice(["jpeg", "jp2", "ccitt4", "jbig2", "none", "bad"])],
        ["--jbig2-mode", rng.choice(["generic", "symbol", "sym-unify", "sym_unify", "bad"])],
        ["--binarization", rng.choice(["adaptive", "fixed", "heavy", "bad", ""])],
        ["--threshold", rng.choice(["0", "1", "128", "255", "256", "-1", "nan", ""])],
        ["--sauvola-k", rng.choice(["0", "0.25", "1", "-0.1", "nan", "inf", ""])],
        ["--djvu-quality", rng.choice(["1", "50", "100", "0", "101", "-1", "nan", ""])],
        ["--output", str(tmpdir)],
        ["--language", rng.choice(["eng", "deu", "eng_best", "bad-lang", "", "🙂"])],
        [rng.choice(["--no-layout", "--ocr", "--no-ocr", "--best-ocr", "--no-cover"])],
        [rng.choice(["--invert", "--deskew", "--jpeg-compat", "--high-quality"])],
        [rng.choice(["--center-margins", "--crop-margins", "--force-crop"])],
        [rng.choice(["--image-only", "--original-images", "--fast-resize", "--probe-json"])],
        [rng.choice(["--unknown", "--", "---", "-"])],
    ]
    args: list[str] = []
    for option in rng.sample(option_pool, rng.randint(0, min(5, len(option_pool)))):
        args.extend(option)

    if sample_pdf and rng.random() < 0.35:
        args.append(str(sample_pdf))
        if rng.random() < 0.8:
            args.append("1-1")
    elif rng.random() < 0.75:
        args.append(weird_path(rng))
        if rng.random() < 0.45:
            args.append(weird_range(rng))

    return args


def make_interactive_case(rng: random.Random, index: int) -> FuzzCase:
    file_line = weird_path(rng)
    page_range = weird_range(rng)
    if page_range:
        file_line = f'"{file_line}" {page_range}'
    stdin = "\n".join(
        [
            file_line,
            weird_format(rng),
            weird_binarization(rng),
            weird_target(rng),
            "",
        ]
    )
    return FuzzCase(f"interactive-{index}", [], stdin, 8.0)


def make_cases(rng: random.Random, tmpdir: Path, sample_pdf: Path | None) -> list[FuzzCase]:
    cases = [
        FuzzCase("empty-stdin", [], "\n\n\n\n\n", 8.0),
        FuzzCase("help-ish", ["--help"], "", 4.0),
        FuzzCase("version-ish", ["--version"], "", 4.0),
        FuzzCase("bad-option-missing-value", ["--threshold"], "", 4.0),
        FuzzCase("probe-corrupt-pdf", ["--probe-json", str(tmpdir / "corrupt.pdf")], "", 5.0),
        FuzzCase("probe-tiny-png", ["--probe-json", str(tmpdir / "tiny.png")], "", 5.0),
    ]
    if sample_pdf:
        cases.extend(
            [
                FuzzCase("probe-sample-pdf", ["--probe-json", str(sample_pdf)], "", 10.0),
                FuzzCase(
                    "sample-pdf-no-layout-one-page",
                    [
                        str(sample_pdf),
                        "1-1",
                        "--output",
                        str(tmpdir),
                        "--no-layout",
                        "--text-format",
                        "ccitt4",
                    ],
                    "",
                    20.0,
                ),
            ]
        )
    return cases


def write_seed_files(tmpdir: Path) -> None:
    tmpdir.mkdir(parents=True, exist_ok=True)
    (tmpdir / "corrupt.pdf").write_bytes(b"%PDF-1.7\n1 0 obj\n<<>>\n")
    (tmpdir / "empty.pdf").write_bytes(b"")
    tiny_png = base64.b64decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAFgwJ/luzG/QAAAABJRU5ErkJggg=="
    )
    (tmpdir / "tiny.png").write_bytes(tiny_png)


def command_env(runtime_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["NO_COLOR"] = "1"
    env["RUST_BACKTRACE"] = "0"
    env["LEGE_DATA_DIR"] = str(runtime_dir)
    env["LEGE_ASSET_DIR"] = str(runtime_dir)
    pdfium_name = "pdfium.dll" if os.name == "nt" else "libpdfium.so"
    pdfium = runtime_dir / pdfium_name
    if pdfium.exists():
        env["PDFIUM_PATH"] = str(pdfium)
    if os.name == "nt":
        path_key = "PATH"
        env[path_key] = str(runtime_dir) + os.pathsep + env.get(path_key, "")
    else:
        env["LD_LIBRARY_PATH"] = str(runtime_dir) + os.pathsep + env.get("LD_LIBRARY_PATH", "")
    return env


def classify_result(case: FuzzCase, proc: subprocess.CompletedProcess[str], seed: int) -> Failure | None:
    combined = f"{proc.stdout}\n{proc.stderr}".lower()
    if proc.returncode == 101:
        return Failure(case, seed, proc.returncode, "rust panic exit code 101", proc.stdout, proc.stderr)
    if os.name == "nt" and proc.returncode in WINDOWS_CRASH_CODES:
        return Failure(case, seed, proc.returncode, f"windows crash exit code {proc.returncode}", proc.stdout, proc.stderr)
    if any(marker in combined for marker in PANIC_MARKERS):
        return Failure(case, seed, proc.returncode, "panic marker in output", proc.stdout, proc.stderr)
    return None


def save_failure(failure_dir: Path, failure: Failure) -> Path:
    failure_dir.mkdir(parents=True, exist_ok=True)
    safe_name = "".join(c if c.isalnum() or c in "-_" else "_" for c in failure.case.name)
    path = failure_dir / f"{int(time.time())}-{safe_name}.txt"
    path.write_text(
        "\n".join(
            [
                f"seed={failure.seed}",
                f"name={failure.case.name}",
                f"reason={failure.reason}",
                f"returncode={failure.returncode}",
                f"args={failure.case.args!r}",
                f"stdin={failure.case.stdin!r}",
                "",
                "--- stdout ---",
                failure.stdout[-8000:],
                "",
                "--- stderr ---",
                failure.stderr[-8000:],
            ]
        ),
        encoding="utf-8",
    )
    return path


def run_case(exe: Path, env: dict[str, str], case: FuzzCase, timeout: float, seed: int) -> Failure | None:
    try:
        proc = subprocess.run(
            [str(exe), *case.args],
            input=case.stdin,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            timeout=max(timeout, case.timeout),
            env=env,
        )
    except subprocess.TimeoutExpired as exc:
        return Failure(
            case,
            seed,
            None,
            f"timeout after {max(timeout, case.timeout):.1f}s",
            exc.stdout or "",
            exc.stderr or "",
        )
    return classify_result(case, proc, seed)


def main() -> int:
    args = parse_args()
    exe = args.exe.resolve()
    if not exe.exists():
        print(f"error: executable not found: {exe}", file=sys.stderr)
        return 2

    runtime_dir = (args.runtime_dir or exe.parent).resolve()
    sample_pdf = args.sample_pdf.resolve() if args.sample_pdf else None
    if sample_pdf and not sample_pdf.exists():
        print(f"error: sample PDF not found: {sample_pdf}", file=sys.stderr)
        return 2

    seed = args.seed if args.seed is not None else random.SystemRandom().randint(0, 2**63 - 1)
    rng = random.Random(seed)
    env = command_env(runtime_dir)

    failures: list[Failure] = []
    started = time.monotonic()
    ran = 0

    with tempfile.TemporaryDirectory(prefix="lege-cli-fuzz-") as tmp:
        tmpdir = Path(tmp)
        write_seed_files(tmpdir)
        corpus = make_cases(rng, tmpdir, sample_pdf)

        print(f"seed={seed}")
        print(f"exe={exe}")
        print(f"runtime_dir={runtime_dir}")

        while True:
            if args.iterations is not None and ran >= args.iterations:
                break
            if time.monotonic() - started >= args.seconds:
                break

            if ran < len(corpus):
                case = corpus[ran]
            elif rng.random() < 0.45:
                case = make_interactive_case(rng, ran)
            else:
                case = FuzzCase(
                    f"argv-{ran}",
                    random_cli_args(rng, tmpdir, sample_pdf),
                    "",
                    args.timeout,
                )

            failure = run_case(exe, env, case, args.timeout, seed)
            ran += 1

            if failure:
                failures.append(failure)
                repro_path = save_failure(args.failure_dir, failure)
                print(
                    f"FAIL {case.name}: {failure.reason}; repro={repro_path}",
                    file=sys.stderr,
                )
                if not args.keep_going:
                    break

    elapsed = time.monotonic() - started
    print(f"ran={ran} elapsed={elapsed:.1f}s failures={len(failures)}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
