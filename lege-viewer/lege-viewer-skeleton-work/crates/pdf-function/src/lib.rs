//! PDF function evaluation (ISO 32000-1 §7.10).
//!
//! A *pure evaluator*: this crate models the four function types the shading,
//! colour and transfer-function machinery needs — Type 2 (exponential
//! interpolation), Type 3 (stitching), Type 0 (sampled, in both the 1-input
//! shading form and the multi-input DeviceN/tint form) and Type 4 (PostScript
//! calculator) — and nothing about how they are spelled in a PDF. Translating a
//! `/FunctionType` dictionary (and decoding a Type 0 sample stream) into a
//! [`Function`] is the caller's job, so this crate stays dependency-free and
//! trivially testable.
//!
//! Two input arities coexist:
//!
//! * [`Function::eval`] evaluates a **single** input (the axial/radial shading
//!   parameter `t`) — the shading hot path, kept byte-for-byte as it was.
//! * [`Function::eval_n`] evaluates **m** inputs and dispatches every variant;
//!   the single-input variants read `inputs.first()`. Multi-input sampled
//!   functions ([`Function::SampledN`], multilinear interpolation over the
//!   `2^m` corners) and Type 4 programs ([`Function::PostScript`]) are only
//!   reachable through `eval_n`.
//!
//! Type 4 program text (the raw `{ … }` bytes of the stream) is turned into an
//! opaque [`PsProgram`] by [`parse_postscript`]; malformed text yields `None`
//! and the caller is expected to fall back to [`Function::Identity`].
//!
//! Robustness: this evaluates data from untrusted PDFs, so nothing here panics
//! or hangs on garbage. PostScript execution is bounded (max ops and stack
//! depth); domain/type/bounds errors abort the single evaluation and yield
//! range-clamped zeros. See [`parse_postscript`] and [`Function::eval_n`].

/// A callable PDF function. Evaluation clamps inputs to the declared domain
/// and clamps sampled/exponential outputs to their range where one is given.
#[derive(Debug, Clone)]
pub enum Function {
    /// Type 2 — exponential interpolation between `c0` and `c1`.
    Exponential { domain: [f32; 2], c0: Vec<f32>, c1: Vec<f32>, n: f32 },
    /// Type 3 — stitching of `functions` across `bounds`, each subfunction fed
    /// an input re-encoded into its `encode` interval.
    Stitching {
        domain: [f32; 2],
        functions: Vec<Function>,
        bounds: Vec<f32>,
        encode: Vec<[f32; 2]>,
    },
    /// Type 0 — a 1-input sampled function with `n_out` outputs.
    Sampled {
        domain: [f32; 2],
        encode: [f32; 2],
        /// Number of samples along the single input axis.
        size: usize,
        n_out: usize,
        /// Per-output `[min, max]` decode intervals (length `n_out`).
        decode: Vec<[f32; 2]>,
        /// Row-major samples normalized to `[0, 1]`, `size * n_out` values.
        samples: Vec<f32>,
    },
    /// Type 0 — an `m`-input sampled function with `n_out` outputs (DeviceN /
    /// tint transforms). `domain`, `encode` and `size` are per input dimension
    /// (`m = size.len()`). `samples` is row-major with **input 0 varying
    /// fastest** (ISO 32000-1 §7.10.2); each grid point stores `n_out`
    /// consecutive values normalized to `[0, 1]`, matching the 1-D
    /// [`Function::Sampled`] convention. Evaluated with multilinear
    /// interpolation over the `2^m` surrounding corners.
    SampledN {
        domain: Vec<[f32; 2]>,
        encode: Vec<[f32; 2]>,
        size: Vec<usize>,
        n_out: usize,
        /// Per-output `[min, max]` decode intervals (length `n_out`).
        decode: Vec<[f32; 2]>,
        samples: Vec<f32>,
    },
    /// Type 4 — a PostScript calculator function. `program` is the parsed body
    /// (see [`parse_postscript`]); `range.len()` fixes the output arity.
    PostScript {
        domain: Vec<[f32; 2]>,
        range: Vec<[f32; 2]>,
        program: PsProgram,
    },
    /// Identity: returns the (clamped) inputs unchanged. The tolerant fallback
    /// for an unparseable function.
    Identity { n_out: usize },
}

/// Cap on input dimensionality for [`Function::SampledN`]. A pathological
/// `/Size` array (or a corrupt stream reporting hundreds of inputs) would make
/// the `2^m` corner walk explode; beyond this we return zeros instead.
const MAX_SAMPLED_INPUTS: usize = 12;

impl Function {
    /// Number of output components this function produces.
    pub fn output_len(&self) -> usize {
        match self {
            Function::Exponential { c0, .. } => c0.len(),
            Function::Stitching { functions, .. } => {
                functions.first().map(Function::output_len).unwrap_or(0)
            }
            Function::Sampled { n_out, .. } => *n_out,
            Function::SampledN { n_out, .. } => *n_out,
            Function::PostScript { range, .. } => range.len(),
            Function::Identity { n_out } => *n_out,
        }
    }

    /// Evaluate at a single input value (the shading parameter). Returns the
    /// output components. Multi-input variants receive the value as their only
    /// input via [`Function::eval_n`].
    pub fn eval(&self, x: f32) -> Vec<f32> {
        match self {
            Function::Exponential { domain, c0, c1, n } => {
                let x = clamp(x, domain[0], domain[1]);
                let xn = if *n == 1.0 { x } else { x.powf(*n) };
                c0.iter().zip(c1).map(|(&a, &b)| a + xn * (b - a)).collect()
            }
            Function::Stitching { domain, functions, bounds, encode } => {
                let x = clamp(x, domain[0], domain[1]);
                if functions.is_empty() {
                    return Vec::new();
                }
                // Subfunction k: the first whose upper bound exceeds x. The
                // segment interval is [lo, hi) in the parent domain. A
                // malformed `/Bounds` (longer or shorter than `functions`
                // allows) must select *some* subfunction, never index out of
                // range.
                let k = bounds
                    .iter()
                    .position(|&b| x < b)
                    .unwrap_or(functions.len() - 1)
                    .min(functions.len() - 1);
                let lo = if k == 0 { domain[0] } else { bounds.get(k - 1).copied().unwrap_or(domain[0]) };
                let hi = bounds.get(k).copied().unwrap_or(domain[1]);
                let [e0, e1] = encode.get(k).copied().unwrap_or([0.0, 1.0]);
                let xe = interpolate(x, lo, hi, e0, e1);
                functions[k].eval(xe)
            }
            Function::Sampled { domain, encode, size, n_out, decode, samples } => {
                let x = clamp(x, domain[0], domain[1]);
                if *size == 0 || *n_out == 0 {
                    return vec![0.0; *n_out];
                }
                // Encode input to a sample coordinate in [0, size-1].
                let e = interpolate(x, domain[0], domain[1], encode[0], encode[1]);
                let e = clamp(e, 0.0, (*size - 1) as f32);
                let i0 = e.floor() as usize;
                let i1 = (i0 + 1).min(*size - 1);
                let frac = e - i0 as f32;
                let mut out = Vec::with_capacity(*n_out);
                for j in 0..*n_out {
                    let s0 = samples[i0 * n_out + j];
                    let s1 = samples[i1 * n_out + j];
                    let s = s0 + frac * (s1 - s0);
                    let [d0, d1] = decode.get(j).copied().unwrap_or([0.0, 1.0]);
                    out.push(d0 + s * (d1 - d0));
                }
                out
            }
            // Multi-input variants only make sense through `eval_n`; feed the
            // single input as input 0.
            Function::SampledN { .. } | Function::PostScript { .. } => self.eval_n(&[x]),
            Function::Identity { n_out } => vec![x; *n_out],
        }
    }

    /// Evaluate at `m` inputs. Every variant is handled: the single-input
    /// variants read `inputs.first()` (defaulting to `0.0`); [`Function::SampledN`]
    /// does multilinear interpolation and [`Function::PostScript`] runs the
    /// calculator program. Errors in the Type 4 path (type mismatch, stack
    /// underflow, div-by-zero, op/stack-bound overrun) never panic — they yield
    /// range-clamped zeros.
    pub fn eval_n(&self, inputs: &[f32]) -> Vec<f32> {
        match self {
            Function::SampledN { domain, encode, size, n_out, decode, samples } => {
                eval_sampled_n(inputs, domain, encode, size, *n_out, decode, samples)
            }
            Function::PostScript { domain, range, program } => {
                run_postscript(&program.body, inputs, domain, range)
            }
            _ => self.eval(inputs.first().copied().unwrap_or(0.0)),
        }
    }
}

#[inline]
fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    v.max(lo).min(hi)
}

/// Linear map of `v` from `[a0, a1]` onto `[b0, b1]`. A degenerate input
/// interval maps to `b0` (avoids a divide-by-zero on a zero-width domain).
#[inline]
fn interpolate(v: f32, a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    if a1 == a0 {
        return b0;
    }
    b0 + (v - a0) * (b1 - b0) / (a1 - a0)
}

// ---------------------------------------------------------------------------
// Type 0, multi-input (SampledN)
// ---------------------------------------------------------------------------

/// Multilinear interpolation of an `m`-input sampled function. `samples` is
/// row-major with input 0 varying fastest, `n_out` values per grid point.
fn eval_sampled_n(
    inputs: &[f32],
    domain: &[[f32; 2]],
    encode: &[[f32; 2]],
    size: &[usize],
    n_out: usize,
    decode: &[[f32; 2]],
    samples: &[f32],
) -> Vec<f32> {
    let m = size.len();
    if m == 0 || m > MAX_SAMPLED_INPUTS || n_out == 0 {
        return vec![0.0; n_out];
    }
    if size.contains(&0) {
        return vec![0.0; n_out];
    }

    // Per-dimension base index and interpolation fraction.
    let mut base = vec![0usize; m];
    let mut frac = vec![0f32; m];
    for d in 0..m {
        let dom = domain.get(d).copied().unwrap_or([0.0, 1.0]);
        let sz = size[d];
        let enc = encode.get(d).copied().unwrap_or([0.0, (sz - 1) as f32]);
        let xd = clamp(inputs.get(d).copied().unwrap_or(0.0), dom[0], dom[1]);
        let e = interpolate(xd, dom[0], dom[1], enc[0], enc[1]);
        let e = clamp(e, 0.0, (sz - 1) as f32);
        let b = e.floor() as usize;
        base[d] = b.min(sz - 1);
        frac[d] = e - base[d] as f32;
    }

    // Row-major strides with input 0 fastest: stride[0] = 1, stride[d] =
    // stride[d-1] * size[d-1]. Saturating so a corrupt `/Size` can never
    // overflow the index arithmetic (out-of-range reads fall back to 0).
    let mut stride = vec![1usize; m];
    for d in 1..m {
        stride[d] = stride[d - 1].saturating_mul(size[d - 1]);
    }

    let mut out = vec![0f32; n_out];
    let corners = 1usize << m;
    for c in 0..corners {
        let mut w = 1f32;
        let mut idx = 0usize;
        for d in 0..m {
            let high = (c >> d) & 1 == 1;
            let (coord, wd) = if high {
                ((base[d] + 1).min(size[d] - 1), frac[d])
            } else {
                (base[d], 1.0 - frac[d])
            };
            w *= wd;
            idx = idx.saturating_add(coord.saturating_mul(stride[d]));
        }
        if w == 0.0 {
            continue;
        }
        let off = idx.saturating_mul(n_out);
        for (j, o) in out.iter_mut().enumerate() {
            let s = samples.get(off.saturating_add(j)).copied().unwrap_or(0.0);
            *o += w * s;
        }
    }

    for (j, o) in out.iter_mut().enumerate() {
        let [d0, d1] = decode.get(j).copied().unwrap_or([0.0, 1.0]);
        *o = d0 + *o * (d1 - d0);
    }
    out
}

// ---------------------------------------------------------------------------
// Type 4, PostScript calculator
// ---------------------------------------------------------------------------

/// Execution bounds for a Type 4 program (ISO 32000-1 §7.10.5). `MAX_OPS`
/// caps the total instructions executed per evaluation (there are no loop
/// operators, so this only ever bites pathological/oversized programs);
/// `MAX_STACK` is the spec's operand-stack limit of 100.
const PS_MAX_OPS: usize = 10_000;
const PS_MAX_STACK: usize = 100;
/// Cap on `{}` nesting depth, bounding both parse and execution recursion.
const PS_MAX_DEPTH: usize = 100;

/// A parsed Type 4 program (the body between the outermost `{ }`). Opaque:
/// build one with [`parse_postscript`] and store it in
/// [`Function::PostScript`].
#[derive(Debug, Clone)]
pub struct PsProgram {
    body: Vec<Instr>,
}

/// A PostScript operand value. Type is tracked because several operators are
/// type-preserving (`add`, `mul`, …) or type-sensitive (`and`, `eq`, …).
#[derive(Debug, Clone, Copy, PartialEq)]
enum PsValue {
    Int(i64),
    Real(f64),
    Bool(bool),
}

impl PsValue {
    #[inline]
    fn as_f64(self) -> f64 {
        match self {
            PsValue::Int(i) => i as f64,
            PsValue::Real(r) => r,
            PsValue::Bool(b) => if b { 1.0 } else { 0.0 },
        }
    }

    /// Coerce to an integer. Reals truncate toward zero (saturating on
    /// non-finite / out-of-range values); booleans map to 0/1. Lenient by
    /// design so a stray real feeding an integer operator degrades instead of
    /// aborting.
    #[inline]
    fn as_i64(self) -> i64 {
        match self {
            PsValue::Int(i) => i,
            PsValue::Real(r) => if r.is_finite() { r.trunc() as i64 } else { 0 },
            PsValue::Bool(b) => b as i64,
        }
    }

    #[inline]
    fn as_bool(self) -> Result<bool, ()> {
        match self {
            PsValue::Bool(b) => Ok(b),
            _ => Err(()),
        }
    }
}

/// One instruction in a parsed program: a literal push, an operator, or a
/// nested procedure literal (`{ … }`, consumed by `if`/`ifelse`).
#[derive(Debug, Clone)]
enum Instr {
    Push(PsValue),
    Op(Op),
    Proc(Vec<Instr>),
}

/// Type 4 operators (ISO 32000-1 Table 42).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    // Arithmetic
    Abs, Add, Atan, Ceiling, Cos, Cvi, Cvr, Div, Exp, Floor, Idiv, Ln, Log,
    Mod, Mul, Neg, Round, Sin, Sqrt, Sub, Truncate,
    // Boolean / bitwise / relational
    And, Bitshift, Eq, False, Ge, Gt, Le, Lt, Ne, Not, Or, True, Xor,
    // Conditional
    If, Ifelse,
    // Stack
    Copy, Dup, Exch, Index, Pop, Roll,
}

impl Op {
    fn from_word(w: &str) -> Option<Op> {
        Some(match w {
            "abs" => Op::Abs,
            "add" => Op::Add,
            "atan" => Op::Atan,
            "ceiling" => Op::Ceiling,
            "cos" => Op::Cos,
            "cvi" => Op::Cvi,
            "cvr" => Op::Cvr,
            "div" => Op::Div,
            "exp" => Op::Exp,
            "floor" => Op::Floor,
            "idiv" => Op::Idiv,
            "ln" => Op::Ln,
            "log" => Op::Log,
            "mod" => Op::Mod,
            "mul" => Op::Mul,
            "neg" => Op::Neg,
            "round" => Op::Round,
            "sin" => Op::Sin,
            "sqrt" => Op::Sqrt,
            "sub" => Op::Sub,
            "truncate" => Op::Truncate,
            "and" => Op::And,
            "bitshift" => Op::Bitshift,
            "eq" => Op::Eq,
            "false" => Op::False,
            "ge" => Op::Ge,
            "gt" => Op::Gt,
            "le" => Op::Le,
            "lt" => Op::Lt,
            "ne" => Op::Ne,
            "not" => Op::Not,
            "or" => Op::Or,
            "true" => Op::True,
            "xor" => Op::Xor,
            "if" => Op::If,
            "ifelse" => Op::Ifelse,
            "copy" => Op::Copy,
            "dup" => Op::Dup,
            "exch" => Op::Exch,
            "index" => Op::Index,
            "pop" => Op::Pop,
            "roll" => Op::Roll,
            _ => return None,
        })
    }
}

/// A flat lexical token.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Sym {
    LBrace,
    RBrace,
    Num(PsValue),
    Op(Op),
}

/// Parse the raw bytes of a Type 4 stream (the `{ … }` program text) into an
/// opaque [`PsProgram`]. Returns `None` for malformed text — unbalanced
/// braces, an unknown token, a missing outer brace group, or nesting beyond
/// [`PS_MAX_DEPTH`] — so the caller can fall back to [`Function::Identity`].
///
/// Accepts PostScript whitespace and `%` end-of-line comments. Numbers parse
/// as integers where possible, otherwise as finite reals; `true`/`false` are
/// boolean literals; every other bareword must be a Table 42 operator.
pub fn parse_postscript(program: &[u8]) -> Option<PsProgram> {
    let toks = tokenize(program)?;
    // The program must be a single outer `{ … }` group.
    if toks.first() != Some(&Sym::LBrace) {
        return None;
    }
    let mut pos = 1usize;
    let body = parse_seq(&toks, &mut pos, 0)?;
    if toks.get(pos) != Some(&Sym::RBrace) {
        return None;
    }
    pos += 1;
    // Anything after the outer close is malformed.
    if pos != toks.len() {
        return None;
    }
    Some(PsProgram { body })
}

fn tokenize(bytes: &[u8]) -> Option<Vec<Sym>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            // PostScript whitespace: space, tab, CR, LF, FF, NUL.
            b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x00 => {
                i += 1;
            }
            b'%' => {
                // Comment to end of line.
                while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
            }
            b'{' => {
                out.push(Sym::LBrace);
                i += 1;
            }
            b'}' => {
                out.push(Sym::RBrace);
                i += 1;
            }
            _ => {
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if matches!(c, b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x00 | b'{' | b'}' | b'%') {
                        break;
                    }
                    i += 1;
                }
                let word = std::str::from_utf8(&bytes[start..i]).ok()?;
                out.push(classify(word)?);
            }
        }
    }
    Some(out)
}

fn classify(word: &str) -> Option<Sym> {
    // Integer first so "7" is Int, "7.0"/"7e0" are Real.
    if let Ok(i) = word.parse::<i64>() {
        return Some(Sym::Num(PsValue::Int(i)));
    }
    if let Ok(r) = word.parse::<f64>() {
        if r.is_finite() {
            return Some(Sym::Num(PsValue::Real(r)));
        }
        // Reject "inf"/"NaN" barewords: not valid program text.
        return None;
    }
    Op::from_word(word).map(Sym::Op)
}

/// Parse a run of instructions up to (but not consuming) the matching
/// `RBrace` or end of input. Nested `{ … }` become [`Instr::Proc`].
fn parse_seq(toks: &[Sym], pos: &mut usize, depth: usize) -> Option<Vec<Instr>> {
    if depth > PS_MAX_DEPTH {
        return None;
    }
    let mut out = Vec::new();
    while let Some(t) = toks.get(*pos) {
        match t {
            Sym::RBrace => break,
            Sym::LBrace => {
                *pos += 1;
                let inner = parse_seq(toks, pos, depth + 1)?;
                if toks.get(*pos) != Some(&Sym::RBrace) {
                    return None;
                }
                *pos += 1;
                out.push(Instr::Proc(inner));
            }
            Sym::Num(v) => {
                out.push(Instr::Push(*v));
                *pos += 1;
            }
            Sym::Op(Op::True) => {
                out.push(Instr::Push(PsValue::Bool(true)));
                *pos += 1;
            }
            Sym::Op(Op::False) => {
                out.push(Instr::Push(PsValue::Bool(false)));
                *pos += 1;
            }
            Sym::Op(op) => {
                out.push(Instr::Op(*op));
                *pos += 1;
            }
        }
    }
    Some(out)
}

/// An operand-stack entry: a value or a procedure reference (borrowed from the
/// parsed program, hence the lifetime).
#[derive(Debug, Clone, Copy)]
enum Item<'a> {
    V(PsValue),
    P(&'a [Instr]),
}

/// Run a parsed program over `inputs`, producing `range.len()` outputs.
///
/// Inputs are clamped to `/Domain` and pushed as reals; outputs are the top
/// `range.len()` stack values (the last output on top, per §7.10.5), each
/// clamped to its `/Range` pair. Any failure — type error, stack under/overflow,
/// or op-bound overrun — yields range-clamped zeros. Tolerances: `div`,
/// `idiv`, `mod` by zero push `0`; `sqrt` of a negative, `ln`/`log` of a
/// non-positive, and non-finite `exp` results push `0.0`.
fn run_postscript(body: &[Instr], inputs: &[f32], domain: &[[f32; 2]], range: &[[f32; 2]]) -> Vec<f32> {
    let zeros = || range.iter().map(|r| clamp(0.0, r[0], r[1])).collect::<Vec<f32>>();

    let mut stack: Vec<Item> = Vec::with_capacity(inputs.len().min(PS_MAX_STACK));
    for (i, &v) in inputs.iter().enumerate() {
        let d = domain.get(i).copied().unwrap_or([f32::NEG_INFINITY, f32::INFINITY]);
        stack.push(Item::V(PsValue::Real(clamp(v, d[0], d[1]) as f64)));
    }

    let mut budget = PS_MAX_OPS;
    if exec(body, &mut stack, &mut budget).is_err() {
        return zeros();
    }

    let k = range.len();
    if stack.len() < k {
        return zeros();
    }
    let mut out = Vec::with_capacity(k);
    for (j, item) in stack[stack.len() - k..].iter().enumerate() {
        let v = match item {
            Item::V(pv) => pv.as_f64() as f32,
            Item::P(_) => return zeros(),
        };
        let r = range[j];
        out.push(clamp(v, r[0], r[1]));
    }
    out
}

fn exec<'a>(prog: &'a [Instr], stack: &mut Vec<Item<'a>>, budget: &mut usize) -> Result<(), ()> {
    for instr in prog {
        if *budget == 0 {
            return Err(());
        }
        *budget -= 1;
        if stack.len() > PS_MAX_STACK {
            return Err(());
        }
        match instr {
            Instr::Push(v) => stack.push(Item::V(*v)),
            Instr::Proc(body) => stack.push(Item::P(body)),
            Instr::Op(op) => exec_op(*op, stack, budget)?,
        }
    }
    Ok(())
}

#[inline]
fn pop_item<'a>(s: &mut Vec<Item<'a>>) -> Result<Item<'a>, ()> {
    s.pop().ok_or(())
}

#[inline]
fn pop_v(s: &mut Vec<Item<'_>>) -> Result<PsValue, ()> {
    match pop_item(s)? {
        Item::V(v) => Ok(v),
        Item::P(_) => Err(()),
    }
}

#[inline]
fn pop_f(s: &mut Vec<Item<'_>>) -> Result<f64, ()> {
    Ok(pop_v(s)?.as_f64())
}

#[inline]
fn pop_i(s: &mut Vec<Item<'_>>) -> Result<i64, ()> {
    Ok(pop_v(s)?.as_i64())
}

fn exec_op<'a>(op: Op, s: &mut Vec<Item<'a>>, budget: &mut usize) -> Result<(), ()> {
    match op {
        // --- Type-preserving binary arithmetic ---
        Op::Add => {
            let b = pop_v(s)?;
            let a = pop_v(s)?;
            s.push(Item::V(bin_typed(a, b, i64::saturating_add, |x, y| x + y)));
        }
        Op::Sub => {
            let b = pop_v(s)?;
            let a = pop_v(s)?;
            s.push(Item::V(bin_typed(a, b, i64::saturating_sub, |x, y| x - y)));
        }
        Op::Mul => {
            let b = pop_v(s)?;
            let a = pop_v(s)?;
            s.push(Item::V(bin_typed(a, b, i64::saturating_mul, |x, y| x * y)));
        }
        // --- Type-preserving unary arithmetic ---
        Op::Neg => {
            let a = pop_v(s)?;
            s.push(Item::V(match a {
                PsValue::Int(x) => PsValue::Int(x.saturating_neg()),
                _ => PsValue::Real(-a.as_f64()),
            }));
        }
        Op::Abs => {
            let a = pop_v(s)?;
            s.push(Item::V(match a {
                PsValue::Int(x) => PsValue::Int(x.saturating_abs()),
                _ => PsValue::Real(a.as_f64().abs()),
            }));
        }
        // --- Real-valued arithmetic ---
        Op::Div => {
            let b = pop_f(s)?;
            let a = pop_f(s)?;
            // Div-by-zero tolerance: push 0.0 rather than +/-inf or NaN.
            s.push(Item::V(PsValue::Real(if b == 0.0 { 0.0 } else { a / b })));
        }
        Op::Idiv => {
            let b = pop_i(s)?;
            let a = pop_i(s)?;
            s.push(Item::V(PsValue::Int(a.checked_div(b).unwrap_or(0))));
        }
        Op::Mod => {
            let b = pop_i(s)?;
            let a = pop_i(s)?;
            s.push(Item::V(PsValue::Int(a.checked_rem(b).unwrap_or(0))));
        }
        Op::Sqrt => {
            let a = pop_f(s)?;
            s.push(Item::V(PsValue::Real(a.max(0.0).sqrt())));
        }
        Op::Sin => {
            let a = pop_f(s)?;
            s.push(Item::V(PsValue::Real(a.to_radians().sin())));
        }
        Op::Cos => {
            let a = pop_f(s)?;
            s.push(Item::V(PsValue::Real(a.to_radians().cos())));
        }
        Op::Atan => {
            let den = pop_f(s)?;
            let num = pop_f(s)?;
            let mut deg = num.atan2(den).to_degrees();
            if deg < 0.0 {
                deg += 360.0;
            }
            s.push(Item::V(PsValue::Real(deg)));
        }
        Op::Exp => {
            let exponent = pop_f(s)?;
            let base = pop_f(s)?;
            let r = base.powf(exponent);
            s.push(Item::V(PsValue::Real(if r.is_finite() { r } else { 0.0 })));
        }
        Op::Ln => {
            let a = pop_f(s)?;
            s.push(Item::V(PsValue::Real(if a > 0.0 { a.ln() } else { 0.0 })));
        }
        Op::Log => {
            let a = pop_f(s)?;
            s.push(Item::V(PsValue::Real(if a > 0.0 { a.log10() } else { 0.0 })));
        }
        // --- Conversions / rounding ---
        Op::Cvi => {
            let a = pop_v(s)?;
            s.push(Item::V(PsValue::Int(a.as_i64())));
        }
        Op::Cvr => {
            let a = pop_f(s)?;
            s.push(Item::V(PsValue::Real(a)));
        }
        Op::Floor => {
            let a = pop_v(s)?;
            s.push(Item::V(round_typed(a, f64::floor)));
        }
        Op::Ceiling => {
            let a = pop_v(s)?;
            s.push(Item::V(round_typed(a, f64::ceil)));
        }
        Op::Round => {
            let a = pop_v(s)?;
            s.push(Item::V(round_typed(a, f64::round)));
        }
        Op::Truncate => {
            let a = pop_v(s)?;
            s.push(Item::V(round_typed(a, f64::trunc)));
        }
        // --- Boolean / bitwise ---
        Op::And => bitwise(s, |x, y| x & y, |x, y| x && y)?,
        Op::Or => bitwise(s, |x, y| x | y, |x, y| x || y)?,
        Op::Xor => bitwise(s, |x, y| x ^ y, |x, y| x ^ y)?,
        Op::Not => {
            let a = pop_v(s)?;
            s.push(Item::V(match a {
                PsValue::Bool(b) => PsValue::Bool(!b),
                _ => PsValue::Int(!a.as_i64()),
            }));
        }
        Op::Bitshift => {
            let shift = pop_i(s)?;
            let v = pop_i(s)?;
            let r = if shift >= 0 {
                if shift >= 64 { 0 } else { v.wrapping_shl(shift as u32) }
            } else {
                let s = -shift;
                if s >= 64 { if v < 0 { -1 } else { 0 } } else { v >> s }
            };
            s.push(Item::V(PsValue::Int(r)));
        }
        // --- Relational ---
        Op::Eq => {
            let b = pop_v(s)?;
            let a = pop_v(s)?;
            s.push(Item::V(PsValue::Bool(ps_eq(a, b))));
        }
        Op::Ne => {
            let b = pop_v(s)?;
            let a = pop_v(s)?;
            s.push(Item::V(PsValue::Bool(!ps_eq(a, b))));
        }
        Op::Gt => rel(s, |a, b| a > b)?,
        Op::Ge => rel(s, |a, b| a >= b)?,
        Op::Lt => rel(s, |a, b| a < b)?,
        Op::Le => rel(s, |a, b| a <= b)?,
        // `true`/`false` are lexed as literals; reaching here means push.
        Op::True => s.push(Item::V(PsValue::Bool(true))),
        Op::False => s.push(Item::V(PsValue::Bool(false))),
        // --- Conditionals ---
        Op::If => {
            let proc = match pop_item(s)? {
                Item::P(p) => p,
                Item::V(_) => return Err(()),
            };
            let cond = pop_v(s)?.as_bool()?;
            if cond {
                exec(proc, s, budget)?;
            }
        }
        Op::Ifelse => {
            let p2 = match pop_item(s)? {
                Item::P(p) => p,
                Item::V(_) => return Err(()),
            };
            let p1 = match pop_item(s)? {
                Item::P(p) => p,
                Item::V(_) => return Err(()),
            };
            let cond = pop_v(s)?.as_bool()?;
            if cond {
                exec(p1, s, budget)?;
            } else {
                exec(p2, s, budget)?;
            }
        }
        // --- Stack ---
        Op::Pop => {
            pop_item(s)?;
        }
        Op::Dup => {
            let top = *s.last().ok_or(())?;
            s.push(top);
        }
        Op::Exch => {
            let l = s.len();
            if l < 2 {
                return Err(());
            }
            s.swap(l - 1, l - 2);
        }
        Op::Copy => {
            let n = pop_i(s)?;
            if n < 0 {
                return Err(());
            }
            let n = n as usize;
            let l = s.len();
            if n > l || l + n > PS_MAX_STACK {
                return Err(());
            }
            let dup: Vec<Item> = s[l - n..].to_vec();
            s.extend(dup);
        }
        Op::Index => {
            let n = pop_i(s)?;
            if n < 0 {
                return Err(());
            }
            let n = n as usize;
            let l = s.len();
            if n >= l {
                return Err(());
            }
            s.push(s[l - 1 - n]);
        }
        Op::Roll => {
            let j = pop_i(s)?;
            let n = pop_i(s)?;
            if n < 0 {
                return Err(());
            }
            let n = n as usize;
            let l = s.len();
            if n > l {
                return Err(());
            }
            if n >= 1 {
                // Positive j = upward motion (rotate top toward bottom of the
                // group), i.e. rotate_right; negative j rotates left.
                let shift = (((j % n as i64) + n as i64) % n as i64) as usize;
                s[l - n..].rotate_right(shift);
            }
        }
    }
    Ok(())
}

/// Type-preserving binary op: integer path when both operands are integers,
/// otherwise real.
#[inline]
fn bin_typed(a: PsValue, b: PsValue, fi: impl Fn(i64, i64) -> i64, fr: impl Fn(f64, f64) -> f64) -> PsValue {
    match (a, b) {
        (PsValue::Int(x), PsValue::Int(y)) => PsValue::Int(fi(x, y)),
        _ => PsValue::Real(fr(a.as_f64(), b.as_f64())),
    }
}

/// `floor`/`ceiling`/`round`/`truncate`: integers pass through unchanged,
/// reals (and coerced bools) go through the float rounding function.
#[inline]
fn round_typed(a: PsValue, f: impl Fn(f64) -> f64) -> PsValue {
    match a {
        PsValue::Int(x) => PsValue::Int(x),
        _ => PsValue::Real(f(a.as_f64())),
    }
}

/// `and`/`or`/`xor`: bitwise on two integers, logical on two booleans. A mixed
/// bool/number pairing is a PostScript type error.
#[inline]
fn bitwise(s: &mut Vec<Item<'_>>, fi: impl Fn(i64, i64) -> i64, fb: impl Fn(bool, bool) -> bool) -> Result<(), ()> {
    let b = pop_v(s)?;
    let a = pop_v(s)?;
    let r = match (a, b) {
        (PsValue::Bool(x), PsValue::Bool(y)) => PsValue::Bool(fb(x, y)),
        (PsValue::Bool(_), _) | (_, PsValue::Bool(_)) => return Err(()),
        _ => PsValue::Int(fi(a.as_i64(), b.as_i64())),
    };
    s.push(Item::V(r));
    Ok(())
}

/// `eq`/`ne` equality: booleans compare as booleans, everything else by
/// numeric value (int/real coercion).
#[inline]
fn ps_eq(a: PsValue, b: PsValue) -> bool {
    match (a, b) {
        (PsValue::Bool(x), PsValue::Bool(y)) => x == y,
        (PsValue::Bool(_), _) | (_, PsValue::Bool(_)) => false,
        _ => a.as_f64() == b.as_f64(),
    }
}

/// Numeric relational operator (`gt`/`ge`/`lt`/`le`).
#[inline]
fn rel(s: &mut Vec<Item<'_>>, f: impl Fn(f64, f64) -> bool) -> Result<(), ()> {
    let b = pop_f(s)?;
    let a = pop_f(s)?;
    s.push(Item::V(PsValue::Bool(f(a, b))));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn exponential_linear_ramp() {
        let f = Function::Exponential {
            domain: [0.0, 1.0],
            c0: vec![0.0, 0.0, 0.0],
            c1: vec![1.0, 0.5, 0.0],
            n: 1.0,
        };
        assert_eq!(f.eval(0.0), vec![0.0, 0.0, 0.0]);
        assert_eq!(f.eval(1.0), vec![1.0, 0.5, 0.0]);
        let mid = f.eval(0.5);
        assert!((mid[0] - 0.5).abs() < 1e-6 && (mid[1] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn exponential_clamps_domain() {
        let f = Function::Exponential { domain: [0.0, 1.0], c0: vec![0.0], c1: vec![1.0], n: 1.0 };
        assert_eq!(f.eval(-3.0), vec![0.0]);
        assert_eq!(f.eval(9.0), vec![1.0]);
    }

    #[test]
    fn stitching_selects_subfunction_and_reencodes() {
        // Two halves: [0,0.5) ramps 0→1, [0.5,1] ramps 1→0, each over encode [0,1].
        let up = Function::Exponential { domain: [0.0, 1.0], c0: vec![0.0], c1: vec![1.0], n: 1.0 };
        let down = Function::Exponential { domain: [0.0, 1.0], c0: vec![1.0], c1: vec![0.0], n: 1.0 };
        let f = Function::Stitching {
            domain: [0.0, 1.0],
            functions: vec![up, down],
            bounds: vec![0.5],
            encode: vec![[0.0, 1.0], [0.0, 1.0]],
        };
        assert!((f.eval(0.25)[0] - 0.5).abs() < 1e-6);
        assert!((f.eval(0.75)[0] - 0.5).abs() < 1e-6);
        assert!((f.eval(0.5)[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sampled_interpolates_and_decodes() {
        // size 2, one output, samples [0, 1] decoded to [0, 10].
        let f = Function::Sampled {
            domain: [0.0, 1.0],
            encode: [0.0, 1.0],
            size: 2,
            n_out: 1,
            decode: vec![[0.0, 10.0]],
            samples: vec![0.0, 1.0],
        };
        assert!((f.eval(0.0)[0] - 0.0).abs() < 1e-6);
        assert!((f.eval(1.0)[0] - 10.0).abs() < 1e-6);
        assert!((f.eval(0.5)[0] - 5.0).abs() < 1e-6);
    }

    #[test]
    fn stitching_survives_malformed_bounds() {
        let ramp = || Function::Exponential { domain: [0.0, 1.0], c0: vec![0.0], c1: vec![1.0], n: 1.0 };
        // /Bounds longer than functions allows: k must clamp, not index OOB.
        let long = Function::Stitching {
            domain: [0.0, 1.0],
            functions: vec![ramp()],
            bounds: vec![0.3, 0.6],
            encode: vec![[0.0, 1.0]],
        };
        // /Bounds shorter than functions expects: lo/hi fall back to domain.
        let short = Function::Stitching {
            domain: [0.0, 1.0],
            functions: vec![ramp(), ramp(), ramp()],
            bounds: Vec::new(),
            encode: Vec::new(),
        };
        for f in [long, short] {
            for x in [0.0, 0.25, 0.45, 0.7, 1.0] {
                assert_eq!(f.eval(x).len(), 1);
            }
        }
    }

    #[test]
    fn identity_returns_input() {
        let f = Function::Identity { n_out: 3 };
        assert_eq!(f.eval(0.4), vec![0.4, 0.4, 0.4]);
    }

    // ----------------------------------------------------------------------
    // eval_n dispatch for the single-input variants
    // ----------------------------------------------------------------------

    #[test]
    fn eval_n_dispatches_single_input_variants() {
        let exp = Function::Exponential { domain: [0.0, 1.0], c0: vec![0.0], c1: vec![1.0], n: 1.0 };
        assert!((exp.eval_n(&[0.5])[0] - 0.5).abs() < 1e-6);
        // Extra inputs ignored; missing input defaults to 0.
        assert!((exp.eval_n(&[0.5, 9.0])[0] - 0.5).abs() < 1e-6);
        assert_eq!(exp.eval_n(&[]), vec![0.0]);

        let id = Function::Identity { n_out: 2 };
        assert_eq!(id.eval_n(&[0.3]), vec![0.3, 0.3]);

        let samp = Function::Sampled {
            domain: [0.0, 1.0],
            encode: [0.0, 1.0],
            size: 2,
            n_out: 1,
            decode: vec![[0.0, 10.0]],
            samples: vec![0.0, 1.0],
        };
        assert!((samp.eval_n(&[0.5])[0] - 5.0).abs() < 1e-6);
    }

    // ----------------------------------------------------------------------
    // SampledN — multilinear interpolation
    // ----------------------------------------------------------------------

    /// 2 inputs (2x2 grid), 3 outputs. Corner values are chosen so each output
    /// is a distinct bilinear surface. Sample layout: input 0 fastest, so the
    /// four grid points are (0,0),(1,0),(0,1),(1,1).
    fn bilinear_2in_3out() -> Function {
        // Per-corner outputs [o0,o1,o2]:
        //  (0,0): 0,0,1   (1,0): 1,0,0
        //  (0,1): 0,1,0   (1,1): 1,1,1
        Function::SampledN {
            domain: vec![[0.0, 1.0], [0.0, 1.0]],
            encode: vec![[0.0, 1.0], [0.0, 1.0]],
            size: vec![2, 2],
            n_out: 3,
            decode: vec![[0.0, 1.0], [0.0, 1.0], [0.0, 1.0]],
            samples: vec![
                0.0, 0.0, 1.0, // (0,0)
                1.0, 0.0, 0.0, // (1,0)
                0.0, 1.0, 0.0, // (0,1)
                1.0, 1.0, 1.0, // (1,1)
            ],
        }
    }

    #[test]
    fn sampled_n_exact_corners() {
        let f = bilinear_2in_3out();
        assert_eq!(f.eval_n(&[0.0, 0.0]), vec![0.0, 0.0, 1.0]);
        assert_eq!(f.eval_n(&[1.0, 0.0]), vec![1.0, 0.0, 0.0]);
        assert_eq!(f.eval_n(&[0.0, 1.0]), vec![0.0, 1.0, 0.0]);
        assert_eq!(f.eval_n(&[1.0, 1.0]), vec![1.0, 1.0, 1.0]);
    }

    #[test]
    fn sampled_n_midpoint_bilinear_blend() {
        let f = bilinear_2in_3out();
        let mid = f.eval_n(&[0.5, 0.5]);
        // o0 = x average: (0+1+0+1)/4 = 0.5
        // o1 = y average: (0+0+1+1)/4 = 0.5
        // o2 = (1+0+0+1)/4 = 0.5
        assert!((mid[0] - 0.5).abs() < 1e-6);
        assert!((mid[1] - 0.5).abs() < 1e-6);
        assert!((mid[2] - 0.5).abs() < 1e-6);
        // Edge midpoint on input 0 only:
        let e = f.eval_n(&[0.5, 0.0]);
        assert!((e[0] - 0.5).abs() < 1e-6);
        assert!((e[1] - 0.0).abs() < 1e-6);
        assert!((e[2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sampled_n_encode_and_decode_applied() {
        // Single input over a 2-sample axis, domain [10,20] encoded to [0,1],
        // sample values 0,1 decoded to [-1, 1].
        let f = Function::SampledN {
            domain: vec![[10.0, 20.0]],
            encode: vec![[0.0, 1.0]],
            size: vec![2],
            n_out: 1,
            decode: vec![[-1.0, 1.0]],
            samples: vec![0.0, 1.0],
        };
        assert!((f.eval_n(&[10.0])[0] + 1.0).abs() < 1e-6); // -1
        assert!((f.eval_n(&[20.0])[0] - 1.0).abs() < 1e-6); //  1
        assert!((f.eval_n(&[15.0])[0] - 0.0).abs() < 1e-6); //  midpoint
        // Domain clamp:
        assert!((f.eval_n(&[0.0])[0] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn sampled_n_three_inputs_trilinear() {
        // 2x2x2 grid, single output equal to the sum of the three normalized
        // coordinates / 3 at the corners → trilinear centre is the mean of the
        // eight corners.
        // Corner value = (i + j + k) as f32, input 0 (i) fastest.
        let mut samples = Vec::new();
        for k in 0..2 {
            for j in 0..2 {
                for i in 0..2 {
                    samples.push((i + j + k) as f32);
                }
            }
        }
        let f = Function::SampledN {
            domain: vec![[0.0, 1.0], [0.0, 1.0], [0.0, 1.0]],
            encode: vec![[0.0, 1.0], [0.0, 1.0], [0.0, 1.0]],
            size: vec![2, 2, 2],
            n_out: 1,
            decode: vec![[0.0, 1.0]],
            samples,
        };
        // Centre = mean of corner sums = (0+1+1+2+1+2+2+3)/8 = 1.5
        assert!((f.eval_n(&[0.5, 0.5, 0.5])[0] - 1.5).abs() < 1e-6);
        // A corner:
        assert!((f.eval_n(&[1.0, 1.0, 1.0])[0] - 3.0).abs() < 1e-6);
        // Vary only input 1:
        assert!((f.eval_n(&[0.0, 1.0, 0.0])[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sampled_n_degenerate_size_one_axis() {
        // input 0 has a single sample (constant along that axis); input 1 has 2.
        let f = Function::SampledN {
            domain: vec![[0.0, 1.0], [0.0, 1.0]],
            encode: vec![[0.0, 0.0], [0.0, 1.0]],
            size: vec![1, 2],
            n_out: 1,
            decode: vec![[0.0, 1.0]],
            // grid points: (0,0)=0.2, (0,1)=0.8  (input 0 fastest, but size 1)
            samples: vec![0.2, 0.8],
        };
        assert!((f.eval_n(&[0.3, 0.0])[0] - 0.2).abs() < 1e-6);
        assert!((f.eval_n(&[0.9, 1.0])[0] - 0.8).abs() < 1e-6);
        assert!((f.eval_n(&[0.5, 0.5])[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sampled_n_guards_pathological_dims() {
        // Too many input dims → zeros of the right length.
        let f = Function::SampledN {
            domain: vec![[0.0, 1.0]; 20],
            encode: vec![[0.0, 1.0]; 20],
            size: vec![2; 20],
            n_out: 2,
            decode: vec![[0.0, 1.0], [0.0, 1.0]],
            samples: vec![0.0; 8],
        };
        assert_eq!(f.eval_n(&[0.5; 20]), vec![0.0, 0.0]);

        // Zero-size axis → zeros.
        let z = Function::SampledN {
            domain: vec![[0.0, 1.0], [0.0, 1.0]],
            encode: vec![[0.0, 1.0], [0.0, 1.0]],
            size: vec![0, 2],
            n_out: 1,
            decode: vec![[0.0, 1.0]],
            samples: vec![],
        };
        assert_eq!(z.eval_n(&[0.5, 0.5]), vec![0.0]);
    }

    // ----------------------------------------------------------------------
    // PostScript — parsing
    // ----------------------------------------------------------------------

    fn ps(program: &str, domain: Vec<[f32; 2]>, range: Vec<[f32; 2]>) -> Function {
        Function::PostScript {
            domain,
            range,
            program: parse_postscript(program.as_bytes()).expect("program should parse"),
        }
    }

    #[test]
    fn ps_rejects_garbage() {
        assert!(parse_postscript(b"not a program").is_none()); // no outer brace
        assert!(parse_postscript(b"{ 1 2 frobnicate }").is_none()); // unknown op
        assert!(parse_postscript(b"{ 1 2 add").is_none()); // unbalanced
        assert!(parse_postscript(b"{ 1 } extra").is_none()); // trailing junk
        assert!(parse_postscript(b"}{").is_none());
        assert!(parse_postscript(b"").is_none());
        assert!(parse_postscript(b"{ 1 { 2 add }").is_none()); // unbalanced nested
    }

    #[test]
    fn ps_accepts_empty_and_comments() {
        assert!(parse_postscript(b"{ }").is_some());
        assert!(parse_postscript(b"{ % a comment\n 1 2 add }").is_some());
    }

    // ----------------------------------------------------------------------
    // PostScript — evaluation
    // ----------------------------------------------------------------------

    #[test]
    fn ps_tint_transform_like() {
        // A plausible DeviceN→2 tint transform: { dup 0.8 mul exch 0.3 mul }
        let f = ps(
            "{ dup 0.8 mul exch 0.3 mul }",
            vec![[0.0, 1.0]],
            vec![[0.0, 1.0], [0.0, 1.0]],
        );
        let out = f.eval_n(&[0.5]);
        assert_eq!(out.len(), 2);
        // Stack trace: 0.5 -> dup(0.5 0.5) -> 0.8 mul(0.5 0.4) -> exch(0.4 0.5)
        //   -> 0.3 mul(0.4 0.15). Outputs bottom→top: [0.4, 0.15].
        assert!((out[0] - 0.4).abs() < 1e-6);
        assert!((out[1] - 0.15).abs() < 1e-6);
    }

    #[test]
    fn ps_degree_trig() {
        let sin = ps("{ 90 sin }", vec![], vec![[-1.0, 1.0]]);
        assert!((sin.eval_n(&[])[0] - 1.0).abs() < 1e-6);
        let cos = ps("{ 0 cos }", vec![], vec![[-1.0, 1.0]]);
        assert!((cos.eval_n(&[])[0] - 1.0).abs() < 1e-6);
        let atan = ps("{ 1 1 atan }", vec![], vec![[0.0, 360.0]]);
        assert!((atan.eval_n(&[])[0] - 45.0).abs() < 1e-4);
        // atan normalizes to [0,360): -1 den gives second/third quadrant.
        let atan2 = ps("{ 0 -1 atan }", vec![], vec![[0.0, 360.0]]);
        assert!((atan2.eval_n(&[])[0] - 180.0).abs() < 1e-4);
    }

    #[test]
    fn ps_integer_ops() {
        let idiv = ps("{ 7 2 idiv }", vec![], vec![[-100.0, 100.0]]);
        assert_eq!(idiv.eval_n(&[])[0], 3.0);
        let md = ps("{ 7 2 mod }", vec![], vec![[-100.0, 100.0]]);
        assert_eq!(md.eval_n(&[])[0], 1.0);
        let ab = ps("{ -3 abs }", vec![], vec![[-100.0, 100.0]]);
        assert_eq!(ab.eval_n(&[])[0], 3.0);
        // cvi truncates toward zero.
        let cvi = ps("{ -2.7 cvi }", vec![], vec![[-100.0, 100.0]]);
        assert_eq!(cvi.eval_n(&[])[0], -2.0);
        // bitshift: left for positive, right (arithmetic) for negative.
        let shl = ps("{ 1 4 bitshift }", vec![], vec![[-1000.0, 1000.0]]);
        assert_eq!(shl.eval_n(&[])[0], 16.0);
        let shr = ps("{ 16 -2 bitshift }", vec![], vec![[-1000.0, 1000.0]]);
        assert_eq!(shr.eval_n(&[])[0], 4.0);
    }

    #[test]
    fn ps_stack_ops() {
        // 1 2 3 3 -1 roll -> group [1,2,3] rotate_right(2) -> [2,3,1] (top=1).
        // Read all three outputs bottom→top.
        let roll = ps("{ 1 2 3 3 -1 roll }", vec![], vec![[-10.0, 10.0], [-10.0, 10.0], [-10.0, 10.0]]);
        assert_eq!(roll.eval_n(&[]), vec![2.0, 3.0, 1.0]);

        // 2 index copies the element two below the top: (10 20 30) 2 index -> 10.
        let idx = ps("{ 10 20 30 2 index }", vec![], vec![[-100.0, 100.0]]);
        assert_eq!(idx.eval_n(&[])[0], 10.0);

        // 3 copy duplicates the top three.
        let cp = ps(
            "{ 1 2 3 3 copy }",
            vec![],
            vec![[-10.0, 10.0], [-10.0, 10.0], [-10.0, 10.0], [-10.0, 10.0], [-10.0, 10.0], [-10.0, 10.0]],
        );
        assert_eq!(cp.eval_n(&[]), vec![1.0, 2.0, 3.0, 1.0, 2.0, 3.0]);

        // exch swaps top two.
        let ex = ps("{ 1 2 exch }", vec![], vec![[-10.0, 10.0], [-10.0, 10.0]]);
        assert_eq!(ex.eval_n(&[]), vec![2.0, 1.0]);

        // dup then pop.
        let dp = ps("{ 5 dup pop }", vec![], vec![[-10.0, 10.0]]);
        assert_eq!(dp.eval_n(&[])[0], 5.0);
    }

    #[test]
    fn ps_positive_roll() {
        // 1 2 3 3 1 roll -> rotate_right(1) on [1,2,3] -> [3,1,2] (top=2).
        let roll = ps("{ 1 2 3 3 1 roll }", vec![], vec![[-10.0, 10.0], [-10.0, 10.0], [-10.0, 10.0]]);
        assert_eq!(roll.eval_n(&[]), vec![3.0, 1.0, 2.0]);
    }

    #[test]
    fn ps_if_both_branches() {
        // true branch executes.
        let t = ps("{ true { 42 } if }", vec![], vec![[0.0, 100.0]]);
        assert_eq!(t.eval_n(&[])[0], 42.0);
        // false branch skips → leaves the pre-pushed 7.
        let fbr = ps("{ 7 false { 42 } if }", vec![], vec![[0.0, 100.0]]);
        assert_eq!(fbr.eval_n(&[])[0], 7.0);
    }

    #[test]
    fn ps_ifelse_both_branches() {
        let prog = "{ dup 0.5 lt { 0 } { 1 } ifelse }";
        let lo = ps(prog, vec![[0.0, 1.0]], vec![[0.0, 1.0]]);
        assert_eq!(lo.eval_n(&[0.2])[0], 0.0);
        let hi = ps(prog, vec![[0.0, 1.0]], vec![[0.0, 1.0]]);
        assert_eq!(hi.eval_n(&[0.8])[0], 1.0);
    }

    #[test]
    fn ps_nested_procedures() {
        // Nested ifelse inside a branch.
        let prog = "{ dup 0.33 lt { 0 } { dup 0.66 lt { 1 } { 2 } ifelse } ifelse }";
        let f = ps(prog, vec![[0.0, 1.0]], vec![[0.0, 2.0]]);
        assert_eq!(f.eval_n(&[0.1])[0], 0.0);
        assert_eq!(f.eval_n(&[0.5])[0], 1.0);
        assert_eq!(f.eval_n(&[0.9])[0], 2.0);
    }

    #[test]
    fn ps_comparison_boolean_arithmetic_chain() {
        // (a > 0) and (a < 10) -> if true push a*2 else 0.
        let prog = "{ dup dup 0 gt exch 10 lt and { 2 mul } { pop 0 } ifelse }";
        let f = ps(prog, vec![[-100.0, 100.0]], vec![[-1000.0, 1000.0]]);
        assert_eq!(f.eval_n(&[5.0])[0], 10.0);
        assert_eq!(f.eval_n(&[-5.0])[0], 0.0);
        assert_eq!(f.eval_n(&[50.0])[0], 0.0);
    }

    #[test]
    fn ps_range_clamping() {
        // Output 5.0 clamped into [0,1].
        let f = ps("{ 5 }", vec![], vec![[0.0, 1.0]]);
        assert_eq!(f.eval_n(&[])[0], 1.0);
        let g = ps("{ -5 }", vec![], vec![[0.0, 1.0]]);
        assert_eq!(g.eval_n(&[])[0], 0.0);
    }

    #[test]
    fn ps_div_by_zero_tolerated() {
        let f = ps("{ 1 0 div }", vec![], vec![[-10.0, 10.0]]);
        assert_eq!(f.eval_n(&[])[0], 0.0);
        let g = ps("{ 5 0 idiv }", vec![], vec![[-10.0, 10.0]]);
        assert_eq!(g.eval_n(&[])[0], 0.0);
        let h = ps("{ 5 0 mod }", vec![], vec![[-10.0, 10.0]]);
        assert_eq!(h.eval_n(&[])[0], 0.0);
    }

    #[test]
    fn ps_domain_error_yields_zeros() {
        // Type error: `add` on a boolean and a number → abort → range-clamped 0.
        let f = ps("{ true 1 add }", vec![], vec![[2.0, 5.0]]);
        // 0 clamped into [2,5] → 2.
        assert_eq!(f.eval_n(&[])[0], 2.0);
        // Stack underflow.
        let g = ps("{ add }", vec![], vec![[0.0, 1.0]]);
        assert_eq!(g.eval_n(&[])[0], 0.0);
    }

    #[test]
    fn ps_op_bound_does_not_hang() {
        // A program far exceeding PS_MAX_OPS instructions: must abort → zeros,
        // never hang.
        let mut prog = String::from("{ 0 ");
        for _ in 0..6000 {
            prog.push_str("1 pop ");
        }
        prog.push('}');
        let f = ps(&prog, vec![], vec![[0.0, 1.0]]);
        assert_eq!(f.eval_n(&[])[0], 0.0);
    }

    #[test]
    fn ps_stack_overflow_aborts() {
        // Grow the stack past PS_MAX_STACK via repeated dup → abort → zeros.
        let mut prog = String::from("{ 1 ");
        for _ in 0..300 {
            prog.push_str("dup ");
        }
        prog.push('}');
        let f = ps(&prog, vec![], vec![[0.0, 1.0]]);
        assert_eq!(f.eval_n(&[])[0], 0.0);
    }

    #[test]
    fn ps_type_preserving_vs_real() {
        // int add stays int, mixing a real promotes.
        let i = ps("{ 2 3 add }", vec![], vec![[0.0, 100.0]]);
        assert_eq!(i.eval_n(&[])[0], 5.0);
        let r = ps("{ 2 3.5 add }", vec![], vec![[0.0, 100.0]]);
        assert_eq!(r.eval_n(&[])[0], 5.5);
        // sqrt of negative tolerated → 0.
        let sq = ps("{ -4 sqrt }", vec![], vec![[-10.0, 10.0]]);
        assert_eq!(sq.eval_n(&[])[0], 0.0);
        // exp: 2^10 = 1024.
        let e = ps("{ 2 10 exp }", vec![], vec![[0.0, 100000.0]]);
        assert!((e.eval_n(&[])[0] - 1024.0).abs() < 1e-3);
    }

    #[test]
    fn ps_output_len_reports_range() {
        let f = ps("{ dup }", vec![[0.0, 1.0]], vec![[0.0, 1.0], [0.0, 1.0]]);
        assert_eq!(f.output_len(), 2);
    }

    #[test]
    fn sampled_n_output_len() {
        let f = bilinear_2in_3out();
        assert_eq!(f.output_len(), 3);
    }
}
