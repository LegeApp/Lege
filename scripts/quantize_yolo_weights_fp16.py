#!/usr/bin/env python3
"""
Weight-only FP16 cast for lege-vision ONNX models.

Unlike a full `onnxconverter_common.convert_float_to_float16` pass (which rewrites
the whole graph to fp16 and inserts runtime Cast nodes), this script ONLY casts
the float *initializers* (weights/biases/constants) to FLOAT16. The graph
structure, op set, and input/output tensor types are left completely unchanged.

Why: lege-vision is a custom wgpu ONNX bridge that lowers everything to f32
kernels. It rejects the residual `Cast` nodes that the full conversion inserts on
the `images` input path. But it up-converts FLOAT16 *initializers* to f32 at load
time (see `onnx/attrs.rs::tensor_const`), so a weight-only fp16 model:

  * loads in lege-vision unchanged (no kernel / feature work),
  * halves the on-disk model size (weights are ~99% of the bytes),
  * runs the identical f32 compute path (only effect: weights rounded to fp16).

NOTE: the output is intentionally NOT ONNX-Runtime loadable. A Conv with an fp16
weight and an f32 activation is a type mismatch ORT rejects. That's expected —
lege-vision is the only consumer. Validate by running the model through the
`lege` CLI / GUI and checking detections, not against an ORT oracle.

Usage:
    python quantize_yolo_weights_fp16.py yolo-layout.onnx
    python quantize_yolo_weights_fp16.py yolo-layout.onnx -o yolo-layout.fp16.onnx
    python quantize_yolo_weights_fp16.py model.onnx --min-elements 64
"""

import argparse
import os
import sys
from pathlib import Path

try:
    import numpy as np
    import onnx
    from onnx import numpy_helper, TensorProto
except ImportError as e:
    print(f"ERROR: missing package: {e}")
    print("  pip install onnx numpy")
    sys.exit(1)

# ONNX dtype enum
FLOAT = TensorProto.FLOAT      # 1
FLOAT16 = TensorProto.FLOAT16  # 10


def cast_initializers_to_fp16(model: onnx.ModelProto, min_elements: int) -> tuple[int, int]:
    """Cast every FLOAT initializer with >= min_elements to FLOAT16, in place.

    Returns (cast_count, skipped_count).
    """
    cast_count = 0
    skipped = 0
    new_inits = []
    for init in model.graph.initializer:
        if init.data_type != FLOAT:
            new_inits.append(init)
            continue
        arr = numpy_helper.to_array(init)
        if arr.size < min_elements:
            new_inits.append(init)
            skipped += 1
            continue
        # f32 -> f16 (round to nearest). Overflow saturates to +/-inf, which is
        # vanishingly unlikely for trained weights but kept as-is if it occurs.
        arr16 = arr.astype(np.float16)
        new = numpy_helper.from_array(arr16, name=init.name)
        new_inits.append(new)
        cast_count += 1

    del model.graph.initializer[:]
    model.graph.initializer.extend(new_inits)
    return cast_count, skipped


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("model", help="input .onnx (a prepared lege-vision model)")
    ap.add_argument("-o", "--output", help="output path (default: <stem>.fp16.onnx)")
    ap.add_argument(
        "--min-elements",
        type=int,
        default=1,
        help="only cast initializers with at least this many elements "
        "(default 1 = cast all). Raise to keep tiny scalar constants in f32.",
    )
    args = ap.parse_args()

    if not os.path.exists(args.model):
        print(f"ERROR: not found: {args.model}")
        return 1

    out = args.output or str(Path(args.model).with_suffix("").as_posix() + ".fp16.onnx")

    orig_size = os.path.getsize(args.model)
    print(f"Loading: {args.model}  ({orig_size / 1024**2:.1f} MB)")
    model = onnx.load(args.model)  # loads external data if present

    n_float = sum(1 for t in model.graph.initializer if t.data_type == FLOAT)
    cast_count, skipped = cast_initializers_to_fp16(model, args.min_elements)

    onnx.save(model, out)
    new_size = os.path.getsize(out)

    print(f"  float initializers: {n_float}  ->  cast {cast_count}, skipped {skipped} (below min-elements)")
    print(f"  {orig_size / 1024**2:.1f} MB  ->  {new_size / 1024**2:.1f} MB  "
          f"({(orig_size - new_size) / orig_size * 100:.1f}% smaller)")
    print(f"  wrote: {out}")
    print("\nReminder: this model is lege-vision-only (not ORT-loadable). "
          "Validate via the lege CLI/GUI.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
