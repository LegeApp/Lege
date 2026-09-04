//! Serde views over `lege-pdf-print` (the print crate stays serde-free).
//!
//! Every length here is in PostScript points, which is the unit the print
//! crate works in end to end; records carry `unit` so a consumer never has to
//! guess.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct PrinterView {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub is_default: bool,
    pub accepting_jobs: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrintersData {
    /// Which spooler backend answered: `cups`, `windows`, or `file`.
    pub backend: &'static str,
    pub printers: Vec<PrinterView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RectView {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct MarginsView {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

/// A PDF-style affine transform, `[a b c d e f]`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TransformView {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

/// One source page placed on one sheet.
///
/// `transform` is authoritative; `scale_x`, `scale_y`, `rotation_degrees` and
/// `translate` are read off it so a consumer can check the plan without
/// decomposing a matrix.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PlacementView {
    /// One-based source page number.
    pub source_page: u32,
    /// Zero-based source page index.
    pub source_page_index: u32,
    pub scale_x: f64,
    pub scale_y: f64,
    pub rotation_degrees: f64,
    pub translate: [f64; 2],
    pub transform: TransformView,
    /// The cell on the sheet this placement may paint into.
    pub cell: RectView,
    /// Where the whole source page lands, before clipping. Under a cropping
    /// scaling this extends past `cell`.
    pub content: RectView,
    /// What actually reaches the paper: `content` clipped to `cell`.
    pub painted: RectView,
}

/// The bitmap one composed side would occupy at the plan's DPI.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RasterSizeView {
    pub width: u32,
    pub height: u32,
    /// 1 for grayscale, 3 for RGB.
    pub channels: u8,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SheetView {
    pub index: u32,
    /// `front` or `back`.
    pub side: &'static str,
    /// The sheet rectangle, already oriented, origin at (0, 0).
    pub bounds: RectView,
    /// Paper minus hardware and user margins.
    pub imageable: RectView,
    pub landscape: bool,
    /// The raster this side would compose to. Absent when the size exceeds
    /// the compose budget, which composition would reject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raster: Option<RasterSizeView>,
    pub placements: Vec<PlacementView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// `queried` when a queue answered, `assumed` when the plan used a
    /// conservative stand-in instead of contacting one.
    pub source: &'static str,
    pub accepts_pdf: bool,
    pub supports_duplex: bool,
    pub supports_color: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_dpi: Option<f64>,
    pub hardware_margins: MarginsView,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaperView {
    /// A standard name (`a4`, `letter`, …) or `custom`.
    pub name: &'static str,
    pub width_pt: f64,
    pub height_pt: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipp_name: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrintOptionsView {
    pub paper: PaperView,
    pub orientation: &'static str,
    pub margins: MarginsView,
    /// `actual`, `fit`, `shrink`, `fill`, or a percentage such as `50%`.
    pub scaling: String,
    pub n_up: &'static str,
    pub n_up_order: &'static str,
    pub n_up_border: bool,
    pub duplex: &'static str,
    pub copies: u16,
    pub collate: bool,
    pub reverse: bool,
    pub source_box: &'static str,
    pub grayscale: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ComposeView {
    pub dpi: f64,
    pub grayscale: bool,
    pub band_rows: u32,
    pub max_pixels: u64,
}

/// What `--dry-run` reports: the whole decision, nothing spooled.
#[derive(Debug, Clone, Serialize)]
pub struct PrintPlanData {
    pub unit: &'static str,
    /// `pass_through` or `composed`.
    pub route: &'static str,
    pub dry_run: bool,
    pub page_count: u32,
    /// One-based pages the range selects, before copies and reversal.
    pub selected_pages: Vec<u32>,
    pub options: PrintOptionsView,
    pub device: DeviceView,
    /// Composition settings. Reported on both routes; only the composed route
    /// uses them.
    pub compose: ComposeView,
    /// Printed *sides* imposed for one copy — the length of `sheets`. `null`
    /// on the pass-through route, where this is the printer's business.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_count: Option<u32>,
    /// Physical sheets of paper for one copy: duplex puts two sides on one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_sheets: Option<u32>,
    /// `sheet_count * copies`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_sides: Option<u32>,
    /// Who multiplies the run for `copies`. Always `spooler`: imposition
    /// emits one copy, and both platform spoolers take a native copy count.
    pub copies_applied_by: &'static str,
    /// One entry per printed side, for one copy.
    pub sheets: Vec<SheetView>,
}

/// What a real submission reports.
#[derive(Debug, Clone, Serialize)]
pub struct SubmittedJobData {
    pub unit: &'static str,
    pub route: &'static str,
    pub dry_run: bool,
    pub job_id: String,
    pub printer: String,
    pub backend: &'static str,
    /// Printed sides composed for one copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_count: Option<u32>,
    /// Who multiplies the run for `copies`; see `PrintPlanData`.
    pub copies_applied_by: &'static str,
    /// Directory the `file` backend wrote to, when that backend was used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spooled_to: Option<String>,
    pub page_count: u32,
    pub selected_pages: Vec<u32>,
    pub options: PrintOptionsView,
}
