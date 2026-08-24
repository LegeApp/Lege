//! Content-stream interpretation: the PDF graphics-state machine that turns a
//! page's operators + resolved resources into a [`semantic::SemanticPage`]
//! (roadmap §7 Phase 2).
//!
//! Interpretation state is page-local by nature — the whole reason pages
//! parallelize. Everything here is owned by one compilation job; nothing is
//! shared except the immutable [`DocumentSnapshot`] it reads from, so any
//! number of pages compile concurrently with no coordination.
//!
//! Lowering a [`semantic::SemanticPage`] into the fully-explicit, interned
//! [`pdf_page_ir::CompiledPage`] is deliberately Phase 3 work; this crate's
//! Phase 2 product is the semantic page and its deterministic dump.

use pdf_document::{DocumentError, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{CompiledPage, FillRule, PageBounds, Rect};

pub mod dump;
mod interpret;
mod lower;
mod mesh;
pub mod semantic;
pub mod state;
pub mod tokenizer;

pub use semantic::{
    ActualTextSpan, ActualTextSpanId, SemColor, SemFont, SemImage, SemanticOp, SemanticPage,
    TextElement, TextRun,
};
pub use state::GraphicsState;
pub use tokenizer::{ContentLexer, Lexeme, Operand};

/// Budgets applied while interpreting a content stream. Owned by the caller,
/// threaded by reference — never global (blueprint §4.4).
#[derive(Debug, Clone)]
pub struct ContentLimits {
    /// Maximum operators executed across a page (including nested forms).
    pub max_ops: u64,
    /// Maximum XObject/pattern invocation nesting (recursion guard).
    pub max_invoke_depth: u32,
    /// Maximum nesting of a `/ColorSpace` entry that names another space
    /// (`/Indexed` over `/Separation` over `/ICCBased`, …). A malformed or
    /// cyclic resource cannot recurse past this.
    pub max_colorspace_depth: u32,
    /// Maximum `/Functions` nesting inside a Type 3 (stitching) function.
    pub max_function_depth: u32,
    /// Maximum nesting of a page's `/Contents` when it is an array of arrays
    /// or a reference chain.
    pub max_content_depth: usize,
    /// Limits for the underlying content tokenizer.
    pub syntax: pdf_syntax::SyntaxLimits,
}

impl Default for ContentLimits {
    fn default() -> Self {
        Self {
            max_ops: 5_000_000,
            max_invoke_depth: 16,
            max_colorspace_depth: 8,
            max_function_depth: 32,
            max_content_depth: 8,
            syntax: pdf_syntax::SyntaxLimits::default(),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ContentError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Syntax(#[from] pdf_syntax::SyntaxError),
    #[error("operator budget exhausted after {0} operations")]
    OperatorBudget(u64),
    #[error("XObject/pattern recursion depth {0} exceeded")]
    RecursionDepth(u32),
    #[error("operand stack overflow ({0} operands)")]
    OperandStackOverflow(usize),
    #[error("operand nesting depth {0} exceeded")]
    NestingDepth(usize),
    #[error("malformed content stream: {0}")]
    Malformed(String),
    #[error("page compilation cancelled")]
    Cancelled,
    /// The inline-image framer could not locate the image's `EI` terminator.
    /// Recoverable: the run loop drops just the image and continues. This is a
    /// distinct variant (rather than reusing [`ContentError::Malformed`]) so the
    /// interpreter can attribute the recovery precisely without threading a
    /// `ParseContext` down into the ctx-free tokenizer (see `read_inline_image`).
    #[error("inline image framing failed: {0}")]
    InlineImage(String),
    #[error(transparent)]
    Font(#[from] pdf_font::FontError),
    #[error(transparent)]
    Image(#[from] pdf_image::ImageError),
    #[error(transparent)]
    Color(#[from] pdf_color::ColorError),
}

impl ContentError {
    /// Whether this error must abort the whole page rather than being recovered
    /// past. The DoS guards and explicit cancellation are fatal — operator
    /// budget, XObject/pattern recursion depth, operand-stack overflow, operand
    /// nesting depth, and cancellation.
    /// Everything else (content malformation, unparseable tokens, and
    /// resource/decode failures) is *recoverable*: the interpreter drops the
    /// offending construct, records a recovery note, and continues, matching
    /// PDFium's content interpreter. Keeping the DoS guards on their own
    /// variants (not `Malformed`) makes this classification robust even when an
    /// error propagates out of a nested form/CharProc `run`.
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(
            self,
            ContentError::OperatorBudget(_)
                | ContentError::RecursionDepth(_)
                | ContentError::OperandStackOverflow(_)
                | ContentError::NestingDepth(_)
                | ContentError::Cancelled
        )
    }
}

/// One-pass page compilation product for tightly integrated consumers such
/// as the Lege viewer. The semantic page remains available for text,
/// selection, links, structure recovery, recoloring policy, and diagnostics;
/// the compiled page is the reusable raster source.
///
/// Keeping both artifacts from one interpretation pass is deliberate. A
/// viewer must not call `compile_semantic` and then `compile` separately,
/// because that would parse and interpret the page twice and recreate the
/// bitmap-server boundary this workspace is designed to avoid.
#[derive(Debug, Clone)]
pub struct PageCompilation {
    pub semantic: std::sync::Arc<SemanticPage>,
    pub compiled: std::sync::Arc<CompiledPage>,
}

/// Compiles pages to the semantic IR. Cheap to construct; one per job or
/// reused per worker.
#[derive(Debug, Default)]
pub struct PageCompiler {
    limits: ContentLimits,
    /// Optional system font source (fonts.md Font Phase 7).
    ///
    /// Injected, never global, and absent by default: system fonts depend on
    /// what a machine happens to have installed, so enabling them trades
    /// this engine's determinism for PDFium-level coverage. Without one, a
    /// non-embedded font resolves to a bundled standard face and two
    /// machines still agree byte for byte.
    system_fonts: Option<std::sync::Arc<dyn pdf_font::SystemFontProvider>>,
    /// Render annotation static appearance streams (`/AP /N`) after the page
    /// content, PDFium `FPDF_ANNOT` parity. Off by default; the render API's
    /// `AnnotationMode` maps onto this at integration.
    annotations: bool,
}

impl PageCompiler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: ContentLimits) -> Self {
        Self {
            limits,
            ..Default::default()
        }
    }

    /// Resolve non-embedded fonts against installed system fonts before
    /// falling back to the bundled standard 14 (fonts.md Font Phase 7).
    ///
    /// This is what lets a document naming *DengXian* or *Cambria* — or any
    /// CJK font — render as intended, and it is what PDFium does. It also
    /// makes output depend on the host's installed fonts, which is why it is
    /// opt-in.
    pub fn with_system_fonts(
        mut self,
        provider: std::sync::Arc<dyn pdf_font::SystemFontProvider>,
    ) -> Self {
        self.system_fonts = Some(provider);
        self
    }

    /// Render each annotation's normal (`/AP /N`) appearance stream after the
    /// page content — static appearances only, matching PDFium's
    /// `FPDF_ANNOT` display pass (two-pass order, `Hidden`/`NoView` skipped,
    /// `/AS` state selection, §12.5.5 rect fitting).
    pub fn with_annotations(mut self, on: bool) -> Self {
        self.annotations = on;
        self
    }

    /// Compile `page` of `snapshot` into a [`SemanticPage`] — the Phase 2
    /// product. `ctx` is the caller's worker-owned parse context; `&self`
    /// receives no document mutation whatsoever, so concurrent calls on
    /// different pages of the same snapshot are independent.
    pub fn compile_semantic(
        &self,
        snapshot: &DocumentSnapshot,
        page: PageIndex,
        ctx: &mut ParseContext,
    ) -> Result<SemanticPage, ContentError> {
        if ctx.is_cancelled() {
            return Err(ContentError::Cancelled);
        }
        let page_ref = snapshot.page(page)?.clone();
        let [x0, y0, x1, y1] = page_ref.crop_box;
        let bounds = PageBounds {
            crop: Rect { x0, y0, x1, y1 }.normalized(),
            rotate: page_ref.rotate,
        };

        let content =
            interpret::gather_content(snapshot, page_ref.contents.as_ref(), ctx, &self.limits)?;
        if ctx.is_cancelled() {
            return Err(ContentError::Cancelled);
        }
        let resources = interpret::resolve_resources(snapshot, page_ref.resources.as_ref(), ctx);

        let mut interp = interpret::Interpreter::new(
            snapshot,
            ctx,
            &self.limits,
            resources,
            self.system_fonts.clone(),
        );
        interp.run(&content)?;
        if self.annotations && !page_ref.annotations.is_empty() {
            interp.run_annotations(&page_ref.annotations)?;
        }
        Ok(interp.finish(bounds))
    }

    /// Compile once and retain both the semantic and lowered artifacts.
    ///
    /// This is the preferred entry point for document applications. Raster-
    /// only callers may continue to use [`Self::compile`].
    pub fn compile_artifacts(
        &self,
        snapshot: &DocumentSnapshot,
        page: PageIndex,
        ctx: &mut ParseContext,
    ) -> Result<PageCompilation, ContentError> {
        let semantic = std::sync::Arc::new(self.compile_semantic(snapshot, page, ctx)?);
        if ctx.is_cancelled() {
            return Err(ContentError::Cancelled);
        }
        let compiled = std::sync::Arc::new(lower::lower(semantic.as_ref()));
        if ctx.is_cancelled() {
            return Err(ContentError::Cancelled);
        }
        Ok(PageCompilation { semantic, compiled })
    }

    /// Compile `page` into the backend-neutral [`CompiledPage`] (roadmap §7
    /// Phase 3): compile the semantic page, then lower it — making implicit
    /// graphics state explicit, interning resources, and recording the clip
    /// stack. Painter order is preserved exactly.
    pub fn compile(
        &self,
        snapshot: &DocumentSnapshot,
        page: PageIndex,
        ctx: &mut ParseContext,
    ) -> Result<CompiledPage, ContentError> {
        let semantic = self.compile_semantic(snapshot, page, ctx)?;
        Ok(lower::lower(&semantic))
    }

    /// Compile a page and return coarse, allocation-free-at-call-site timing
    /// attribution for the semantic and IR-lowering phases.
    #[cfg(feature = "profiling")]
    pub fn compile_profiled(
        &self,
        snapshot: &DocumentSnapshot,
        page: PageIndex,
        ctx: &mut ParseContext,
    ) -> Result<(CompiledPage, pdf_profiling::ProfileReport), ContentError> {
        let all = std::time::Instant::now();
        let page_start = std::time::Instant::now();
        let page_ref = snapshot.page(page)?.clone();
        let [x0, y0, x1, y1] = page_ref.crop_box;
        let bounds = PageBounds {
            crop: Rect { x0, y0, x1, y1 }.normalized(),
            rotate: page_ref.rotate,
        };
        let mut profile = pdf_profiling::ProfileReport::new();
        profile.add_duration("compile.page_lookup", page_start.elapsed());

        let content_start = std::time::Instant::now();
        let content =
            interpret::gather_content(snapshot, page_ref.contents.as_ref(), ctx, &self.limits)?;
        profile.add_duration("compile.content_gather", content_start.elapsed());
        profile.increment("compile.content_bytes", content.len() as u64);

        let resources_start = std::time::Instant::now();
        let resources = interpret::resolve_resources(snapshot, page_ref.resources.as_ref(), ctx);
        profile.add_duration("compile.resources", resources_start.elapsed());

        let interpret_start = std::time::Instant::now();
        let mut interp = interpret::Interpreter::new(
            snapshot,
            ctx,
            &self.limits,
            resources,
            self.system_fonts.clone(),
        );
        interp.run(&content)?;
        if self.annotations && !page_ref.annotations.is_empty() {
            interp.run_annotations(&page_ref.annotations)?;
        }
        profile.add_duration("compile.interpret", interpret_start.elapsed());

        let finish_start = std::time::Instant::now();
        let semantic = interp.finish(bounds);
        profile.add_duration("compile.semantic_finish", finish_start.elapsed());
        let lower_start = std::time::Instant::now();
        let compiled = lower::lower(&semantic);
        profile.add_duration("compile.ir_lower", lower_start.elapsed());
        profile.add_duration("compile.total", all.elapsed());
        profile.increment("compile.operations", compiled.operations.len() as u64);
        profile.increment("compile.objects_visited", ctx.objects_visited as u64);
        profile.increment(
            "compile.decoded_stream_bytes",
            ctx.decoded_bytes_used as u64,
        );
        profile.increment("compile.recovery_events", ctx.recovery.len() as u64);
        profile.increment("compile.object_cache_hits", ctx.object_cache_hits as u64);
        profile.increment(
            "compile.object_cache_misses",
            ctx.object_cache_misses as u64,
        );
        profile.increment(
            "compile.object_stream_cache_hits",
            ctx.object_stream_cache_hits as u64,
        );
        profile.increment(
            "compile.object_stream_inflates",
            ctx.object_stream_inflates as u64,
        );
        Ok((compiled, profile))
    }

    /// Maps a path-painting operator's even-odd flag to the IR fill rule.
    pub fn map_fill_rule(even_odd: bool) -> FillRule {
        if even_odd {
            FillRule::EvenOdd
        } else {
            FillRule::NonZero
        }
    }
}

const fn _assert_send<T: Send>() {}
const _: () = _assert_send::<PageCompiler>();
