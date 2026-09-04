//! Property tests for the imposition core.
//!
//! `layout.rs` is pure geometry, so these are invariants rather than golden
//! files: nothing escapes the imageable area, nothing overlaps its neighbour,
//! aspect ratios survive, the sheet count is the arithmetic one, and folding
//! a booklet reproduces the reading order. Every one of them is a failure
//! mode that is visible on paper and nowhere else.

use lege_pdf_print::layout::{booklet_slots, expand_copies, fit_page, impose};
use lege_pdf_print::paper::{Margins, Orientation, PaperSize, Rect};
use lege_pdf_print::{
    Duplex, NUp, NUpOrder, PageRange, PrintError, PrintJob, PrintOptions, Scaling, Side,
    SourcePage,
};

/// Points of float slop tolerated when comparing geometry.
const EPS: f64 = 1e-6;

/// A quarter inch, the conservative default unprintable border.
fn hardware() -> Margins {
    Margins::uniform(18.0)
}

fn pages(sizes: &[(f64, f64)]) -> Vec<SourcePage> {
    sizes
        .iter()
        .enumerate()
        .map(|(i, &(w, h))| SourcePage::new(u32::try_from(i).unwrap_or(0), w, h))
        .collect()
}

/// `count` identical A4 portrait pages.
fn a4_pages(count: usize) -> Vec<SourcePage> {
    let (w, h) = PaperSize::A4.size();
    pages(&vec![(w, h); count])
}

fn job(pages: Vec<SourcePage>, options: PrintOptions) -> PrintJob {
    PrintJob::new(pages, options)
}

fn find(pages: &[SourcePage], index: u32) -> SourcePage {
    match pages.iter().find(|p| p.index == index) {
        Some(page) => *page,
        None => unreachable!("placement named a page the job does not have"),
    }
}

/// Every scaling that promises the whole page lands on the sheet.
const FITTING: [Scaling; 2] = [Scaling::FitToPage, Scaling::ShrinkToFit];

const EVERY_N_UP: [NUp; 6] = [
    NUp::One,
    NUp::Two,
    NUp::Four,
    NUp::Six,
    NUp::Nine,
    NUp::Sixteen,
];

// ---------------------------------------------------------------------------
// Containment
// ---------------------------------------------------------------------------

#[test]
fn fitted_placements_never_leave_the_imageable_area() {
    let sizes = [
        (595.28, 841.89), // A4 portrait
        (841.89, 595.28), // A4 landscape
        (200.0, 1000.0),  // a very tall page
        (1000.0, 200.0),  // a very wide one
        (400.0, 400.0),   // square
    ];
    let document = pages(&sizes);

    for n_up in EVERY_N_UP {
        for scaling in FITTING {
            for orientation in [
                Orientation::Portrait,
                Orientation::Landscape,
                Orientation::Auto,
            ] {
                let options = PrintOptions {
                    paper: PaperSize::A4,
                    orientation,
                    margins: Margins::millimetres(10.0),
                    scaling,
                    n_up,
                    ..PrintOptions::default()
                };
                let sheets = impose(&job(document.clone(), options), hardware())
                    .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
                for sheet in &sheets {
                    assert!(
                        sheet.imageable.contained_by(sheet.bounds, EPS),
                        "imageable {:?} escaped the sheet {:?}",
                        sheet.imageable,
                        sheet.bounds
                    );
                    for placement in &sheet.placements {
                        let page = find(&document, placement.source_page);
                        let bounds = placement.transformed_bounds(page);
                        assert!(
                            bounds.contained_by(sheet.imageable, EPS),
                            "{n_up:?}/{scaling:?}/{orientation:?}: page {} at {bounds:?} \
                             escaped imageable {:?}",
                            page.index,
                            sheet.imageable
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn user_margins_are_clamped_outward_to_the_hardware_margin() {
    // The user asked for no margin at all; the printer cannot honour that.
    let options = PrintOptions {
        paper: PaperSize::A4,
        orientation: Orientation::Portrait,
        margins: Margins::ZERO,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(1), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    let sheet = &sheets[0];
    assert!((sheet.imageable.x0 - sheet.bounds.x0 - 18.0).abs() < EPS);
    assert!((sheet.bounds.x1 - sheet.imageable.x1 - 18.0).abs() < EPS);
    assert!((sheet.imageable.y0 - sheet.bounds.y0 - 18.0).abs() < EPS);
    assert!((sheet.bounds.y1 - sheet.imageable.y1 - 18.0).abs() < EPS);
}

#[test]
fn a_user_margin_wider_than_the_hardware_one_is_kept() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        orientation: Orientation::Portrait,
        margins: Margins {
            left: 72.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        },
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(1), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    let sheet = &sheets[0];
    assert!((sheet.imageable.x0 - 72.0).abs() < EPS, "{:?}", sheet.imageable);
    assert!(
        (sheet.bounds.x1 - sheet.imageable.x1 - 18.0).abs() < EPS,
        "{:?}",
        sheet.imageable
    );
}

// ---------------------------------------------------------------------------
// N-up
// ---------------------------------------------------------------------------

#[test]
fn n_up_placements_on_one_sheet_never_overlap() {
    let document = pages(&[
        (595.28, 841.89),
        (841.89, 595.28),
        (300.0, 900.0),
        (900.0, 300.0),
        (400.0, 400.0),
        (500.0, 700.0),
        (700.0, 500.0),
        (250.0, 250.0),
        (612.0, 792.0),
        (792.0, 612.0),
        (100.0, 800.0),
        (800.0, 100.0),
        (450.0, 650.0),
        (650.0, 450.0),
        (350.0, 350.0),
        (595.0, 595.0),
    ]);

    for n_up in EVERY_N_UP {
        for order in [
            NUpOrder::RightThenDown,
            NUpOrder::LeftThenDown,
            NUpOrder::DownThenRight,
            NUpOrder::DownThenLeft,
        ] {
            let options = PrintOptions {
                paper: PaperSize::A4,
                n_up,
                n_up_order: order,
                scaling: Scaling::FitToPage,
                ..PrintOptions::default()
            };
            let sheets = impose(&job(document.clone(), options), hardware())
                .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
            for sheet in &sheets {
                for (i, a) in sheet.placements.iter().enumerate() {
                    for b in sheet.placements.iter().skip(i + 1) {
                        assert!(
                            !a.clip.overlaps(b.clip, EPS),
                            "{n_up:?}/{order:?}: cells {:?} and {:?} overlap",
                            a.clip,
                            b.clip
                        );
                        let a_bounds = a.transformed_bounds(find(&document, a.source_page));
                        let b_bounds = b.transformed_bounds(find(&document, b.source_page));
                        assert!(
                            !a_bounds.overlaps(b_bounds, EPS),
                            "{n_up:?}/{order:?}: pages {} and {} overlap on sheet {}",
                            a.source_page,
                            b.source_page,
                            sheet.index
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn n_up_order_permutes_the_same_cells() {
    let document = a4_pages(4);
    let mut seen: Vec<Vec<Rect>> = Vec::new();
    for order in [
        NUpOrder::RightThenDown,
        NUpOrder::LeftThenDown,
        NUpOrder::DownThenRight,
        NUpOrder::DownThenLeft,
    ] {
        let options = PrintOptions {
            paper: PaperSize::A4,
            orientation: Orientation::Portrait,
            n_up: NUp::Four,
            n_up_order: order,
            ..PrintOptions::default()
        };
        let sheets = impose(&job(document.clone(), options), hardware())
            .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
        assert_eq!(sheets.len(), 1);
        seen.push(sheets[0].placements.iter().map(|p| p.clip).collect());
    }
    // Same four cells every time, in different orders.
    let reference = &seen[0];
    for cells in &seen[1..] {
        assert_eq!(cells.len(), reference.len());
        for cell in cells {
            assert!(reference.contains(cell), "stray cell {cell:?}");
        }
    }
    // And the orders really do differ.
    assert_ne!(seen[0], seen[1]);
    assert_ne!(seen[0], seen[2]);
}

#[test]
fn the_grid_is_read_in_the_sheets_own_frame() {
    // Six-up is three columns and two rows of whichever sheet it is given --
    // not three columns of a portrait sheet transposed onto a landscape one.
    // Turning the grid with the sheet would keep each cell's shape fixed
    // relative to the paper, but it wastes paper: see `cell_grid`.
    let document = a4_pages(6);
    let cells = |orientation| -> (f64, f64) {
        let options = PrintOptions {
            paper: PaperSize::A4,
            orientation,
            margins: Margins::ZERO,
            n_up: NUp::Six,
            ..PrintOptions::default()
        };
        let sheets = impose(&job(document.clone(), options), Margins::ZERO)
            .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
        let clip = sheets[0].placements[0].clip;
        (clip.width(), clip.height())
    };
    let (pw, ph) = cells(Orientation::Portrait);
    let (lw, lh) = cells(Orientation::Landscape);
    let (paper_w, paper_h) = PaperSize::A4.size();
    assert!((pw - paper_w / 3.0).abs() < EPS, "{pw}");
    assert!((ph - paper_h / 2.0).abs() < EPS, "{ph}");
    assert!((lw - paper_h / 3.0).abs() < EPS, "{lw}");
    assert!((lh - paper_w / 2.0).abs() < EPS, "{lh}");
}

#[test]
fn auto_orientation_finds_the_arrangement_that_wastes_less_paper() {
    // The measured payoff of reading the grid in the sheet's own frame:
    // two-up A4 onto A4 reaches ~0.707 rather than 0.5, and six-up ~0.354
    // rather than 0.333. `Auto` must find those unaided, so the expectation
    // is computed from the paper rather than asserted as the old constant --
    // A4 is 210x297 mm, whose ratio is near but not exactly sqrt(2).
    let (paper_w, paper_h) = PaperSize::A4.size();
    let best_scale = |n_up: NUp| -> f64 {
        let (columns, rows) = n_up.grid();
        [(paper_w, paper_h), (paper_h, paper_w)]
            .into_iter()
            .map(|(sheet_w, sheet_h)| {
                let cell_w = sheet_w / f64::from(columns);
                let cell_h = sheet_h / f64::from(rows);
                (cell_w / paper_w).min(cell_h / paper_h)
            })
            .fold(f64::MIN, f64::max)
    };
    for n_up in [NUp::Two, NUp::Four, NUp::Six, NUp::Nine, NUp::Sixteen] {
        let expected = best_scale(n_up);
        let options = PrintOptions {
            paper: PaperSize::A4,
            orientation: Orientation::Auto,
            margins: Margins::ZERO,
            n_up,
            scaling: Scaling::FitToPage,
            ..PrintOptions::default()
        };
        let sheets = impose(
            &job(a4_pages(n_up.per_side() as usize), options),
            Margins::ZERO,
        )
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
        let transform = sheets[0].placements[0].transform;
        let scale = transform.a.hypot(transform.b);
        assert!(
            (scale - expected).abs() < 1e-9,
            "{n_up:?}: scale {scale} is not the achievable {expected}"
        );
    }
}

// ---------------------------------------------------------------------------
// Scaling
// ---------------------------------------------------------------------------

#[test]
fn fitting_scalings_preserve_the_aspect_ratio() {
    let cell = Rect::new(20.0, 30.0, 320.0, 230.0);
    for &(w, h) in &[(200.0, 400.0), (400.0, 200.0), (55.0, 55.0), (17.0, 991.0)] {
        let page = SourcePage::new(0, w, h);
        for scaling in FITTING {
            for orientation in [Orientation::Portrait, Orientation::Auto] {
                let bounds =
                    fit_page(page, cell, scaling, orientation).transformed_bounds(page);
                let placed = bounds.width() / bounds.height();
                let upright = w / h;
                assert!(
                    (placed - upright).abs() < 1e-9 || (placed - 1.0 / upright).abs() < 1e-9,
                    "{scaling:?}/{orientation:?}: {w}x{h} became {placed}"
                );
                assert!(bounds.contained_by(cell, EPS), "{bounds:?} escaped {cell:?}");
            }
        }
    }
}

#[test]
fn shrink_to_fit_never_scales_above_one() {
    // A cell far larger than any of these pages.
    let cell = Rect::new(0.0, 0.0, 2000.0, 2000.0);
    for &(w, h) in &[(200.0, 400.0), (400.0, 200.0), (55.0, 55.0)] {
        let page = SourcePage::new(0, w, h);
        let shrink = fit_page(page, cell, Scaling::ShrinkToFit, Orientation::Portrait)
            .transformed_bounds(page);
        assert!((shrink.width() - w).abs() < 1e-9, "{shrink:?}");
        assert!((shrink.height() - h).abs() < 1e-9, "{shrink:?}");

        // FitToPage, given the same room, does scale up.
        let grow = fit_page(page, cell, Scaling::FitToPage, Orientation::Portrait)
            .transformed_bounds(page);
        assert!(grow.width() > w + 1.0, "{grow:?}");
    }
}

#[test]
fn actual_size_is_one_to_one_and_clips_the_overflow() {
    let page = SourcePage::new(0, 1000.0, 1000.0);
    let cell = Rect::new(0.0, 0.0, 400.0, 400.0);
    let placement = fit_page(page, cell, Scaling::ActualSize, Orientation::Portrait);
    let bounds = placement.transformed_bounds(page);
    assert!((bounds.width() - 1000.0).abs() < 1e-9, "{bounds:?}");
    assert!(!bounds.contained_by(cell, EPS), "1:1 should overflow here");
    // What reaches the paper is the cell, and no more.
    let painted = placement.painted_bounds(page);
    assert!(painted.contained_by(cell, EPS), "{painted:?}");
    assert!((painted.width() - 400.0).abs() < 1e-9, "{painted:?}");
}

#[test]
fn fill_page_covers_the_cell_and_crops() {
    let page = SourcePage::new(0, 200.0, 400.0);
    let cell = Rect::new(0.0, 0.0, 400.0, 400.0);
    let bounds = fit_page(page, cell, Scaling::FillPage, Orientation::Portrait)
        .transformed_bounds(page);
    // Cover: the short axis is met exactly, the long one overflows.
    assert!((bounds.width() - 400.0).abs() < 1e-9, "{bounds:?}");
    assert!(bounds.height() > 400.0 + EPS, "{bounds:?}");
    // Still centred, so the crop is symmetric.
    let (cx, cy) = bounds.center();
    assert!((cx - 200.0).abs() < 1e-9 && (cy - 200.0).abs() < 1e-9);
}

#[test]
fn percent_is_a_factor_not_a_percentage() {
    let page = SourcePage::new(0, 100.0, 100.0);
    let cell = Rect::new(0.0, 0.0, 1000.0, 1000.0);
    let half = fit_page(page, cell, Scaling::Percent(0.5), Orientation::Portrait)
        .transformed_bounds(page);
    assert!((half.width() - 50.0).abs() < 1e-9, "{half:?}");
    let double = fit_page(page, cell, Scaling::Percent(2.0), Orientation::Portrait)
        .transformed_bounds(page);
    assert!((double.width() - 200.0).abs() < 1e-9, "{double:?}");
}

// ---------------------------------------------------------------------------
// Orientation
// ---------------------------------------------------------------------------

#[test]
fn auto_orientation_turns_the_sheet_towards_the_page() {
    let wide = pages(&[(1000.0, 500.0)]);
    let tall = pages(&[(500.0, 1000.0)]);
    let options = PrintOptions {
        paper: PaperSize::A4,
        orientation: Orientation::Auto,
        scaling: Scaling::FitToPage,
        ..PrintOptions::default()
    };
    let wide_sheets = impose(&job(wide, options.clone()), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert!(wide_sheets[0].is_landscape(), "a wide page wants a wide sheet");

    let tall_sheets = impose(&job(tall, options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert!(
        !tall_sheets[0].is_landscape(),
        "a tall page wants a tall sheet"
    );
}

#[test]
fn auto_orientation_decides_once_per_sheet_not_once_per_page() {
    // Four wide pages and one tall one, four to a sheet: the first sheet is
    // decided by its four wide pages, the second by its single tall one.
    let mut sizes = vec![(1000.0, 500.0); 4];
    sizes.push((500.0, 1000.0));
    let document = pages(&sizes);
    let options = PrintOptions {
        paper: PaperSize::A4,
        orientation: Orientation::Auto,
        n_up: NUp::Four,
        scaling: Scaling::FitToPage,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(document, options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert_eq!(sheets.len(), 2);
    assert!(sheets[0].is_landscape());
    assert!(!sheets[1].is_landscape());
    // Every placement on a sheet shares that sheet's bounds.
    for sheet in &sheets {
        for placement in &sheet.placements {
            assert!(placement.clip.contained_by(sheet.imageable, EPS));
        }
    }
}

#[test]
fn a_duplex_pair_shares_one_orientation() {
    // The front is wide, the back tall: they are the same piece of paper, so
    // one rotation has to serve both.
    let document = pages(&[(1000.0, 500.0), (500.0, 1000.0)]);
    let options = PrintOptions {
        paper: PaperSize::A4,
        orientation: Orientation::Auto,
        duplex: Duplex::LongEdge,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(document, options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert_eq!(sheets.len(), 2);
    assert_eq!(sheets[0].bounds, sheets[1].bounds);
}

#[test]
fn mixed_page_sizes_are_fitted_independently() {
    let document = pages(&[(200.0, 400.0), (400.0, 200.0), (100.0, 100.0), (600.0, 800.0)]);
    let options = PrintOptions {
        paper: PaperSize::A4,
        orientation: Orientation::Portrait,
        n_up: NUp::Four,
        scaling: Scaling::FitToPage,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(document.clone(), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert_eq!(sheets.len(), 1);
    let placements = &sheets[0].placements;
    assert_eq!(placements.len(), 4);
    for placement in placements {
        let page = find(&document, placement.source_page);
        let bounds = placement.transformed_bounds(page);
        // Each page fills at least one axis of its own cell exactly: that is
        // what "fitted independently" means.
        let touches_width = (bounds.width() - placement.clip.width()).abs() < EPS;
        let touches_height = (bounds.height() - placement.clip.height()).abs() < EPS;
        assert!(
            touches_width || touches_height,
            "page {} at {bounds:?} does not touch its cell {:?}",
            page.index,
            placement.clip
        );
    }
}

// ---------------------------------------------------------------------------
// Sheet counting
// ---------------------------------------------------------------------------

#[test]
fn side_count_is_pages_over_per_side_rounded_up() {
    for n_up in EVERY_N_UP {
        let per_side = usize::try_from(n_up.per_side()).unwrap_or(1);
        for count in [1usize, 2, 5, 7, 16, 17, 33] {
            let options = PrintOptions {
                paper: PaperSize::A4,
                n_up,
                ..PrintOptions::default()
            };
            let sheets = impose(&job(a4_pages(count), options), hardware())
                .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
            assert_eq!(
                sheets.len(),
                count.div_ceil(per_side),
                "{n_up:?} with {count} pages"
            );
            assert!(sheets.iter().all(|s| s.side == Side::Front));
            // Every page appears exactly once, in order.
            let placed: Vec<u32> = sheets
                .iter()
                .flat_map(|s| s.placements.iter().map(|p| p.source_page))
                .collect();
            let expected: Vec<u32> = (0..u32::try_from(count).unwrap_or(0)).collect();
            assert_eq!(placed, expected, "{n_up:?} with {count} pages");
        }
    }
}

#[test]
fn duplex_pads_to_a_whole_number_of_sheets() {
    for count in [1usize, 2, 3, 4, 5] {
        let options = PrintOptions {
            paper: PaperSize::A4,
            duplex: Duplex::LongEdge,
            ..PrintOptions::default()
        };
        let sheets = impose(&job(a4_pages(count), options), hardware())
            .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
        assert_eq!(sheets.len(), count.div_ceil(2) * 2, "{count} pages");
        assert_eq!(sheets.len() % 2, 0);
        for (i, sheet) in sheets.iter().enumerate() {
            let expected = if i % 2 == 0 { Side::Front } else { Side::Back };
            assert_eq!(sheet.side, expected);
            assert_eq!(sheet.index, u32::try_from(i).unwrap_or(0));
        }
        // The padding is a blank back side, not a duplicated page.
        if count % 2 == 1 {
            assert!(sheets.last().is_some_and(|s| s.placements.is_empty()));
        }
    }
}

#[test]
fn reverse_prints_the_selection_backwards() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        reverse: true,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(4), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    let placed: Vec<u32> = sheets
        .iter()
        .flat_map(|s| s.placements.iter().map(|p| p.source_page))
        .collect();
    assert_eq!(placed, vec![3, 2, 1, 0]);
}

#[test]
fn a_page_range_selects_before_imposition() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        range: PageRange::parse("2,4-5", 6)
            .unwrap_or_else(|e| unreachable!("range parse failed: {e}")),
        n_up: NUp::Two,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(6), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    let placed: Vec<u32> = sheets
        .iter()
        .flat_map(|s| s.placements.iter().map(|p| p.source_page))
        .collect();
    assert_eq!(placed, vec![1, 3, 4]);
    assert_eq!(sheets.len(), 2);
}

// ---------------------------------------------------------------------------
// Duplex geometry
// ---------------------------------------------------------------------------

#[test]
fn long_edge_and_short_edge_backs_differ() {
    // An asymmetric binding margin is what makes the difference visible: a
    // symmetric one looks the same either way, which is exactly why this bug
    // survives casual testing.
    let margins = Margins {
        left: 72.0,
        right: 18.0,
        top: 54.0,
        bottom: 18.0,
    };
    let build = |duplex| {
        let options = PrintOptions {
            paper: PaperSize::A4,
            orientation: Orientation::Portrait,
            margins,
            duplex,
            ..PrintOptions::default()
        };
        impose(&job(a4_pages(2), options), hardware())
            .unwrap_or_else(|e| unreachable!("impose failed: {e}"))
    };
    let long = build(Duplex::LongEdge);
    let short = build(Duplex::ShortEdge);

    // Fronts agree: the binding only ever moves the back.
    assert_eq!(long[0].imageable, short[0].imageable);
    assert_eq!(long[0].placements[0].transform, short[0].placements[0].transform);

    // Backs do not.
    assert_ne!(
        long[1].placements[0].transform, short[1].placements[0].transform,
        "long-edge and short-edge duplex must place the back differently"
    );

    // Long-edge on a portrait sheet mirrors left/right ...
    assert!((long[1].imageable.x0 - 18.0).abs() < EPS, "{:?}", long[1].imageable);
    assert!((long[1].imageable.y0 - 18.0).abs() < EPS, "{:?}", long[1].imageable);
    // ... short-edge mirrors top/bottom.
    assert!((short[1].imageable.x0 - 72.0).abs() < EPS, "{:?}", short[1].imageable);
    assert!((short[1].imageable.y0 - 54.0).abs() < EPS, "{:?}", short[1].imageable);
}

#[test]
fn the_binding_axis_follows_the_sheet_on_landscape_paper() {
    let margins = Margins {
        left: 72.0,
        right: 18.0,
        top: 54.0,
        bottom: 18.0,
    };
    let build = |duplex| {
        let options = PrintOptions {
            paper: PaperSize::A4,
            orientation: Orientation::Landscape,
            margins,
            duplex,
            ..PrintOptions::default()
        };
        impose(&job(a4_pages(2), options), hardware())
            .unwrap_or_else(|e| unreachable!("impose failed: {e}"))
    };
    // On a landscape sheet the long axis is horizontal, so the roles swap.
    let long = build(Duplex::LongEdge);
    let short = build(Duplex::ShortEdge);
    assert!((long[1].imageable.y0 - 54.0).abs() < EPS, "{:?}", long[1].imageable);
    assert!((short[1].imageable.x0 - 18.0).abs() < EPS, "{:?}", short[1].imageable);
}

#[test]
fn simplex_backs_do_not_exist() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        duplex: Duplex::None,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(3), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert_eq!(sheets.len(), 3);
    assert!(sheets.iter().all(|s| s.side == Side::Front));
}

// ---------------------------------------------------------------------------
// Booklets
// ---------------------------------------------------------------------------

/// Fold the emitted booklet back into reading order: reading a saddle-stitched
/// stack gives front-right, back-left going forwards from the start, and
/// front-left, back-right going backwards from the end.
fn fold(total: u32) -> Vec<Option<u32>> {
    let padded = total.div_ceil(4) * 4;
    let mut sequence: Vec<Option<u32>> = vec![None; usize::try_from(padded).unwrap_or(0)];
    for sheet in 0..padded / 4 {
        let [front_left, front_right, back_left, back_right] = booklet_slots(sheet, total);
        let head = usize::try_from(2 * sheet).unwrap_or(0);
        let tail = usize::try_from(padded - 1 - 2 * sheet).unwrap_or(0);
        sequence[head] = front_right;
        sequence[head + 1] = back_left;
        sequence[tail] = front_left;
        sequence[tail - 1] = back_right;
    }
    sequence
}

#[test]
fn booklet_order_round_trips() {
    for total in [4u32, 6, 8, 12] {
        let folded = fold(total);
        let padded = total.div_ceil(4) * 4;
        assert_eq!(folded.len(), usize::try_from(padded).unwrap_or(0));
        for (position, slot) in folded.iter().enumerate() {
            let position = u32::try_from(position).unwrap_or(u32::MAX);
            let expected = if position < total { Some(position) } else { None };
            assert_eq!(*slot, expected, "N={total}, folded position {position}");
        }
    }
}

#[test]
fn booklet_slots_are_the_published_table() {
    // (N-2n, 2n+1) on the front, (2n+2, N-2n-1) on the back, one-based, with
    // the returned values zero-based.
    assert_eq!(booklet_slots(0, 4), [Some(3), Some(0), Some(1), Some(2)]);

    assert_eq!(booklet_slots(0, 8), [Some(7), Some(0), Some(1), Some(6)]);
    assert_eq!(booklet_slots(1, 8), [Some(5), Some(2), Some(3), Some(4)]);

    assert_eq!(booklet_slots(0, 12), [Some(11), Some(0), Some(1), Some(10)]);
    assert_eq!(booklet_slots(1, 12), [Some(9), Some(2), Some(3), Some(8)]);
    assert_eq!(booklet_slots(2, 12), [Some(7), Some(4), Some(5), Some(6)]);

    // Six pages pad to eight; positions 7 and 8 are blank.
    assert_eq!(booklet_slots(0, 6), [None, Some(0), Some(1), None]);
    assert_eq!(booklet_slots(1, 6), [Some(5), Some(2), Some(3), Some(4)]);

    // Past the end of the booklet everything is blank.
    assert_eq!(booklet_slots(9, 6), [None, None, None, None]);
}

#[test]
fn a_booklet_is_two_cells_side_by_side_on_a_landscape_sheet() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        orientation: Orientation::Auto,
        n_up: NUp::Booklet,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(6), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    // Six pages pad to eight slots: two sheets, four sides.
    assert_eq!(sheets.len(), 4);
    for sheet in &sheets {
        assert!(sheet.is_landscape(), "booklets fold down a vertical line");
    }
    for (i, sheet) in sheets.iter().enumerate() {
        let expected = if i % 2 == 0 { Side::Front } else { Side::Back };
        assert_eq!(sheet.side, expected);
    }
    // Sheet 0 front is blank | page 1; sheet 0 back is page 2 | blank.
    assert_eq!(sheets[0].placements.len(), 1);
    assert_eq!(sheets[0].placements[0].source_page, 0);
    assert_eq!(sheets[1].placements.len(), 1);
    assert_eq!(sheets[1].placements[0].source_page, 1);
    // Page 1 sits in the right-hand cell, page 2 in the left-hand one.
    let mid = sheets[0].imageable.center().0;
    assert!(sheets[0].placements[0].clip.x0 >= mid - EPS);
    assert!(sheets[1].placements[0].clip.x1 <= mid + EPS);
}

#[test]
fn a_booklet_back_mirrors_horizontally_whatever_the_duplex_setting() {
    let margins = Margins {
        left: 72.0,
        right: 18.0,
        top: 18.0,
        bottom: 18.0,
    };
    for duplex in [Duplex::None, Duplex::LongEdge, Duplex::ShortEdge] {
        let options = PrintOptions {
            paper: PaperSize::A4,
            orientation: Orientation::Landscape,
            margins,
            n_up: NUp::Booklet,
            duplex,
            ..PrintOptions::default()
        };
        let sheets = impose(&job(a4_pages(4), options), hardware())
            .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
        assert_eq!(sheets.len(), 2);
        assert!((sheets[0].imageable.x0 - 72.0).abs() < EPS);
        assert!(
            (sheets[1].imageable.x0 - 18.0).abs() < EPS,
            "{duplex:?}: {:?}",
            sheets[1].imageable
        );
    }
}

// ---------------------------------------------------------------------------
// Copies
// ---------------------------------------------------------------------------

#[test]
fn impose_ignores_copies_and_expand_copies_applies_them() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        copies: 3,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(2), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert_eq!(sheets.len(), 2, "impose emits one copy");

    let collated = expand_copies(&sheets, 3, true);
    let order: Vec<u32> = collated
        .iter()
        .flat_map(|s| s.placements.iter().map(|p| p.source_page))
        .collect();
    assert_eq!(order, vec![0, 1, 0, 1, 0, 1]);

    let uncollated = expand_copies(&sheets, 3, false);
    let order: Vec<u32> = uncollated
        .iter()
        .flat_map(|s| s.placements.iter().map(|p| p.source_page))
        .collect();
    assert_eq!(order, vec![0, 0, 0, 1, 1, 1]);

    // Both re-index the whole run.
    for (i, sheet) in uncollated.iter().enumerate() {
        assert_eq!(sheet.index, u32::try_from(i).unwrap_or(0));
    }
}

#[test]
fn uncollated_copies_keep_duplex_pairs_together() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        duplex: Duplex::LongEdge,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(2), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert_eq!(sheets.len(), 2);
    let expanded = expand_copies(&sheets, 2, false);
    let sides: Vec<Side> = expanded.iter().map(|s| s.side).collect();
    assert_eq!(
        sides,
        vec![Side::Front, Side::Back, Side::Front, Side::Back],
        "a front must never be separated from its back"
    );
}

#[test]
fn one_copy_is_the_identity() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        ..PrintOptions::default()
    };
    let sheets = impose(&job(a4_pages(3), options), hardware())
        .unwrap_or_else(|e| unreachable!("impose failed: {e}"));
    assert_eq!(expand_copies(&sheets, 1, true), sheets);
    assert_eq!(expand_copies(&sheets, 1, false), sheets);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn an_empty_selection_is_an_error() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        range: PageRange::Spans(vec![(50, 60)]),
        ..PrintOptions::default()
    };
    assert!(matches!(
        impose(&job(a4_pages(3), options), hardware()),
        Err(PrintError::EmptyRange)
    ));

    let options = PrintOptions {
        paper: PaperSize::A4,
        ..PrintOptions::default()
    };
    assert!(matches!(
        impose(&job(Vec::new(), options), hardware()),
        Err(PrintError::EmptyRange)
    ));
}

#[test]
fn paper_smaller_than_its_margins_is_an_error() {
    let options = PrintOptions {
        paper: PaperSize::Custom {
            width: 30.0,
            height: 30.0,
        },
        margins: Margins::ZERO,
        ..PrintOptions::default()
    };
    let result = impose(&job(a4_pages(1), options), Margins::uniform(20.0));
    assert!(
        matches!(result, Err(PrintError::EmptyImageableArea { .. })),
        "{result:?}"
    );
}

#[test]
fn invalid_options_are_rejected_before_any_geometry() {
    let options = PrintOptions {
        paper: PaperSize::A4,
        copies: 0,
        ..PrintOptions::default()
    };
    assert!(matches!(
        impose(&job(a4_pages(1), options), hardware()),
        Err(PrintError::InvalidOptions(_))
    ));

    let options = PrintOptions {
        paper: PaperSize::A4,
        scaling: Scaling::Percent(0.0),
        ..PrintOptions::default()
    };
    assert!(matches!(
        impose(&job(a4_pages(1), options), hardware()),
        Err(PrintError::InvalidOptions(_))
    ));
}
