//! Plan-time types: op kinds, conv/pool params, elementwise ops.
//! These are leaf types; nothing in this module imports other crate submodules.

#[derive(Debug, Clone)]
pub(crate) enum TensorConst {
    Int64(Vec<i64>),
    Float32(Vec<f32>),
}

#[derive(Debug)]
pub(crate) struct PlannedOp {
    pub(crate) name: String,
    pub(crate) inputs: Vec<String>,
    pub(crate) outputs: Vec<String>,
    pub(crate) input_shapes: Vec<Vec<i64>>,
    pub(crate) output_shapes: Vec<Vec<i64>>,
    pub(crate) kind: PlannedOpKind,
}

#[derive(Debug)]
pub(crate) enum PlannedOpKind {
    Conv2d(Conv2dPlan),
    Elementwise(ElementwiseKind),
    Unary(UnaryKind),
    Concat {
        axis: usize,
    },
    Split {
        axis: usize,
        sizes: Vec<i64>,
    },
    Slice {
        axes: Vec<usize>,
        starts: Vec<i64>,
        ends: Vec<i64>,
        steps: Vec<i64>,
    },
    Reshape {
        target: Vec<i64>,
    },
    Transpose {
        perm: Vec<usize>,
    },
    MaxPool2d(Pool2dPlan),
    ResizeNearest {
        scales: Vec<f32>,
    },
    ResizeLinear {
        sizes: Vec<i64>,
        align_corners: bool,
    },
    Pad {
        pads: Vec<i64>,
        mode: PadMode,
        value: f32,
    },
    PRelu,
    Unsqueeze {
        axes: Vec<usize>,
    },
    Squeeze {
        axes: Vec<usize>,
    },
    GridSample {
        align_corners: bool,
    },
    MatMul,
    Gemm {
        alpha: f32,
        beta: f32,
        trans_a: bool,
        trans_b: bool,
    },
    Softmax {
        axis: usize,
    },
    GlobalAveragePool,
    /// Inclusive prefix sum along one axis (exclusive=0, reverse=0).
    CumSum {
        axis: usize,
    },
    /// Sum-reduce a single axis. `keepdims` retains it as a size-1 dim.
    ReduceSum {
        axis: usize,
        keepdims: bool,
    },
    /// SpaceToDepth (DCR mode): [N,C,H,W] -> [N,C*b*b,H/b,W/b].
    SpaceToDepth {
        blocksize: usize,
    },
    /// DepthToSpace (DCR mode): [N,C,H,W] -> [N,C/(b*b),H*b,W*b].
    DepthToSpace {
        blocksize: usize,
    },
}

impl PlannedOpKind {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            PlannedOpKind::Conv2d(_) => "Conv2d",
            PlannedOpKind::Elementwise(ElementwiseKind::Add) => "Add",
            PlannedOpKind::Elementwise(ElementwiseKind::Mul) => "Mul",
            PlannedOpKind::Elementwise(ElementwiseKind::Sub) => "Sub",
            PlannedOpKind::Elementwise(ElementwiseKind::Div) => "Div",
            PlannedOpKind::Elementwise(ElementwiseKind::Max) => "Max",
            PlannedOpKind::Unary(UnaryKind::Identity) => "Identity",
            PlannedOpKind::Unary(UnaryKind::Sigmoid) => "Sigmoid",
            PlannedOpKind::Unary(UnaryKind::Relu) => "Relu",
            PlannedOpKind::Unary(UnaryKind::HardSwish) => "HardSwish",
            PlannedOpKind::Unary(UnaryKind::HardSigmoid { .. }) => "HardSigmoid",
            PlannedOpKind::Unary(UnaryKind::Sqrt) => "Sqrt",
            PlannedOpKind::Unary(UnaryKind::Pow { .. }) => "Pow",
            PlannedOpKind::Concat { .. } => "Concat",
            PlannedOpKind::Split { .. } => "Split",
            PlannedOpKind::Slice { .. } => "Slice",
            PlannedOpKind::Reshape { .. } => "Reshape",
            PlannedOpKind::Transpose { .. } => "Transpose",
            PlannedOpKind::MaxPool2d(_) => "MaxPool2d",
            PlannedOpKind::ResizeNearest { .. } => "ResizeNearest",
            PlannedOpKind::ResizeLinear { .. } => "ResizeLinear",
            PlannedOpKind::Pad { .. } => "Pad",
            PlannedOpKind::PRelu => "PRelu",
            PlannedOpKind::Unsqueeze { .. } => "Unsqueeze",
            PlannedOpKind::Squeeze { .. } => "Squeeze",
            PlannedOpKind::GridSample { .. } => "GridSample",
            PlannedOpKind::MatMul => "MatMul",
            PlannedOpKind::Gemm { .. } => "Gemm",
            PlannedOpKind::Softmax { .. } => "Softmax",
            PlannedOpKind::GlobalAveragePool => "GlobalAveragePool",
            PlannedOpKind::CumSum { .. } => "CumSum",
            PlannedOpKind::ReduceSum { .. } => "ReduceSum",
            PlannedOpKind::SpaceToDepth { .. } => "SpaceToDepth",
            PlannedOpKind::DepthToSpace { .. } => "DepthToSpace",
        }
    }
}

#[derive(Debug)]
pub(crate) enum PadMode {
    Constant,
    Reflect,
}

#[derive(Debug, Clone)]
pub(crate) struct Conv2dPlan {
    pub(crate) group: i64,
    pub(crate) pads: [i64; 4],
    pub(crate) strides: [i64; 2],
    pub(crate) dilations: [i64; 2],
    pub(crate) kernel_shape: [i64; 2],
}

#[derive(Debug)]
pub(crate) struct Pool2dPlan {
    pub(crate) pads: [i64; 4],
    pub(crate) strides: [i64; 2],
    pub(crate) dilations: [i64; 2],
    pub(crate) kernel_shape: [i64; 2],
}

#[derive(Debug)]
pub(crate) enum ElementwiseKind {
    Add,
    Mul,
    Sub,
    Div,
    Max,
}

#[derive(Debug)]
pub(crate) enum UnaryKind {
    Identity,
    Sigmoid,
    Relu,
    HardSwish,
    HardSigmoid {
        alpha: f32,
        beta: f32,
    },
    Sqrt,
    /// Power with a constant scalar exponent (sign-correct for integer exponents).
    Pow {
        exponent: f32,
    },
}
