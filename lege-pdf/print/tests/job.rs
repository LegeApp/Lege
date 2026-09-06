//! Route selection — the one decision `job.rs` owns.
//!
//! The spooler is faked, so these assert the seam and nothing below it: no
//! printer, no filesystem, no renderer. The pass-through path never opens the
//! document, which is why a few bytes of header stand in for a PDF.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use lege_pdf_print::job::{
    MAX_COMPOSE_DPI, PrintRequest, RouteKind, compose_options_for, print_document_with, route_for,
};
use lege_pdf_print::spool::{
    DeviceCapabilities, JobId, JobStatus, PrinterId, PrinterInfo, SpoolJob, SpoolPayload, Spooler,
};
use lege_pdf_print::{Margins, NUp, PrintError, PrintOptions, PrintRoute, Scaling};

/// What the fake spooler saw, flattened so the test can compare it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    PassThrough {
        printer: String,
        title: String,
        bytes: usize,
    },
    Sheets {
        printer: String,
        title: String,
        sheets: usize,
    },
}

#[derive(Debug)]
struct FakeSpooler {
    capabilities: DeviceCapabilities,
    seen: Mutex<Vec<Seen>>,
}

impl FakeSpooler {
    fn new(capabilities: DeviceCapabilities) -> Self {
        Self {
            capabilities,
            seen: Mutex::new(Vec::new()),
        }
    }

    /// A queue that takes PDF directly, as CUPS does.
    fn pdf_queue() -> Self {
        Self::new(DeviceCapabilities {
            accepts_pdf: true,
            ..DeviceCapabilities::default()
        })
    }

    /// A queue that only takes bitmaps, as winspool does.
    fn raster_queue() -> Self {
        Self::new(DeviceCapabilities {
            accepts_pdf: false,
            ..DeviceCapabilities::default()
        })
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Spooler for FakeSpooler {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PrintError> {
        Ok(vec![PrinterInfo {
            id: PrinterId::new("fake"),
            description: Some("Fake queue".into()),
            location: None,
            is_default: true,
            accepting_jobs: true,
        }])
    }

    fn capabilities(&self, _printer: &PrinterId) -> Result<DeviceCapabilities, PrintError> {
        Ok(self.capabilities.clone())
    }

    fn submit(&self, job: SpoolJob<'_>) -> Result<JobId, PrintError> {
        let printer = job.printer.as_str().to_owned();
        let title = job.title.clone();
        let record = match job.payload {
            SpoolPayload::PassThroughPdf(bytes) => Seen::PassThrough {
                printer,
                title,
                bytes: bytes.len(),
            },
            SpoolPayload::Sheets { sheets, .. } => Seen::Sheets {
                printer,
                title,
                sheets: sheets.len(),
            },
        };
        let mut seen = self.seen.lock().unwrap();
        seen.push(record);
        Ok(JobId(format!("fake-{}", seen.len())))
    }

    fn status(&self, _job: &JobId) -> Result<JobStatus, PrintError> {
        Ok(JobStatus::Unknown)
    }

    fn cancel(&self, _job: &JobId) -> Result<(), PrintError> {
        Ok(())
    }
}

fn pdf_bytes() -> Arc<[u8]> {
    Arc::from(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".as_slice())
}

/// A real one-page document, for the composed route — which, unlike
/// pass-through, actually opens what it is handed.
fn fixture_bytes() -> Arc<[u8]> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../render/crates/pdf-chaos-tests/tests/fixtures/hello_world.pdf");
    Arc::from(std::fs::read(path).unwrap().into_boxed_slice())
}

fn request<'a>(printer: &'a PrinterId, options: &'a PrintOptions) -> PrintRequest<'a> {
    PrintRequest {
        printer,
        title: "job title",
        pdf_bytes: pdf_bytes(),
        password: None,
        options,
        compose: None,
    }
}

#[test]
fn plain_job_on_a_pdf_queue_routes_pass_through() {
    let spooler = FakeSpooler::pdf_queue();
    let printer = PrinterId::new("fake");
    let options = PrintOptions::default();

    assert_eq!(
        route_for(&options, &spooler.capabilities),
        RouteKind::PassThrough
    );

    let submitted = print_document_with(&spooler, &request(&printer, &options)).unwrap();
    assert_eq!(submitted.route, PrintRoute::PassThrough);
    assert_eq!(submitted.route.sheets(), None);
    assert_eq!(submitted.id, JobId("fake-1".into()));
    assert_eq!(
        spooler.seen(),
        vec![Seen::PassThrough {
            printer: "fake".into(),
            title: "job title".into(),
            bytes: pdf_bytes().len(),
        }]
    );
}

#[test]
fn n_up_forces_composition() {
    let capabilities = FakeSpooler::pdf_queue().capabilities;
    let options = PrintOptions {
        n_up: NUp::Two,
        ..PrintOptions::default()
    };
    assert_eq!(route_for(&options, &capabilities), RouteKind::Composed);
}

#[test]
fn a_booklet_forces_composition() {
    let capabilities = FakeSpooler::pdf_queue().capabilities;
    let options = PrintOptions {
        n_up: NUp::Booklet,
        ..PrintOptions::default()
    };
    assert_eq!(route_for(&options, &capabilities), RouteKind::Composed);
}

#[test]
fn a_margin_forces_composition() {
    let capabilities = FakeSpooler::pdf_queue().capabilities;
    let options = PrintOptions {
        margins: Margins::millimetres(10.0),
        ..PrintOptions::default()
    };
    assert_eq!(route_for(&options, &capabilities), RouteKind::Composed);
}

#[test]
fn scaling_that_changes_geometry_forces_composition() {
    let capabilities = FakeSpooler::pdf_queue().capabilities;
    for scaling in [Scaling::FitToPage, Scaling::FillPage, Scaling::Percent(0.5)] {
        let options = PrintOptions {
            scaling,
            ..PrintOptions::default()
        };
        assert_eq!(
            route_for(&options, &capabilities),
            RouteKind::Composed,
            "{scaling:?}"
        );
    }
}

#[test]
fn a_queue_that_cannot_take_pdf_forces_composition() {
    let capabilities = FakeSpooler::raster_queue().capabilities;
    let options = PrintOptions::default();
    assert!(options.is_pass_through_capable());
    assert_eq!(route_for(&options, &capabilities), RouteKind::Composed);
}

#[test]
fn spooler_options_do_not_force_composition() {
    // Copies, collation, duplex, reverse and a page range are all things the
    // spooler expresses itself, so none of them costs us a rasterization.
    let capabilities = FakeSpooler::pdf_queue().capabilities;
    let options = PrintOptions {
        copies: 3,
        collate: false,
        reverse: true,
        duplex: lege_pdf_print::Duplex::LongEdge,
        range: lege_pdf_print::PageRange::Odd,
        ..PrintOptions::default()
    };
    assert_eq!(route_for(&options, &capabilities), RouteKind::PassThrough);
}

#[test]
fn invalid_options_are_rejected_before_anything_is_submitted() {
    let spooler = FakeSpooler::pdf_queue();
    let printer = PrinterId::new("fake");
    let options = PrintOptions {
        copies: 0,
        ..PrintOptions::default()
    };
    let err = print_document_with(&spooler, &request(&printer, &options)).unwrap_err();
    assert!(matches!(err, PrintError::InvalidOptions(_)), "{err:?}");
    assert!(spooler.seen().is_empty());
}

#[test]
fn compose_settings_follow_the_device() {
    let options = PrintOptions::default();

    let unknown_resolution = DeviceCapabilities::default();
    assert_eq!(
        compose_options_for(&options, &unknown_resolution).dpi,
        lege_pdf_print::ComposeOptions::default().dpi
    );

    let laser = DeviceCapabilities {
        resolution_dpi: Some(1200.0),
        ..DeviceCapabilities::default()
    };
    assert_eq!(compose_options_for(&options, &laser).dpi, MAX_COMPOSE_DPI);

    let mono = DeviceCapabilities {
        supports_color: false,
        ..DeviceCapabilities::default()
    };
    assert!(compose_options_for(&options, &mono).grayscale);
}

#[test]
fn composed_jobs_reach_the_spooler_as_sheets() {
    let spooler = FakeSpooler::raster_queue();
    let printer = PrinterId::new("fake");
    let options = PrintOptions {
        n_up: NUp::Two,
        ..PrintOptions::default()
    };
    let submitted = print_document_with(
        &spooler,
        &PrintRequest {
            pdf_bytes: fixture_bytes(),
            ..request(&printer, &options)
        },
    )
    .unwrap();
    assert_eq!(submitted.route, PrintRoute::Composed { sheets: 1 });
    assert_eq!(
        spooler.seen(),
        vec![Seen::Sheets {
            printer: "fake".into(),
            title: "job title".into(),
            sheets: 1,
        }]
    );
}

#[test]
fn copies_are_left_to_the_spooler() {
    // Both platform spoolers take a native copy count. Expanding the sheet
    // run here as well would print copies-squared pages.
    let spooler = FakeSpooler::raster_queue();
    let printer = PrinterId::new("fake");
    let one = PrintOptions {
        n_up: NUp::Two,
        ..PrintOptions::default()
    };
    let many = PrintOptions {
        copies: 7,
        ..one.clone()
    };

    let submit = |options: &PrintOptions| {
        print_document_with(
            &spooler,
            &PrintRequest {
                pdf_bytes: fixture_bytes(),
                ..request(&printer, options)
            },
        )
        .unwrap()
        .route
    };
    assert_eq!(submit(&one), submit(&many));
}

#[test]
fn the_pass_through_route_never_opens_the_document() {
    // `pdf_bytes` is not a PDF at all: if the pass-through route ever tried
    // to parse what it spools, this would fail.
    let spooler = FakeSpooler::pdf_queue();
    let printer = PrinterId::new("fake");
    let options = PrintOptions::default();
    let submitted = print_document_with(&spooler, &request(&printer, &options)).unwrap();
    assert_eq!(submitted.route.kind(), RouteKind::PassThrough);
}
