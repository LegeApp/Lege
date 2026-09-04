//! Pure imposition maths: geometry in, geometry out.
//!
//! No I/O, no OS, no renderer. This is where every bug that produces a
//! wrong-looking printout lives, and it is the only part that can be
//! exhaustively tested without a printer.
//!
//! # The frames
//!
//! * *Source points* — the coordinate system of one source page: origin at
//!   its bottom-left corner, `y` up, and already sized as
//!   [`SourcePage::width`]/[`SourcePage::height`], i.e. after the page's
//!   `/Rotate` has been applied.
//! * *Sheet points* — one physical side of one sheet: origin at its
//!   bottom-left corner, `y` up, extent [`Sheet::bounds`].
//!
//! [`Placement::transform`] maps the first onto the second, and
//! [`Placement::clip`] is the cell it may paint into, already intersected
//! with the sheet's imageable area.
//!
//! # Decisions worth knowing about
//!
//! **Copies and collation are not applied here.** [`impose`] emits the sheet
//! sequence for exactly *one* copy. Copies are a spooler concern — every
//! target spooler takes a copy count natively, a print preview wants one copy
//! rather than five identical ones, and [`PrintOptions::is_pass_through_capable`]
//! already treats copies and collation as options that do not force
//! composition. A caller that must expand them itself (a `file` backend
//! writing one PNG per side, say) calls [`expand_copies`].
//!
//! **Margins are read in the oriented sheet's frame.** Both the user margins
//! and the hardware margins passed to [`impose`] name the left/right/top/
//! bottom edges of the sheet *as it prints*. A driver that reports its
//! unprintable border against portrait media should rotate it with
//! [`Margins::rotated_quarter_turns`] before handing it over. The two combine
//! with [`Margins::max_with`]: a user margin narrower than the hardware
//! border is silently widened to it, because the alternative is content that
//! vanishes on paper.
//!
//! **A quarter turn is applied once, at the level that can absorb it.**
//! Turning the sheet and turning the page are the same printout, so doing
//! both is either a no-op or a double negative. [`Orientation::Auto`]
//! therefore turns the *sheet* — once per physical sheet, scored against
//! every page that lands on it — and leaves the pages upright in their cells,
//! which is what makes `Auto` mean anything at all. A pinned
//! [`Orientation::Portrait`] or [`Orientation::Landscape`] fixes the sheet,
//! so the pages take the quarter turn instead when it fits more of them on
//! the paper — the usual "auto-rotate and centre". [`fit_page`] exposes both
//! behaviours directly to callers that want to choose.
//!
//! **Booklets ignore the N-up grid and order.** A saddle-stitched sheet is
//! folded down its vertical centre line, so it always carries exactly two
//! cells side by side and always mirrors its back side horizontally,
//! whatever [`PrintOptions::duplex`] and [`PrintOptions::n_up_order`] say.
//! The field names of [`booklet_slots`] — left and right — are that
//! constraint written down.

use core::cmp::Ordering;

use crate::paper::{Margins, Orientation, Rect};
use crate::{
    Duplex, Matrix, NUp, NUpOrder, PrintError, PrintJob, PrintOptions, Scaling, SourcePage,
};

/// Lengths closer than this are the same length. Imposition works in points,
/// where a printer's addressable dot is ~0.06pt, so this is pure float slop.
const EPSILON: f64 = 1e-9;

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

impl Placement {
    /// The source page's bounding box after [`Placement::transform`], in
    /// sheet points. Content outside [`Placement::clip`] is not printed, so
    /// this may extend past the cell under the cropping scalings.
    #[must_use]
    pub fn transformed_bounds(&self, page: SourcePage) -> Rect {
        let corners = [
            self.transform.apply(0.0, 0.0),
            self.transform.apply(page.width, 0.0),
            self.transform.apply(0.0, page.height),
            self.transform.apply(page.width, page.height),
        ];
        let mut bounds = Rect::new(f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (x, y) in corners {
            bounds.x0 = bounds.x0.min(x);
            bounds.y0 = bounds.y0.min(y);
            bounds.x1 = bounds.x1.max(x);
            bounds.y1 = bounds.y1.max(y);
        }
        bounds
    }

    /// What actually reaches the paper: the transformed page clipped to its
    /// cell.
    #[must_use]
    pub fn painted_bounds(&self, page: SourcePage) -> Rect {
        self.transformed_bounds(page).intersect(self.clip)
    }
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

impl Sheet {
    /// Whether the sheet is wider than it is tall.
    #[must_use]
    pub fn is_landscape(&self) -> bool {
        self.bounds.width() > self.bounds.height()
    }
}

/// Impose `job` onto sheets for a device with `hardware_margins`.
///
/// Emits one [`Sheet`] per printed *side*, in the order they leave the
/// printer: for a duplex or booklet job the sides alternate
/// [`Side::Front`], [`Side::Back`], and the run is padded with a blank back
/// side so a whole number of physical sheets is used.
///
/// Copies and collation are deliberately not expanded here; see the module
/// documentation and [`expand_copies`].
///
/// # Errors
///
/// [`PrintError::InvalidOptions`] if the options are not printable,
/// [`PrintError::EmptyRange`] if the page range selects nothing, and
/// [`PrintError::EmptyImageableArea`] if the paper is smaller than its
/// margins.
pub fn impose(job: &PrintJob, hardware_margins: Margins) -> Result<Vec<Sheet>, PrintError> {
    let options = &job.options;
    options.validate()?;
    hardware_margins.validate()?;

    let page_count = u32::try_from(job.pages.len()).unwrap_or(u32::MAX);
    let mut order = options.range.resolve(page_count);
    if options.reverse {
        order.reverse();
    }
    let pages: Vec<SourcePage> = order
        .into_iter()
        .filter_map(|index| job.pages.get(usize::try_from(index).ok()?).copied())
        .collect();
    if pages.is_empty() {
        return Err(PrintError::EmptyRange);
    }

    let booklet = options.n_up == NUp::Booklet;
    let sides = if booklet {
        booklet_sides(&pages)
    } else {
        grid_sides(&pages, options)
    };
    build_sheets(&sides, booklet || options.duplex.is_duplex(), options, hardware_margins)
}

/// Repeat an imposed sheet sequence for `copies` copies.
///
/// Collated output repeats the whole run — 1,2,3, 1,2,3. Uncollated output
/// repeats each *physical* sheet in place — 1,1, 2,2, 3,3 — where a physical
/// sheet is a [`Side::Front`] together with the [`Side::Back`] that follows
/// it, so duplex pairs are never split. Every sheet is re-indexed.
#[must_use]
pub fn expand_copies(sheets: &[Sheet], copies: u16, collate: bool) -> Vec<Sheet> {
    let copies = usize::from(copies.max(1));
    let mut out: Vec<Sheet> = Vec::with_capacity(sheets.len().saturating_mul(copies));
    if collate {
        for _ in 0..copies {
            out.extend(sheets.iter().cloned());
        }
    } else {
        for group in physical_sheets(sheets) {
            for _ in 0..copies {
                out.extend(group.iter().cloned());
            }
        }
    }
    for (position, sheet) in out.iter_mut().enumerate() {
        sheet.index = u32::try_from(position).unwrap_or(u32::MAX);
    }
    out
}

/// Split a side sequence into physical sheets: each front, plus any backs
/// that follow before the next front.
fn physical_sheets(sheets: &[Sheet]) -> Vec<&[Sheet]> {
    let mut groups = Vec::new();
    let mut start = 0usize;
    while start < sheets.len() {
        let mut end = start + 1;
        while sheets.get(end).is_some_and(|s| s.side == Side::Back) {
            end += 1;
        }
        if let Some(group) = sheets.get(start..end) {
            groups.push(group);
        }
        start = end;
    }
    groups
}

/// Fit `page` into `cell` under `scaling`, honouring `orientation`.
///
/// `orientation` is the rotation of the *page inside the cell*, not the sheet
/// rotation: [`Orientation::Portrait`] places the page upright,
/// [`Orientation::Landscape`] turns it a quarter turn counter-clockwise, and
/// [`Orientation::Auto`] takes whichever of the two puts more of the page on
/// the paper. The page is centred in the cell either way, and
/// [`Placement::clip`] is set to the cell so the cropping scalings
/// ([`Scaling::ActualSize`], [`Scaling::FillPage`], [`Scaling::Percent`])
/// overflow into a clip rather than onto a neighbour.
///
/// [`Scaling::Percent`] is a *factor*, `1.0` meaning 1:1 —
/// [`PrintOptions::validate`] accepts `0 < p <= 100`, so `2.0` is 200%.
#[must_use]
pub fn fit_page(
    page: SourcePage,
    cell: Rect,
    scaling: Scaling,
    orientation: Orientation,
) -> Placement {
    fit(page, cell, scaling, orientation).placement
}

/// Booklet page order: the source-page slots for sheet `sheet` of a booklet
/// with `sheets * 4` slots, as `[front_left, front_right, back_left,
/// back_right]`, `None` meaning a blank.
///
/// `total_slots` is the number of pages actually being printed; the booklet
/// itself is `total_slots` rounded up to a multiple of four, and the padding
/// comes back as `None`. Sheet *n* of an *N*-slot booklet carries the
/// one-based reading positions `(N-2n, 2n+1)` on its front and
/// `(2n+2, N-2n-1)` on its back; the returned values are those positions
/// converted to **zero-based** indices into the reading order, which is the
/// same convention as [`PageRange::resolve`](crate::PageRange::resolve).
///
/// Folding the emitted stack in half reproduces the reading order, which is
/// the property the round-trip test asserts.
#[must_use]
pub fn booklet_slots(sheet: u32, total_slots: u32) -> [Option<u32>; 4] {
    let padded = total_slots.div_ceil(4).saturating_mul(4);
    let taken = sheet.saturating_mul(2);
    // One-based reading positions; 0 and anything past the real page count is
    // a blank leaf of the padding.
    let pick = |position: u32| -> Option<u32> {
        (position >= 1 && position <= total_slots).then(|| position - 1)
    };
    [
        pick(padded.saturating_sub(taken)),
        pick(taken.saturating_add(1)),
        pick(taken.saturating_add(2)),
        pick(padded.saturating_sub(taken.saturating_add(1))),
    ]
}

// ---------------------------------------------------------------------------
// Side planning
// ---------------------------------------------------------------------------

/// The pages that land on one side, in cell order. `None` is an empty cell.
type SidePlan = Vec<Option<SourcePage>>;

/// Chunk the selected pages into N-up sides, padding the last side and, for
/// duplex, padding the run to a whole number of physical sheets.
fn grid_sides(pages: &[SourcePage], options: &PrintOptions) -> Vec<SidePlan> {
    let per_side = usize::try_from(options.n_up.per_side()).unwrap_or(1).max(1);
    let mut sides: Vec<SidePlan> = pages
        .chunks(per_side)
        .map(|chunk| {
            let mut slots: SidePlan = chunk.iter().copied().map(Some).collect();
            slots.resize(per_side, None);
            slots
        })
        .collect();
    if options.duplex.is_duplex() && sides.len() % 2 == 1 {
        sides.push(vec![None; per_side]);
    }
    sides
}

/// Saddle-stitch order: two slots a side, front and back of every sheet.
fn booklet_sides(pages: &[SourcePage]) -> Vec<SidePlan> {
    let total = u32::try_from(pages.len()).unwrap_or(u32::MAX);
    let sheets = total.div_ceil(4);
    let mut sides = Vec::new();
    for sheet in 0..sheets {
        let slots = booklet_slots(sheet, total);
        let at = |slot: Option<u32>| -> Option<SourcePage> {
            let position = usize::try_from(slot?).ok()?;
            pages.get(position).copied()
        };
        sides.push(vec![at(slots[0]), at(slots[1])]);
        sides.push(vec![at(slots[2]), at(slots[3])]);
    }
    sides
}

// ---------------------------------------------------------------------------
// Sheet construction
// ---------------------------------------------------------------------------

fn build_sheets(
    sides: &[SidePlan],
    two_sided: bool,
    options: &PrintOptions,
    hardware: Margins,
) -> Result<Vec<Sheet>, PrintError> {
    let booklet = options.n_up == NUp::Booklet;
    let per_physical_sheet = if two_sided { 2 } else { 1 };
    let user = options.margins.max_with(hardware);
    let page_rotation = page_rotation(options.orientation);
    let order = if booklet {
        NUpOrder::RightThenDown
    } else {
        options.n_up_order
    };

    let mut out: Vec<Sheet> = Vec::with_capacity(sides.len());
    let mut index: u32 = 0;
    for chunk in sides.chunks(per_physical_sheet) {
        // One rotation for the whole physical sheet: the front and the back
        // are the same piece of paper.
        let on_sheet: Vec<SourcePage> = chunk.iter().flatten().flatten().copied().collect();
        let orientation = choose_orientation(&on_sheet, options, hardware)?;
        let bounds = options.paper.rect(orientation);
        let landscape = orientation == Orientation::Landscape;
        let (columns, rows) = cell_grid(options.n_up, landscape);

        for (position, slots) in chunk.iter().enumerate() {
            let side = if position == 0 { Side::Front } else { Side::Back };
            let margins = if side == Side::Back {
                back_margins(user, options.duplex, booklet, landscape)
            } else {
                user
            };
            let imageable = inset(bounds, margins);
            if imageable.is_empty() {
                return Err(PrintError::EmptyImageableArea {
                    paper_w: bounds.width(),
                    paper_h: bounds.height(),
                });
            }
            let cells = cell_rects(imageable, columns, rows, order);
            let mut placements = Vec::new();
            for (slot, cell) in slots.iter().zip(cells) {
                if let Some(page) = *slot {
                    let mut placement = fit_page(page, cell, options.scaling, page_rotation);
                    placement.clip = placement.clip.intersect(imageable);
                    placements.push(placement);
                }
            }
            out.push(Sheet {
                index,
                bounds,
                imageable,
                side,
                placements,
            });
            index = index.saturating_add(1);
        }
    }
    Ok(out)
}

/// The back side's margins, so that the binding margin stays on the binding
/// side once the sheet is turned over.
///
/// Long-edge binding flips the sheet about its long axis, short-edge about
/// its short one, so on a portrait sheet long-edge is a left/right mirror and
/// short-edge a top/bottom one — and the other way round on a landscape
/// sheet. A booklet always folds down the vertical centre line, so its back
/// is always a left/right mirror.
fn back_margins(front: Margins, duplex: Duplex, booklet: bool, landscape: bool) -> Margins {
    if booklet {
        return front.mirrored_horizontally();
    }
    match duplex {
        Duplex::None => front,
        Duplex::LongEdge => {
            if landscape {
                front.mirrored_vertically()
            } else {
                front.mirrored_horizontally()
            }
        }
        Duplex::ShortEdge => {
            if landscape {
                front.mirrored_horizontally()
            } else {
                front.mirrored_vertically()
            }
        }
    }
}

/// Resolve [`Orientation::Auto`] against the pages that land on one physical
/// sheet: lay them out both ways and keep the rotation that puts more of them
/// on the paper.
fn choose_orientation(
    pages: &[SourcePage],
    options: &PrintOptions,
    hardware: Margins,
) -> Result<Orientation, PrintError> {
    let candidates: &[Orientation] = match options.orientation {
        Orientation::Portrait => &[Orientation::Portrait],
        Orientation::Landscape => &[Orientation::Landscape],
        Orientation::Auto => &[Orientation::Portrait, Orientation::Landscape],
    };
    let margins = options.margins.max_with(hardware);
    let page_rotation = page_rotation(options.orientation);

    let mut best: Option<(Orientation, FitScore)> = None;
    for &candidate in candidates {
        let bounds = options.paper.rect(candidate);
        let imageable = inset(bounds, margins);
        if imageable.is_empty() {
            continue;
        }
        let landscape = candidate == Orientation::Landscape;
        let (columns, rows) = cell_grid(options.n_up, landscape);
        let cells = cell_rects(imageable, columns, rows, NUpOrder::RightThenDown);
        let mut score = FitScore::ZERO;
        for (position, page) in pages.iter().enumerate() {
            // A duplex group carries more pages than one side has cells, so
            // wrap: page k of the group would sit in cell k mod cells.
            if let Some(cell) = cells.get(position % cells.len().max(1)) {
                score = score.plus(fit(*page, *cell, options.scaling, page_rotation).score);
            }
        }
        let better = match best {
            None => true,
            Some((_, incumbent)) => score.better_than(incumbent),
        };
        if better {
            best = Some((candidate, score));
        }
    }

    match best {
        Some((orientation, _)) => Ok(orientation),
        None => {
            let bounds = options.paper.rect(options.orientation);
            Err(PrintError::EmptyImageableArea {
                paper_w: bounds.width(),
                paper_h: bounds.height(),
            })
        }
    }
}

/// How a page may turn inside its cell, given the sheet's own rotation
/// policy. See the module documentation: the quarter turn belongs to the
/// sheet when the sheet is free to take it, and to the page when it is not.
fn page_rotation(sheet: Orientation) -> Orientation {
    match sheet {
        Orientation::Auto => Orientation::Portrait,
        Orientation::Portrait | Orientation::Landscape => Orientation::Auto,
    }
}

/// The `(columns, rows)` an N-up sheet is divided into.
///
/// [`NUp::grid`] is read in the *sheet's own* frame, whichever way the sheet
/// is turned. A booklet is the exception: it is always two cells side by
/// side, because the fold is always the vertical centre line.
///
/// The tempting alternative — treating [`NUp::grid`] as portrait-relative and
/// swapping the pair for a landscape sheet — keeps a cell's shape fixed
/// relative to the paper, but it costs real paper on the two non-square
/// grids. Measured, A4 pages onto A4 paper under `FitToPage` and
/// [`Orientation::Auto`]: two-up falls from 0.707 to 0.500 and six-up from
/// 0.354 to 0.333, while every square grid is unaffected. Reading the grid in
/// the sheet's frame is also exactly the conventional CUPS arrangement — a
/// landscape sheet with two columns for two-up, three-by-two for six-up — and
/// [`Orientation::Auto`] finds those arrangements unaided, because they score
/// strictly better on the coverage metric it already uses.
fn cell_grid(n_up: NUp, _landscape: bool) -> (u32, u32) {
    if n_up == NUp::Booklet {
        return (2, 1);
    }
    n_up.grid()
}

/// The cells of `area`, in the order `order` fills them. Row 0 is the top
/// row, because reading order runs down the page while sheet coordinates run
/// up it.
fn cell_rects(area: Rect, columns: u32, rows: u32, order: NUpOrder) -> Vec<Rect> {
    let columns = columns.max(1);
    let rows = rows.max(1);
    let count = columns.saturating_mul(rows);
    let cell_width = area.width() / f64::from(columns);
    let cell_height = area.height() / f64::from(rows);

    let mut cells = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for k in 0..count {
        let (column, row) = match order {
            NUpOrder::RightThenDown => (k % columns, k / columns),
            NUpOrder::LeftThenDown => (columns - 1 - (k % columns), k / columns),
            NUpOrder::DownThenRight => (k / rows, k % rows),
            NUpOrder::DownThenLeft => (columns - 1 - (k / rows), k % rows),
        };
        let x0 = area.x0 + f64::from(column) * cell_width;
        let y1 = area.y1 - f64::from(row) * cell_height;
        cells.push(Rect::new(x0, y1 - cell_height, x0 + cell_width, y1));
    }
    cells
}

/// The paper minus its margins. Margins are measured from the paper edge.
fn inset(bounds: Rect, margins: Margins) -> Rect {
    Rect::new(
        bounds.x0 + margins.left,
        bounds.y0 + margins.bottom,
        bounds.x1 - margins.right,
        bounds.y1 - margins.top,
    )
}

// ---------------------------------------------------------------------------
// Fitting
// ---------------------------------------------------------------------------

/// How well a candidate placement uses the paper.
///
/// `visible` is the fraction of the source page that survives the clip and
/// `coverage` the fraction of the cell it fills, both averaged over the pages
/// being scored. Comparing the pair in that order is what "wastes less paper"
/// means across every scaling: a fitting scaling always shows the whole page,
/// so `coverage` decides and the bigger scale wins; a cropping scaling shows
/// less than the whole page, so `visible` decides and the arrangement that
/// crops least wins.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FitScore {
    visible: f64,
    coverage: f64,
}

impl FitScore {
    const ZERO: Self = Self {
        visible: 0.0,
        coverage: 0.0,
    };

    fn plus(self, other: Self) -> Self {
        Self {
            visible: self.visible + other.visible,
            coverage: self.coverage + other.coverage,
        }
    }

    fn better_than(self, other: Self) -> bool {
        if (self.visible - other.visible).abs() > EPSILON {
            return self.visible > other.visible;
        }
        match self.coverage.total_cmp(&other.coverage) {
            Ordering::Greater => self.coverage - other.coverage > EPSILON,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Fit {
    placement: Placement,
    score: FitScore,
}

fn fit(page: SourcePage, cell: Rect, scaling: Scaling, orientation: Orientation) -> Fit {
    let rotations: &[i32] = match orientation {
        Orientation::Portrait => &[0],
        Orientation::Landscape => &[90],
        Orientation::Auto => &[0, 90],
    };
    let mut best: Option<Fit> = None;
    for &rotation in rotations {
        let candidate = fit_rotated(page, cell, scaling, rotation);
        let better = match best {
            None => true,
            Some(incumbent) => candidate.score.better_than(incumbent.score),
        };
        if better {
            best = Some(candidate);
        }
    }
    best.unwrap_or_else(|| fit_rotated(page, cell, scaling, 0))
}

fn fit_rotated(page: SourcePage, cell: Rect, scaling: Scaling, rotation: i32) -> Fit {
    let cell = cell.normalized();
    let page_width = page.width.max(0.0);
    let page_height = page.height.max(0.0);

    // Extent of the page once rotated, and the bottom-left corner of that
    // rotated box in unscaled rotated coordinates. A quarter turn
    // counter-clockwise sends (x, y) to (-y, x), so the box moves left.
    let (extent_x, extent_y, origin_x, origin_y) = if rotation.rem_euclid(360) == 90 {
        (page_height, page_width, -page_height, 0.0)
    } else {
        (page_width, page_height, 0.0, 0.0)
    };

    let cell_width = cell.width().max(0.0);
    let cell_height = cell.height().max(0.0);
    let scale = scale_for(scaling, extent_x, extent_y, cell_width, cell_height);
    let placed_width = extent_x * scale;
    let placed_height = extent_y * scale;

    let x0 = cell.x0 + (cell_width - placed_width) / 2.0;
    let y0 = cell.y0 + (cell_height - placed_height) / 2.0;
    let transform = Matrix::rotate_degrees(rotation)
        .then(Matrix::scale(scale, scale))
        .then(Matrix::translate(
            x0 - origin_x * scale,
            y0 - origin_y * scale,
        ));

    let placed = Rect::from_size(x0, y0, placed_width, placed_height);
    let visible = placed.intersect(cell);
    let visible_area = visible.width().max(0.0) * visible.height().max(0.0);
    let placed_area = placed_width * placed_height;
    let cell_area = cell_width * cell_height;

    Fit {
        placement: Placement {
            source_page: page.index,
            transform,
            clip: cell,
        },
        score: FitScore {
            visible: if placed_area > EPSILON {
                visible_area / placed_area
            } else {
                0.0
            },
            coverage: if cell_area > EPSILON {
                visible_area / cell_area
            } else {
                0.0
            },
        },
    }
}

fn scale_for(scaling: Scaling, width: f64, height: f64, cell_width: f64, cell_height: f64) -> f64 {
    let x = ratio(cell_width, width);
    let y = ratio(cell_height, height);
    let scale = match scaling {
        Scaling::ActualSize => 1.0,
        Scaling::FitToPage => x.min(y),
        Scaling::ShrinkToFit => x.min(y).min(1.0),
        Scaling::FillPage => x.max(y),
        Scaling::Percent(percent) => percent,
    };
    if scale.is_finite() && scale > 0.0 { scale } else { 1.0 }
}

/// `numerator / denominator`, with a degenerate denominator reading as "no
/// constraint from this axis" rather than a NaN.
fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator > EPSILON {
        numerator / denominator
    } else {
        f64::INFINITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booklet_slots_are_blank_past_the_page_count() {
        // N = 6 pads to 8; positions 7 and 8 do not exist.
        assert_eq!(booklet_slots(0, 6), [None, Some(0), Some(1), None]);
        assert_eq!(booklet_slots(1, 6), [Some(5), Some(2), Some(3), Some(4)]);
    }

    #[test]
    fn cells_run_left_to_right_then_down() {
        let cells = cell_rects(Rect::new(0.0, 0.0, 100.0, 100.0), 2, 2, NUpOrder::RightThenDown);
        assert_eq!(cells[0], Rect::new(0.0, 50.0, 50.0, 100.0));
        assert_eq!(cells[1], Rect::new(50.0, 50.0, 100.0, 100.0));
        assert_eq!(cells[2], Rect::new(0.0, 0.0, 50.0, 50.0));
        assert_eq!(cells[3], Rect::new(50.0, 0.0, 100.0, 50.0));
    }

    #[test]
    fn cell_order_variants_permute_the_same_cells() {
        let area = Rect::new(0.0, 0.0, 90.0, 60.0);
        let base = cell_rects(area, 3, 2, NUpOrder::RightThenDown);
        for order in [
            NUpOrder::LeftThenDown,
            NUpOrder::DownThenRight,
            NUpOrder::DownThenLeft,
        ] {
            let cells = cell_rects(area, 3, 2, order);
            assert_eq!(cells.len(), base.len());
            for cell in &cells {
                assert!(base.contains(cell), "{order:?} produced a stray cell {cell:?}");
            }
        }
    }

    #[test]
    fn a_quarter_turn_lands_the_page_inside_the_cell() {
        let page = SourcePage::new(0, 200.0, 100.0);
        let cell = Rect::new(10.0, 20.0, 110.0, 220.0);
        let placement = fit_page(page, cell, Scaling::FitToPage, Orientation::Landscape);
        let bounds = placement.transformed_bounds(page);
        assert!(bounds.contained_by(cell, 1e-6), "{bounds:?} escaped {cell:?}");
        // Rotated, the 200x100 page is 100 wide and 200 tall: an exact fit.
        assert!((bounds.width() - 100.0).abs() < 1e-6, "{bounds:?}");
        assert!((bounds.height() - 200.0).abs() < 1e-6, "{bounds:?}");
    }

    #[test]
    fn back_margins_mirror_on_the_binding_axis() {
        let front = Margins {
            left: 40.0,
            right: 10.0,
            top: 5.0,
            bottom: 20.0,
        };
        let portrait_long = back_margins(front, Duplex::LongEdge, false, false);
        assert_eq!(portrait_long.left, 10.0);
        assert_eq!(portrait_long.right, 40.0);
        assert_eq!(portrait_long.top, 5.0);

        let portrait_short = back_margins(front, Duplex::ShortEdge, false, false);
        assert_eq!(portrait_short.left, 40.0);
        assert_eq!(portrait_short.top, 20.0);
        assert_eq!(portrait_short.bottom, 5.0);

        // A landscape sheet swaps which mirror belongs to which binding.
        assert_eq!(
            back_margins(front, Duplex::LongEdge, false, true),
            portrait_short
        );
        assert_eq!(
            back_margins(front, Duplex::ShortEdge, false, true),
            portrait_long
        );
    }
}
