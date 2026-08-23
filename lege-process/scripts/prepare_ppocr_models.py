#!/usr/bin/env python3
"""
Prepare stock PaddleOCR det/rec ONNX exports for the lege-vision wgpu runtime.

The models published by PaddleOCR do not load in lege-vision as-is. This script
performs the four fixes they need, and exists mainly so the embedded assets in
`lege-ocr/assets/` are reproducible: the PP-OCRv5 assets were prepared by hand
and the recipe was lost, which is why moving to v6 needed this rediscovered.

What it does, in order:

  1. ONNX Runtime "basic" graph optimization. Folds BatchNormalization into the
     preceding Conv (the `ConvBnFusion_*` initializer names in the v5 assets are
     this pass's fingerprint) and constant-folds shape plumbing. Basic level is
     deliberate — "extended" emits `com.microsoft` fused ops that lege-vision
     rejects.

  2. ConvTranspose -> Conv 1x1 + DepthToSpace. lege-vision has no ConvTranspose
     kernel. When kernel == stride and padding is zero (the sub-pixel case PP-OCR
     detection heads use) the two forms are exactly equivalent, not an
     approximation: each input pixel writes one disjoint k*k output block, which
     is precisely what DepthToSpace does with the channels a 1x1 conv produces.

  3. Any BatchNormalization the fusion could not absorb (PP-OCRv6 recognition has
     some behind a Squeeze) becomes the per-channel affine it already is at
     inference time: Mul by a scale constant, then Add of a shift constant.

  4. Renames the graph input to the name lege-vision matches models by
     (`lege-gpu/src/vision/onnx/load.rs`: `pp_det_image` / `pp_rec_image`). Stock
     Paddle exports call it `x`, which collides with the paddle-rotate target's
     fixed 224x224 shape and fails graph construction with a confusing error.

Weight-only fp16 is a separate, optional step — run `quantize_yolo_weights_fp16.py`
on the output to halve the on-disk size, as the v5 assets were.

Usage:
    python prepare_ppocr_models.py --det path/to/det.onnx --rec path/to/rec.onnx \
        --out-dir lege-ocr/assets

Validate the result with the opt-in probe, which needs a real GPU:
    LEGE_OCR_PROBE_MODELS=$PWD/lege-ocr/assets \
    LEGE_OCR_PROBE_PAGE=$PWD/lege-process/page_0002-original.png \
      cargo test -p lege-ocr --features paddle-ocr --test model_generation_probe -- --nocapture
"""

import argparse
import shutil
import sys
import tempfile
from pathlib import Path

try:
    import numpy as np
    import onnx
    import onnx.shape_inference
    from onnx import helper, numpy_helper
except ImportError as e:
    print(f"ERROR: missing package: {e}")
    print("  pip install onnx numpy onnxruntime")
    sys.exit(1)


def optimize_with_onnxruntime(model_path: Path) -> onnx.ModelProto:
    """Run ORT's basic graph optimizations and return the rewritten model."""
    import onnxruntime as ort

    with tempfile.TemporaryDirectory() as scratch:
        optimized = Path(scratch) / "optimized.onnx"
        options = ort.SessionOptions()
        options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_BASIC
        options.optimized_model_filepath = str(optimized)
        # Constructing the session is what performs the optimization; the session
        # itself is discarded.
        ort.InferenceSession(str(model_path), options, providers=["CPUExecutionProvider"])
        return onnx.load(optimized)


def rewrite_conv_transpose(graph: onnx.GraphProto) -> int:
    """Replace sub-pixel ConvTranspose nodes with Conv 1x1 + DepthToSpace.

    Only the kernel == stride, zero-padding, group == 1 case is handled; anything
    else is left alone and will be reported as unsupported downstream rather than
    silently rewritten into something that computes a different function.
    """
    initializers = {init.name: init for init in graph.initializer}
    rewritten = 0

    for index, node in reversed(list(enumerate(graph.node))):
        if node.op_type != "ConvTranspose":
            continue

        attrs = {attr.name: attr for attr in node.attribute}
        kernel = list(attrs["kernel_shape"].ints) if "kernel_shape" in attrs else None
        strides = list(attrs["strides"].ints) if "strides" in attrs else None
        pads = list(attrs["pads"].ints) if "pads" in attrs else [0, 0, 0, 0]
        dilations = list(attrs["dilations"].ints) if "dilations" in attrs else [1, 1]
        group = attrs["group"].i if "group" in attrs else 1

        if (
            kernel is None
            or strides is None
            or kernel != strides
            or kernel[0] != kernel[1]
            or any(pad != 0 for pad in pads)
            or any(dilation != 1 for dilation in dilations)
            or group != 1
        ):
            print(
                f"  ! {node.name}: kernel={kernel} strides={strides} pads={pads} "
                f"group={group} is not the sub-pixel case; left as ConvTranspose"
            )
            continue

        block = kernel[0]
        weight = numpy_helper.to_array(initializers[node.input[1]])
        # ONNX ConvTranspose weight layout is [Cin, Cout, kH, kW].
        channels_in, channels_out = weight.shape[0], weight.shape[1]

        # Channel (i * block + j) * Cout + co of the 1x1 conv becomes output pixel
        # (i, j) of channel co after DepthToSpace in DCR mode.
        conv_weight = np.transpose(weight, (2, 3, 1, 0))  # [kH, kW, Cout, Cin]
        conv_weight = conv_weight.reshape(block * block * channels_out, channels_in, 1, 1)
        conv_weight_name = f"{node.name}_subpixel_W"
        graph.initializer.append(
            numpy_helper.from_array(conv_weight.astype(np.float32), conv_weight_name)
        )
        conv_inputs = [node.input[0], conv_weight_name]

        # A bias applies per output channel, so it repeats across the block.
        if len(node.input) > 2 and node.input[2]:
            bias = numpy_helper.to_array(initializers[node.input[2]])
            bias = np.tile(bias, block * block)
            bias_name = f"{node.name}_subpixel_B"
            graph.initializer.append(
                numpy_helper.from_array(bias.astype(np.float32), bias_name)
            )
            conv_inputs.append(bias_name)

        shuffled = f"{node.name}_subpixel"
        conv = helper.make_node(
            "Conv",
            inputs=conv_inputs,
            outputs=[shuffled],
            name=f"{node.name}_subpixel_conv",
            kernel_shape=[1, 1],
            strides=[1, 1],
            pads=[0, 0, 0, 0],
            dilations=[1, 1],
            group=1,
        )
        depth_to_space = helper.make_node(
            "DepthToSpace",
            inputs=[shuffled],
            outputs=[node.output[0]],
            name=f"{node.name}_subpixel_shuffle",
            blocksize=block,
            mode="DCR",
        )

        del graph.node[index]
        graph.node.insert(index, depth_to_space)
        graph.node.insert(index, conv)
        rewritten += 1
        print(
            f"  + {node.name}: ConvTranspose {block}x{block}/s{block} "
            f"[{channels_in}->{channels_out}] -> Conv1x1 + DepthToSpace"
        )

    return rewritten


def rewrite_batch_norm(graph: onnx.GraphProto) -> int:
    """Replace leftover BatchNormalization with the Mul + Add it reduces to.

    At inference a BN is `x * gamma/sqrt(var + eps) + (beta - mean*gamma/sqrt(var + eps))`
    per channel. The constants are shaped to broadcast over the channel axis of a
    rank-4 NCHW tensor, or a rank-3 tensor once a spatial axis has been squeezed.
    """
    initializers = {init.name: init for init in graph.initializer}
    value_ranks = {}
    for value in list(graph.value_info) + list(graph.output) + list(graph.input):
        if value.type.tensor_type.HasField("shape"):
            value_ranks[value.name] = len(value.type.tensor_type.shape.dim)
    rewritten = 0

    for index, node in reversed(list(enumerate(graph.node))):
        if node.op_type != "BatchNormalization":
            continue
        if any(name not in initializers for name in node.input[1:5]):
            print(f"  ! {node.name}: non-constant BN parameters; left as BatchNormalization")
            continue

        attrs = {attr.name: attr for attr in node.attribute}
        epsilon = attrs["epsilon"].f if "epsilon" in attrs else 1e-5
        gamma, beta, mean, variance = (
            numpy_helper.to_array(initializers[name]) for name in node.input[1:5]
        )
        scale = gamma / np.sqrt(variance + epsilon)
        shift = beta - mean * scale

        # Channel is axis 1; give the constants trailing unit axes so they
        # broadcast over whatever spatial axes remain.
        rank = value_ranks.get(node.input[0], 4)
        broadcast_shape = (1, scale.shape[0]) + (1,) * max(rank - 2, 0)
        scale_name, shift_name = f"{node.name}_scale", f"{node.name}_shift"
        graph.initializer.append(
            numpy_helper.from_array(
                scale.astype(np.float32).reshape(broadcast_shape), scale_name
            )
        )
        graph.initializer.append(
            numpy_helper.from_array(
                shift.astype(np.float32).reshape(broadcast_shape), shift_name
            )
        )

        scaled = f"{node.name}_scaled"
        multiply = helper.make_node(
            "Mul", [node.input[0], scale_name], [scaled], name=f"{node.name}_mul"
        )
        add = helper.make_node(
            "Add", [scaled, shift_name], [node.output[0]], name=f"{node.name}_add"
        )

        del graph.node[index]
        graph.node.insert(index, add)
        graph.node.insert(index, multiply)
        rewritten += 1
        print(f"  + {node.name}: BatchNormalization -> Mul + Add (rank {rank})")

    return rewritten


def rewrite_spatial_reduce_mean(model: onnx.ModelProto) -> int:
    """Replace whole-spatial ReduceMean on NCHW tensors with GlobalAveragePool.

    PP-OCRv6's squeeze-and-excite blocks emit `ReduceMean(axes=[2,3], keepdims=1)`,
    which is the definition of GlobalAveragePool. lege-vision reduces one axis at
    a time but has a dedicated global-pool kernel, so this is both a compatibility
    fix and the faster lowering.
    """
    # Ranks come from shape inference; dynamic dimensions still carry a rank.
    inferred = onnx.shape_inference.infer_shapes(model)
    ranks = {
        value.name: len(value.type.tensor_type.shape.dim)
        for value in list(inferred.graph.value_info)
        + list(inferred.graph.input)
        + list(inferred.graph.output)
        if value.type.tensor_type.HasField("shape")
    }
    graph = model.graph
    rewritten = 0

    for index, node in reversed(list(enumerate(graph.node))):
        if node.op_type != "ReduceMean":
            continue
        attrs = {attr.name: attr for attr in node.attribute}
        # Opset 18 moved `axes` to an input; PP-OCR exports predate that.
        if "axes" not in attrs:
            continue
        axes = sorted(list(attrs["axes"].ints))
        keepdims = attrs["keepdims"].i if "keepdims" in attrs else 1
        rank = ranks.get(node.input[0])
        if rank != 4 or keepdims != 1 or axes not in ([2, 3], [-2, -1]):
            continue

        pooled = helper.make_node(
            "GlobalAveragePool",
            inputs=[node.input[0]],
            outputs=[node.output[0]],
            name=f"{node.name}_global_pool",
        )
        del graph.node[index]
        graph.node.insert(index, pooled)
        rewritten += 1
        print(f"  + {node.name}: ReduceMean(axes={axes}) -> GlobalAveragePool")

    return rewritten


def rename_graph_input(graph: onnx.GraphProto, new_name: str) -> str:
    """Rename the single graph input, and every reference to it, in place."""
    if len(graph.input) != 1:
        raise SystemExit(f"expected exactly one graph input, found {len(graph.input)}")
    old_name = graph.input[0].name
    if old_name == new_name:
        return old_name
    graph.input[0].name = new_name
    for node in graph.node:
        for index, name in enumerate(node.input):
            if name == old_name:
                node.input[index] = new_name
    for value in graph.value_info:
        if value.name == old_name:
            value.name = new_name
    return old_name


def prepare(source: Path, destination: Path, input_name: str) -> None:
    print(f"{source}  ->  {destination}")
    model = optimize_with_onnxruntime(source)
    graph = model.graph

    rewrite_conv_transpose(graph)
    rewrite_batch_norm(graph)
    # Those rewrites invalidate the cached shapes of the tensors they touch, and
    # the ReduceMean pass below re-infers from scratch.
    del graph.value_info[:]

    rewrite_spatial_reduce_mean(model)
    old_name = rename_graph_input(graph, input_name)
    print(f"  + graph input {old_name!r} -> {input_name!r}")
    del graph.value_info[:]

    onnx.checker.check_model(model)
    destination.parent.mkdir(parents=True, exist_ok=True)
    onnx.save(model, destination)
    size_mb = destination.stat().st_size / 1e6
    print(f"  = {len(graph.node)} nodes, {size_mb:.2f} MB\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--det", type=Path, required=True, help="stock detection ONNX")
    parser.add_argument("--rec", type=Path, required=True, help="stock recognition ONNX")
    parser.add_argument("--dict", type=Path, help="character dictionary to copy alongside")
    parser.add_argument("--out-dir", type=Path, required=True, help="output directory")
    args = parser.parse_args()

    prepare(args.det, args.out_dir / "ppocr-det.onnx", "pp_det_image")
    prepare(args.rec, args.out_dir / "ppocr-rec.onnx", "pp_rec_image")
    if args.dict:
        shutil.copy(args.dict, args.out_dir / "ppocr-dict.txt")
        lines = args.dict.read_text(encoding="utf-8").count("\n")
        print(f"{args.dict} -> {args.out_dir / 'ppocr-dict.txt'} ({lines} glyphs)")


if __name__ == "__main__":
    main()
