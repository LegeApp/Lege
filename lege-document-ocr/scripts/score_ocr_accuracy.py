#!/usr/bin/env python3
"""
Score OCR output against a reference transcript: character and word error rate.

Compares one or more candidate transcripts to a reference, after normalizing
away the differences nobody is measuring — Unicode form, case, punctuation and
whitespace runs. What survives is the recognition quality.

CER and WER are Levenshtein distance over characters and over whitespace-split
words, as a percentage of the reference length. Lower is better; a candidate
that drops text scores badly on both, and one that invents text scores badly on
WER first.

Usage:
    python score_ocr_accuracy.py \
        --reference benchmarks/recipe-2026-08-20/reference.txt \
        --candidate v5=benchmarks/recipe-2026-08-20/ppocrv5-wgpu.txt \
        --candidate v6=benchmarks/recipe-2026-08-20/ppocrv6-trt.txt

A path may also be a directory, in which case the first `*.txt` found beneath it
is used — which is how the `lege-ocr batch` CLI lays out its per-document output
directories, so a run can be scored without digging for the file:

    python score_ocr_accuracy.py --reference out/reference --candidate v6=out/v6

`benchmarks/` holds the transcripts behind recorded accuracy claims; see the
README there before changing them.
"""

import argparse
import pathlib
import re
import sys
import unicodedata


def read_transcript(path: pathlib.Path) -> str:
    """Read a transcript file, or the first `*.txt` under a directory."""
    if path.is_dir():
        found = sorted(path.rglob("*.txt"))
        if not found:
            raise SystemExit(f"no *.txt found under {path}")
        path = found[0]
    return path.read_text(encoding="utf-8")


def normalize(text: str) -> str:
    """Fold away everything the score is not meant to measure.

    NFKC so composed and decomposed accents compare equal, lowercase so casing
    errors do not dominate, punctuation to spaces because OCR punctuation is
    noisy and separately interesting, and whitespace runs collapsed because line
    and column breaks are a layout question, not a recognition one.
    """
    text = unicodedata.normalize("NFKC", text).lower()
    text = re.sub(r"[^\w\s]", " ", text)
    return re.sub(r"\s+", " ", text).strip()


def levenshtein(left: list | str, right: list | str) -> int:
    """Edit distance, two rows at a time rather than the full matrix."""
    previous = list(range(len(right) + 1))
    for index, left_value in enumerate(left, 1):
        current = [index]
        for right_index, right_value in enumerate(right, 1):
            current.append(
                min(
                    previous[right_index] + 1,
                    current[right_index - 1] + 1,
                    previous[right_index - 1] + (left_value != right_value),
                )
            )
        previous = current
    return previous[-1]


def parse_candidate(value: str) -> tuple[str, pathlib.Path]:
    name, separator, path = value.partition("=")
    if not separator:
        raise argparse.ArgumentTypeError(f"expected NAME=PATH, got {value!r}")
    return name, pathlib.Path(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--reference", type=pathlib.Path, required=True, help="ground-truth transcript"
    )
    parser.add_argument(
        "--candidate",
        type=parse_candidate,
        action="append",
        required=True,
        metavar="NAME=PATH",
        help="a transcript to score (repeatable)",
    )
    args = parser.parse_args()

    reference = normalize(read_transcript(args.reference))
    if not reference:
        raise SystemExit(f"reference {args.reference} is empty after normalization")
    reference_words = reference.split()
    print(f"reference: {len(reference)} characters, {len(reference_words)} words")

    for name, path in args.candidate:
        candidate = normalize(read_transcript(path))
        words = candidate.split()
        cer = 100 * levenshtein(candidate, reference) / len(reference)
        wer = 100 * levenshtein(words, reference_words) / len(reference_words)
        print(
            f"{name}: {len(candidate)} characters, {len(words)} words, "
            f"CER {cer:.2f}%, WER {wer:.2f}%"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
