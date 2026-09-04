//! Pure imposition maths: geometry in, geometry out.
//!
//! No I/O, no OS, no renderer. This is where every bug that produces a
//! wrong-looking printout lives, and it is the only part that can be
//! exhaustively tested without a printer.

use crate::paper::Rect;
use crate::{Matrix, PrintError, PrintJob, SourcePage};

/// Which side of a sheet a placement is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Front,
    Back,
}

/// One source page placed on one sheet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Zero-based index of the source page.
    pub source_page: u32,
    /// Source page points -> sheet points.
    pub transform: Matrix,
    /// The cell on the sheet this placement may paint into, in sheet points.
    pub clip: Rect,
}

/// One physical side of one sheet of paper.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    /// Zero-based position in the emitted sequence.
    pub index: u32,
    /// The sheet rectangle, origin at (0, 0), already oriented.
    pub bounds: Rect,
    /// Paper minus hardware and user margins.
    pub imageable: Rect,
    pub side: Side,
    pub placements: Vec<Placement>,
}

/// Impose `job` onto sheets for a device with `hardware_margins`.
pub fn impose(
    job: &PrintJob,
    hardware_margins: crate::paper::Margins,
) -> Result<Vec<Sheet>, PrintError> {
    let _ = (job, hardware_margins);
    unimplemented!("phase 1")
}

/// Fit `page` into `cell` under `scaling`, honouring `orientation`.
pub fn fit_page(
    page: SourcePage,
    cell: Rect,
    scaling: crate::Scaling,
    orientation: crate::paper::Orientation,
) -> Placement {
    let _ = (page, cell, scaling, orientation);
    unimplemented!("phase 1")
}

/// Booklet page order: the source-page slots for sheet `sheet` of a booklet
/// with `sheets * 4` slots, as `[front_left, front_right, back_left,
/// back_right]`, `None` meaning a blank.
#[must_use]
pub fn booklet_slots(sheet: u32, total_slots: u32) -> [Option<u32>; 4] {
    let _ = (sheet, total_slots);
    unimplemented!("phase 1")
}
