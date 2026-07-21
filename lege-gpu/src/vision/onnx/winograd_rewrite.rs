//! Rewrites eligible 3x3 stride-1 same-padding convs into Winograd F(2,3)
//! transform/GEMM/transform triples.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};

use crate::vision::onnx::attrs::shape_i64_to_usize;
use crate::vision::onnx::graph::TensorIr;
use crate::vision::onnx::types::{PlannedOp, PlannedOpKind};
use crate::vision::ops::winograd::weight_transform_f23;
use crate::vision::reference::Tensor;

pub(crate) fn rewrite_winograd_f23(
    ops: Vec<PlannedOp>,
    known_shapes: &mut BTreeMap<String, Vec<i64>>,
    constants: &mut HashMap<String, Tensor>,
    initializers: &mut Vec<TensorIr>,
) -> Result<Vec<PlannedOp>> {
    let mut rewritten = Vec::with_capacity(ops.len());
    let mut count = 0usize;

    for op in ops {
        if !is_eligible_conv(&op) {
            rewritten.push(op);
            continue;
        }

        let Some(weight_name) = op.inputs.get(1).cloned() else {
            rewritten.push(op);
            continue;
        };
        if !constants.contains_key(&weight_name) {
            rewritten.push(op);
            continue;
        }

        let replacement = rewrite_one(op, &weight_name, known_shapes, constants, initializers)
            .with_context(|| format!("Winograd rewrite failed for weight `{weight_name}`"))?;
        count += 1;
        rewritten.extend(replacement);
    }

    if count > 0 {
        #[cfg(feature = "debug-logging")]
        eprintln!("  winograd.rewrite: replaced {count} Conv ops with F(2,3) triples");
    }
    Ok(rewritten)
}

fn is_eligible_conv(op: &PlannedOp) -> bool {
    let PlannedOpKind::Conv2d(plan) = &op.kind else {
        return false;
    };
    plan.kernel_shape == [3, 3]
        && plan.group == 1
        && plan.pads == [1, 1, 1, 1]
        && plan.strides == [1, 1]
        && plan.dilations == [1, 1]
        && op.inputs.len() >= 2
        && op.outputs.len() == 1
        && op.input_shapes.len() >= 2
        && op.output_shapes.len() == 1
}

fn rewrite_one(
    op: PlannedOp,
    weight_name: &str,
    known_shapes: &mut BTreeMap<String, Vec<i64>>,
    constants: &mut HashMap<String, Tensor>,
    initializers: &mut Vec<TensorIr>,
) -> Result<Vec<PlannedOp>> {
    let x_shape = shape_i64_to_usize(&op.input_shapes[0])?;
    let w_shape = shape_i64_to_usize(&op.input_shapes[1])?;
    let y_shape = op.output_shapes[0].clone();
    let y_shape_u = shape_i64_to_usize(&y_shape)?;
    if x_shape.len() != 4 || w_shape.len() != 4 || y_shape_u.len() != 4 {
        bail!("Winograd rewrite expects rank-4 input, weight, and output");
    }
    if x_shape[0] != 1 || y_shape_u[0] != 1 {
        bail!("Winograd rewrite only supports batch 1");
    }
    let cin = x_shape[1];
    let h = x_shape[2];
    let w = x_shape[3];
    let cout = w_shape[0];
    if w_shape != [cout, cin, 3, 3] {
        bail!(
            "Winograd weight shape mismatch: got {:?}, expected [{cout},{cin},3,3]",
            w_shape
        );
    }
    if y_shape_u[1] != cout || y_shape_u[2] != h || y_shape_u[3] != w {
        bail!(
            "Winograd output must preserve spatial dims/channels: x={x_shape:?} w={w_shape:?} y={y_shape_u:?}"
        );
    }

    let source_w = constants
        .get(weight_name)
        .with_context(|| format!("missing conv weight constant `{weight_name}`"))?;
    let u_name = format!("{weight_name}__wino_U");
    if !constants.contains_key(&u_name) {
        let mut u = vec![0.0f32; 16 * cout * cin];
        for co in 0..cout {
            for ci in 0..cin {
                let mut g = [0.0f32; 9];
                let base = (co * cin + ci) * 9;
                g.copy_from_slice(&source_w.data[base..base + 9]);
                let ut = weight_transform_f23(&g);
                for e in 0..16 {
                    u[e * cout * cin + co * cin + ci] = ut[e];
                }
            }
        }
        constants.insert(u_name.clone(), Tensor::new(vec![16, cout, cin], u)?);
        known_shapes.insert(u_name.clone(), vec![16, cout as i64, cin as i64]);
        initializers.push(TensorIr {
            name: u_name.clone(),
            dtype: "FLOAT".to_owned(),
            shape: vec![16, cout as i64, cin as i64],
        });
    }

    let ntw = w.div_ceil(2);
    let nth = h.div_ceil(2);
    let p = ntw * nth;
    let y_name = op.outputs[0].clone();
    let v_name = format!("{y_name}__wino_V");
    let m_name = format!("{y_name}__wino_M");
    let v_shape = vec![16, cin as i64, p as i64];
    let m_shape = vec![16, cout as i64, p as i64];
    known_shapes.insert(v_name.clone(), v_shape.clone());
    known_shapes.insert(m_name.clone(), m_shape.clone());

    let bias_inputs = if op.inputs.len() >= 3 {
        vec![m_name.clone(), op.inputs[2].clone()]
    } else {
        vec![m_name.clone()]
    };
    let bias_shapes = if op.inputs.len() >= 3 {
        vec![m_shape.clone(), op.input_shapes[2].clone()]
    } else {
        vec![m_shape.clone()]
    };

    Ok(vec![
        PlannedOp {
            name: format!("{}__wino_input", op.name),
            inputs: vec![op.inputs[0].clone()],
            outputs: vec![v_name.clone()],
            input_shapes: vec![op.input_shapes[0].clone()],
            output_shapes: vec![v_shape.clone()],
            kind: PlannedOpKind::WinogradInputTransform,
        },
        PlannedOp {
            name: format!("{}__wino_gemm", op.name),
            inputs: vec![u_name, v_name],
            outputs: vec![m_name.clone()],
            input_shapes: vec![vec![16, cout as i64, cin as i64], v_shape],
            output_shapes: vec![m_shape],
            kind: PlannedOpKind::WinogradBatchedGemm,
        },
        PlannedOp {
            name: format!("{}__wino_output", op.name),
            inputs: bias_inputs,
            outputs: vec![y_name],
            input_shapes: bias_shapes,
            output_shapes: vec![y_shape],
            kind: PlannedOpKind::WinogradOutputTransform {
                use_bias: op.inputs.len() >= 3,
                h,
                w,
            },
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::onnx::{PreparedGraph, load_model};
    use crate::vision::runtime::compiled::CompiledGraph;

    #[test]
    fn rewrites_runtime_yolo_model_when_available() {
        let path = std::env::var_os("LEGE_PACKAGE_INPUT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("lege-gpu should live under the repository root")
                    .join("packaging/linux64")
            })
            .join("doclayout.onnx");
        if !path.exists() {
            eprintln!(
                "skipping runtime Winograd rewrite test: {} not found",
                path.display()
            );
            return;
        }

        let model = load_model(&path).expect("load runtime YOLO model");
        let graph = PreparedGraph::from_model(&model).expect("prepare runtime YOLO graph");
        let input_count = graph
            .planned_ops
            .iter()
            .filter(|op| matches!(op.kind, PlannedOpKind::WinogradInputTransform))
            .count();
        let gemm_count = graph
            .planned_ops
            .iter()
            .filter(|op| matches!(op.kind, PlannedOpKind::WinogradBatchedGemm))
            .count();
        let output_count = graph
            .planned_ops
            .iter()
            .filter(|op| matches!(op.kind, PlannedOpKind::WinogradOutputTransform { .. }))
            .count();

        assert_eq!(input_count, 38, "expected 38 eligible YOLO conv rewrites");
        assert_eq!(gemm_count, input_count);
        assert_eq!(output_count, input_count);

        let compiled = pollster::block_on(CompiledGraph::build(&graph))
            .expect("compile rewritten runtime YOLO graph");
        assert!(compiled.step_count() > 0);
    }
}
