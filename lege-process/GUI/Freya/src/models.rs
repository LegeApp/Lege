// GUI models — all types defined locally; no lege dependency.

use std::path::PathBuf;

// ── Processing option enums ───────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputFormat {
    #[default]
    Pdf,
    Djvu,
    Epub,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OcrMode {
    #[default]
    #[serde(alias = "Low")]
    Fast,
    #[serde(alias = "High")]
    Thorough,
}

/// How binarized text is written into a PDF. One choice, not a combination:
/// the page's text is either a per-book font or a raster codec.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompressionType {
    /// Truetyping: the printed text becomes an embedded per-book TrueType
    /// font and the page carries real text objects. Opt-in: on a long book
    /// it is several times slower than JBIG2 and about twice the size, since
    /// every 40-odd pages of glyph variants fill a font's id space and start
    /// another font.
    Truetyping,
    /// JBIG2 symbol-substitution raster text. The default: the smallest and
    /// fastest text encoding, and every reader understands it.
    #[default]
    Jbig2,
    /// CCITT Group 4 raster text; reached through compatibility mode.
    Ccitt4,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CoverImageType {
    #[default]
    Jpeg,
    Jpeg2000,
    None,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImageProcessingType {
    #[default]
    Original,
    Dithered,
}

impl std::fmt::Display for ImageProcessingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dithered => write!(f, "Dithered"),
            Self::Original => write!(f, "Original"),
        }
    }
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pdf => write!(f, "PDF"),
            Self::Djvu => write!(f, "DjVu"),
            Self::Epub => write!(f, "EPUB"),
        }
    }
}

impl std::fmt::Display for OcrMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fast => write!(f, "Fast"),
            Self::Thorough => write!(f, "Thorough"),
        }
    }
}

impl std::fmt::Display for CompressionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truetyping => write!(f, "Truetyping"),
            Self::Ccitt4 => write!(f, "CCITT4"),
            Self::Jbig2 => write!(f, "JBIG2"),
        }
    }
}

// ── ProcessingOptions ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ProcessingOptions {
    pub input_path: Option<PathBuf>,
    pub output_path: Option<PathBuf>,
    /// True when `output_path` was auto-derived from an input rather than
    /// picked by the user. Auto output follows each file's own input folder
    /// at job time (mixed-folder queues land next to their sources); an
    /// explicitly chosen folder applies to every file.
    pub output_path_is_auto: bool,

    pub output_format: OutputFormat,
    pub compression_type: CompressionType,
    pub cover_image_type: CoverImageType,
    pub image_processing_type: ImageProcessingType,
    pub ccitt4_dithered_images: bool,
    pub original_cover: bool,

    pub target_height: Option<u32>,
    pub target_width: Option<u32>,
    pub target_device: Option<String>,
    pub page_range: Option<String>,
    pub no_front_cover: bool,
    pub png_folder_mode: bool,
    pub layout_analysis: bool,
    pub layout_exclusion_pages: Option<String>,
    pub use_ocr: bool,
    pub ocr_mode: OcrMode,
    pub make_epub_also: bool,
    pub use_jbig2_halftone: bool,
    pub high_quality_output: bool,
    pub jpeg_compat: bool,
    /// Turn JBIG2 symbol substitution on, in its unifying form. Off is the
    /// bit-exact generic region. Only meaningful while JBIG2 is the text
    /// encoder, i.e. while compatibility mode is off.
    pub symbol_mode: bool,
    pub invert_input: bool,

    pub center_margins: bool,
    pub crop_margins: bool,
    pub crop_footnotes: bool,
    pub crop_free_aspect: bool,
    pub reflow: bool,

    pub use_heavy_binarization: bool,
    pub k_factor: f32,
    /// When set, adaptive binarization uses the user-supplied `k_factor` and the
    /// K-factor input is revealed; otherwise the default `k_factor` is used and hidden.
    pub use_custom_adaptive: bool,
    pub use_fixed_threshold: bool,
    pub threshold_value: u8,
    /// Grayscale/MRC rendering: text becomes a JBIG2/JB2 ink mask over a
    /// JP2/IW44 grayscale background (keeps antialiasing). Mutually exclusive
    /// with the bilevel binarization options above; image regions stay JP2/IW44.
    pub grayscale_mode: bool,
}

impl ProcessingOptions {
    pub fn new() -> Self {
        Self {
            output_format: OutputFormat::Pdf,
            // JBIG2 default; truetyping and CCITT4 (compatibility mode) are
            // the two opt-outs. Truetyping was the default for two days in
            // September 2026 and went back: on an 886-page scan it took 3.6x
            // the time and made 2.1x the file of JBIG2.
            compression_type: CompressionType::Jbig2,
            // Symbol substitution compresses scanned text far better, but a
            // wrong match rewrites a word, so it is the opt-in and the page
            // is bit-exact until it is asked for.
            symbol_mode: false,
            cover_image_type: CoverImageType::Jpeg,
            image_processing_type: ImageProcessingType::Original,
            original_cover: true,
            target_height: Some(1200),
            layout_analysis: true,
            k_factor: lege_ipc::DEFAULT_K_FACTOR,
            // Adaptive is the effective processing default, so represent that
            // default explicitly in the mutually-exclusive GUI controls.
            use_custom_adaptive: true,
            threshold_value: 180,
            ..Default::default()
        }
    }

    /// The `--text-format` this configuration asks the worker for. For PDF it
    /// is the text encoder alone: image regions are chosen separately, by
    /// `image_processing_type`.
    pub fn effective_text_format(&self) -> &'static str {
        match self.output_format {
            OutputFormat::Djvu => "djvu",
            OutputFormat::Epub => "epub",
            // Raster reflow re-renders every page as an image and never asks
            // the text encoder anything; truetyping would only raise the
            // render height for glyph tracing that never happens.
            OutputFormat::Pdf if self.reflow => "ccitt4",
            OutputFormat::Pdf => match self.compression_type {
                CompressionType::Truetyping => "truetyping",
                CompressionType::Jbig2 => "jbig2",
                CompressionType::Ccitt4 => "ccitt4",
            },
        }
    }

    /// Choose the PDF text encoder. Compatibility mode and truetyping are the
    /// two opt-outs from JBIG2 and exclude each other; clearing either one
    /// returns to JBIG2.
    pub fn set_text_encoder(&mut self, encoder: CompressionType) {
        self.jpeg_compat = matches!(encoder, CompressionType::Ccitt4);
        if !matches!(encoder, CompressionType::Jbig2) {
            self.symbol_mode = false;
        }
        if matches!(encoder, CompressionType::Ccitt4) {
            self.use_jbig2_halftone = false;
        }
        // Truetyping draws its text as outlines over a background, so what the
        // ink mask needs is to be clean, not to be a page. The MRC clean path
        // despeckles it; Sauvola on a raw scan keeps the paper grain and turns
        // every speck into a glyph (41 pages of a book scan: 0.77 MB against
        // 2.56 MB). Grayscale therefore comes on with truetyping, and stays
        // selectable on its own.
        if matches!(encoder, CompressionType::Truetyping) {
            self.grayscale_mode = true;
        }
        self.compression_type = encoder;
    }

    /// Select original or conventionally dithered image regions. JBIG2
    /// halftone is represented by the same durable fields for compatibility
    /// with existing saved settings, but behaves as a third exclusive choice
    /// in the GUI.
    pub fn set_image_processing_type(&mut self, image_type: ImageProcessingType) {
        self.image_processing_type = image_type;
        self.use_jbig2_halftone = false;
    }

    /// Select JBIG2 halftone encoding for detected image regions. This is
    /// independent of whether the page's text uses JBIG2 or truetyping.
    pub fn set_jbig2_halftone_images(&mut self) {
        self.image_processing_type = ImageProcessingType::Dithered;
        self.use_jbig2_halftone = true;
    }

    /// JBIG2 halftone image regions need a JBIG2-capable page: the effective
    /// text encoder, not the stored choice, decides. Raster reflow forces
    /// `ccitt4`, and the CLI rejects `--text-format ccitt4 --halftone`.
    pub fn can_select_jbig2_halftone_images(&self) -> bool {
        matches!(self.effective_text_format(), "truetyping" | "jbig2")
    }

    pub fn uses_jbig2_halftone_images(&self) -> bool {
        self.can_select_jbig2_halftone_images()
            && matches!(self.image_processing_type, ImageProcessingType::Dithered)
            && self.use_jbig2_halftone
    }

    /// Keep GUI state aligned with the core pipeline's effective feature
    /// rules. Reflow is a layout mode; inversion disables the layout model.
    pub fn set_layout_analysis(&mut self, enabled: bool) {
        self.layout_analysis = enabled;
        if enabled {
            self.invert_input = false;
        } else {
            self.reflow = false;
        }
    }

    pub fn set_use_ocr(&mut self, enabled: bool) {
        self.use_ocr = enabled;
        if !enabled {
            self.make_epub_also = false;
        }
    }

    pub fn set_invert_input(&mut self, enabled: bool) {
        self.invert_input = enabled;
        if enabled {
            self.layout_analysis = false;
            self.reflow = false;
        }
    }

    pub fn set_reflow(&mut self, enabled: bool) {
        self.reflow = enabled;
        if enabled {
            // Reflow's ccitt4 pages cannot carry JBIG2 halftone regions;
            // the image choice falls back to conventional dithering.
            self.use_jbig2_halftone = false;
            self.layout_analysis = true;
            self.invert_input = false;
            self.center_margins = false;
            self.crop_margins = false;
            self.crop_footnotes = false;
            self.crop_free_aspect = false;
        }
    }

    /// Repair stale or hand-edited settings before they become worker
    /// arguments. An explicit reflow selection wins over hidden incompatible
    /// state because it is the most constrained mode.
    pub fn normalize_processing_dependencies(&mut self) {
        if self.reflow {
            self.set_reflow(true);
        } else if self.invert_input {
            self.set_invert_input(true);
        } else if !self.layout_analysis {
            self.set_layout_analysis(false);
        }
    }
}

#[cfg(test)]
mod reflow_tests {
    use super::ProcessingOptions;

    #[test]
    fn reflow_enables_layout_and_clears_incompatible_geometry_state() {
        let mut options = ProcessingOptions::new();
        options.layout_analysis = false;
        options.invert_input = true;
        options.center_margins = true;
        options.crop_margins = true;
        options.crop_footnotes = true;
        options.crop_free_aspect = true;

        options.set_reflow(true);

        assert!(options.reflow);
        assert!(options.layout_analysis);
        assert!(!options.invert_input);
        assert!(!options.center_margins);
        assert!(!options.crop_margins);
        assert!(!options.crop_footnotes);
        assert!(!options.crop_free_aspect);
    }

    #[test]
    fn disabling_layout_or_enabling_inversion_disables_reflow() {
        let mut options = ProcessingOptions::new();
        options.set_reflow(true);
        options.set_layout_analysis(false);
        assert!(!options.reflow);

        options.set_reflow(true);
        options.set_invert_input(true);
        assert!(!options.reflow);
        assert!(!options.layout_analysis);
    }

    #[test]
    fn reflow_and_halftone_never_produce_a_rejected_job() {
        // Halftone first, then reflow.
        let mut options = ProcessingOptions::new();
        options.set_jbig2_halftone_images();
        options.set_reflow(true);
        options.normalize_processing_dependencies();
        assert_eq!(options.effective_text_format(), "ccitt4");
        assert!(!options.uses_jbig2_halftone_images());
        assert!(!options.can_select_jbig2_halftone_images());

        // Reflow first, then halftone.
        let mut options = ProcessingOptions::new();
        options.set_reflow(true);
        options.set_jbig2_halftone_images();
        options.normalize_processing_dependencies();
        assert_eq!(options.effective_text_format(), "ccitt4");
        assert!(!options.uses_jbig2_halftone_images());

        // And it comes back once reflow is off.
        options.set_reflow(false);
        options.set_jbig2_halftone_images();
        assert!(options.can_select_jbig2_halftone_images());
        assert!(options.uses_jbig2_halftone_images());
    }

    #[test]
    fn normalization_repairs_stale_reflow_settings() {
        let mut options = ProcessingOptions::new();
        options.reflow = true;
        options.layout_analysis = false;
        options.invert_input = true;
        options.crop_margins = true;

        options.normalize_processing_dependencies();

        assert!(options.reflow);
        assert!(options.layout_analysis);
        assert!(!options.invert_input);
        assert!(!options.crop_margins);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolutionPreset {
    pub height: u32,
    pub width: Option<u32>,
}

impl ResolutionPreset {
    pub fn from_options(options: &ProcessingOptions) -> Self {
        Self {
            height: options.target_height.unwrap_or(1200),
            width: options.target_width,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum InputKind {
    Pdf,
    ImageFolder,
    ZipArchive,
    #[default]
    Unknown,
}

impl InputKind {
    pub fn detect(path: &PathBuf) -> Self {
        if path.is_dir() {
            return InputKind::ImageFolder;
        }
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("pdf") => InputKind::Pdf,
            Some("zip") => InputKind::ZipArchive,
            _ => InputKind::Unknown,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocumentItem {
    pub id: String,
    pub file_path: PathBuf,
    pub file_name: String,
    pub input_kind: InputKind,
    pub page_count: Option<u32>,
    pub status: DocumentStatus,
    pub output_path: Option<PathBuf>,
    pub progress: f32,
    pub error_message: Option<String>,
}

impl DocumentItem {
    pub fn new(file_path: PathBuf) -> Self {
        let file_name = file_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let input_kind = InputKind::detect(&file_path);

        Self {
            id: unique_id(),
            file_path,
            file_name,
            input_kind,
            page_count: None,
            status: DocumentStatus::Queued,
            output_path: None,
            progress: 0.0,
            error_message: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum DocumentStatus {
    #[default]
    Queued,
}

impl std::fmt::Display for DocumentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocumentStatus::Queued => write!(f, "Queued"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_text_format_is_jbig2_until_an_encoder_is_chosen() {
        let mut options = ProcessingOptions::new();
        assert_eq!(options.effective_text_format(), "jbig2");
        assert_eq!(CompressionType::default(), CompressionType::Jbig2);

        options.set_text_encoder(CompressionType::Truetyping);
        assert_eq!(options.effective_text_format(), "truetyping");

        options.set_text_encoder(CompressionType::Ccitt4);
        assert_eq!(options.effective_text_format(), "ccitt4");

        options.set_text_encoder(CompressionType::Jbig2);
        assert_eq!(options.effective_text_format(), "jbig2");
    }

    #[test]
    fn image_handling_no_longer_decides_the_text_encoder() {
        let mut options = ProcessingOptions::new();
        options.image_processing_type = ImageProcessingType::Dithered;
        assert_eq!(options.effective_text_format(), "jbig2");

        options.layout_analysis = false;
        assert_eq!(options.effective_text_format(), "jbig2");
    }

    #[test]
    fn the_two_opt_outs_exclude_each_other() {
        let mut options = ProcessingOptions::new();

        options.set_text_encoder(CompressionType::Ccitt4);
        assert!(options.jpeg_compat);

        options.set_text_encoder(CompressionType::Truetyping);
        assert!(!options.jpeg_compat);

        options.set_text_encoder(CompressionType::Jbig2);
        assert!(!options.jpeg_compat);

        options.symbol_mode = true;
        options.set_text_encoder(CompressionType::Truetyping);
        assert!(
            !options.symbol_mode,
            "a JBIG2-only sub-option cannot outlive JBIG2"
        );
    }

    #[test]
    fn symbol_substitution_is_off_until_it_is_asked_for() {
        let options = ProcessingOptions::new();
        assert!(!options.symbol_mode);
    }

    #[test]
    fn truetyping_turns_grayscale_on_and_leaves_it_to_the_user() {
        let mut options = ProcessingOptions::new();
        assert!(!options.grayscale_mode);

        options.set_text_encoder(CompressionType::Truetyping);
        assert!(options.grayscale_mode);

        // The pair of buttons still decides: binarized truetyping stays
        // reachable, and going back to JBIG2 does not undo the choice.
        options.grayscale_mode = false;
        options.set_text_encoder(CompressionType::Jbig2);
        assert!(!options.grayscale_mode);
    }

    #[test]
    fn halftone_images_survive_truetyping_and_jbig2_text_selection() {
        let mut options = ProcessingOptions::new();
        options.set_jbig2_halftone_images();
        assert!(options.uses_jbig2_halftone_images());

        options.set_text_encoder(CompressionType::Jbig2);
        assert!(options.uses_jbig2_halftone_images());

        options.set_text_encoder(CompressionType::Truetyping);
        assert!(options.uses_jbig2_halftone_images());

        options.set_text_encoder(CompressionType::Ccitt4);
        assert!(!options.uses_jbig2_halftone_images());
        assert!(!options.use_jbig2_halftone);
    }
}
/// Process-unique identifier for a queue entry.
///
/// Replaces `uuid::Uuid::new_v4()`. These ids are local queue keys — never
/// parsed back, never persisted, never sent anywhere that requires RFC-4122
/// shape — so a nanosecond timestamp plus a monotonic counter is sufficient:
/// unique within a run by the counter, and across runs by the clock.
pub fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}{:08x}", nanos, SEQ.fetch_add(1, Ordering::Relaxed))
}
