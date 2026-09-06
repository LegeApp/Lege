//! Spooler backend tests.
//!
//! The file backend is exercised for real — it writes files and they are read
//! back. The CUPS backend is exercised through its parsers and its option
//! builder against string fixtures; nothing here shells out, so the suite
//! behaves the same on a laptop with three printers and on a CI box with
//! none.

// Tests assert by panicking; `expect` is the clearest way to say which step
// failed, and the workspace's deny-by-default is aimed at library code.
#![allow(clippy::expect_used)]

use std::path::PathBuf;

use lege_pdf_print::compose::SheetRaster;
use lege_pdf_print::paper::Margins;
use lege_pdf_print::spool::file::FileSpooler;
use lege_pdf_print::spool::{JobStatus, PrinterId, SpoolJob, SpoolPayload, Spooler};
use lege_pdf_print::{ComposeOptions, Duplex, PageRange, PaperSize, PrintOptions};

// ---------------------------------------------------------------------------
// FileSpooler
// ---------------------------------------------------------------------------

fn solid(width: u32, height: u32, channels: u8, value: u8) -> SheetRaster {
    SheetRaster {
        width,
        height,
        channels,
        pixels: vec![value; (width * height) as usize * channels as usize],
    }
}

#[test]
fn file_backend_reports_one_synthetic_queue() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spooler = FileSpooler::new(dir.path());
    let printers = spooler.printers().expect("printers");
    assert_eq!(printers.len(), 1);
    assert!(printers[0].is_default);
    assert!(printers[0].accepting_jobs);
    assert_eq!(
        spooler.default_printer().expect("default"),
        Some(printers[0].id.clone())
    );

    let caps = spooler.capabilities(&printers[0].id).expect("capabilities");
    assert!(
        caps.accepts_pdf,
        "the file backend can always take PDF bytes"
    );
    assert!(caps.supports_duplex);
    assert!(caps.supports_color);
    // A directory has no unprintable border, which keeps layout tests honest.
    assert_eq!(caps.hardware_margins, Margins::ZERO);
}

#[test]
fn pass_through_writes_the_exact_input_bytes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spooler = FileSpooler::new(dir.path());
    let bytes = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog >>\nendobj\n%%EOF\n";
    let options = PrintOptions {
        copies: 3,
        duplex: Duplex::ShortEdge,
        range: PageRange::parse("2-4", 10).expect("range"),
        ..PrintOptions::default()
    };

    let printer = PrinterId::new("file");
    let id = spooler
        .submit(SpoolJob {
            printer: printer.clone(),
            title: "Contract".to_string(),
            options: &options,
            payload: SpoolPayload::PassThroughPdf(bytes),
        })
        .expect("submit");

    let recorded = spooler.recorded();
    assert_eq!(recorded.len(), 1);
    let job = &recorded[0];
    assert_eq!(job.id, id);
    assert_eq!(job.printer, printer);
    assert_eq!(job.title, "Contract");
    assert!(job.pass_through);
    assert_eq!(job.compose, None);
    // The options the caller chose reached the backend unaltered.
    assert_eq!(job.options, options);

    assert_eq!(job.files.len(), 1);
    assert_eq!(
        job.files[0].file_name().and_then(|n| n.to_str()),
        Some("document-0001.pdf")
    );
    let written = std::fs::read(&job.files[0]).expect("read back");
    assert_eq!(written, bytes, "pass-through must not touch the bytes");

    assert_eq!(spooler.status(&id).expect("status"), JobStatus::Completed);
    spooler.cancel(&id).expect("cancel is a no-op");
}

#[test]
fn sheets_are_written_one_png_each_in_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spooler = FileSpooler::new(dir.path());
    let options = PrintOptions {
        paper: PaperSize::A5,
        grayscale: true,
        ..PrintOptions::default()
    };
    let compose = ComposeOptions {
        dpi: 150.0,
        grayscale: true,
        ..ComposeOptions::default()
    };
    let rasters = vec![
        solid(8, 4, 1, 0x20),
        solid(8, 4, 1, 0x40),
        solid(8, 4, 1, 0x60),
    ];

    let printer = PrinterId::new("file");
    let id = spooler
        .submit_rasters(&printer, "Booklet", &options, compose, &rasters)
        .expect("submit rasters");

    let job = spooler.last_job().expect("a recorded job");
    assert_eq!(job.id, id);
    assert!(!job.pass_through);
    assert_eq!(job.compose, Some(compose));
    assert_eq!(job.options, options);

    let names: Vec<String> = job
        .files
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
        .collect();
    assert_eq!(
        names,
        ["sheet-0001.png", "sheet-0002.png", "sheet-0003.png"]
    );

    for (path, raster) in job.files.iter().zip(&rasters) {
        let (info, pixels) = decode_png(path);
        assert_eq!(info.width, raster.width);
        assert_eq!(info.height, raster.height);
        assert_eq!(info.color_type, png::ColorType::Grayscale);
        assert_eq!(pixels, raster.pixels, "{}", path.display());
    }
}

#[test]
fn a_second_submission_continues_the_sheet_sequence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spooler = FileSpooler::new(dir.path());
    let options = PrintOptions::default();
    let compose = ComposeOptions::default();
    let printer = PrinterId::new("file");
    let rasters = vec![solid(4, 4, 3, 0xff)];

    spooler
        .submit_rasters(&printer, "First", &options, compose, &rasters)
        .expect("first");
    spooler
        .submit_rasters(&printer, "Second", &options, compose, &rasters)
        .expect("second");

    let recorded = spooler.recorded();
    assert_eq!(recorded.len(), 2);
    assert_eq!(
        file_names(&recorded[0].files),
        vec!["sheet-0001.png".to_string()]
    );
    assert_eq!(
        file_names(&recorded[1].files),
        vec!["sheet-0002.png".to_string()],
        "a second job must not overwrite the first job's sheets"
    );
    assert_ne!(recorded[0].id, recorded[1].id);
}

#[test]
fn rgb_sheets_round_trip_through_png() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spooler = FileSpooler::new(dir.path());
    let mut raster = solid(2, 2, 3, 0);
    raster.pixels = vec![
        255, 0, 0, // red
        0, 255, 0, // green
        0, 0, 255, // blue
        255, 255, 255, // white
    ];
    spooler
        .submit_rasters(
            &PrinterId::new("file"),
            "Colours",
            &PrintOptions::default(),
            ComposeOptions::default(),
            std::slice::from_ref(&raster),
        )
        .expect("submit");

    let job = spooler.last_job().expect("job");
    let (info, pixels) = decode_png(&job.files[0]);
    assert_eq!(info.color_type, png::ColorType::Rgb);
    assert_eq!(pixels, raster.pixels);
}

fn file_names(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
        .collect()
}

fn decode_png(path: &std::path::Path) -> (png::OutputInfo, Vec<u8>) {
    let file = std::fs::File::open(path).expect("open png");
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("png header");
    let mut buffer = vec![0u8; reader.output_buffer_size().expect("output size")];
    let info = reader.next_frame(&mut buffer).expect("png frame");
    buffer.truncate(info.buffer_size());
    (info, buffer)
}

// ---------------------------------------------------------------------------
// CUPS: parsers and the option builder
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod cups {
    use lege_pdf_print::spool::cups::{
        LpMode, build_lp_args, parse_job_status, parse_lp_job_id, parse_lpoptions,
        parse_lpoptions_entries, parse_printers,
    };
    use lege_pdf_print::spool::{JobId, JobStatus, PrinterId};
    use lege_pdf_print::{Duplex, Orientation, PageRange, PaperSize, PrintOptions, Scaling};
    use std::path::PathBuf;

    const LPSTAT_E: &str = "Office\nBasement_HP\nPDF\n";

    const LPSTAT_P: &str = "\
printer Basement_HP disabled since Mon 01 Jan 2024 10:00:00 AM CET -
\tOut of paper
printer Office is idle.  enabled since Mon 01 Jan 2024 09:12:44 AM CET
printer PDF is idle.  enabled since Mon 01 Jan 2024 09:12:44 AM CET
";

    const LPSTAT_D: &str = "system default destination: Office\n";

    #[test]
    fn printers_come_from_lpstat_e_with_state_and_default_filled_in() {
        let printers = parse_printers(LPSTAT_E, LPSTAT_P, LPSTAT_D);
        let names: Vec<&str> = printers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(names, ["Office", "Basement_HP", "PDF"]);

        let office = &printers[0];
        assert!(office.is_default);
        assert!(office.accepting_jobs);

        let basement = &printers[1];
        assert!(!basement.is_default);
        assert!(
            !basement.accepting_jobs,
            "a disabled queue is not accepting"
        );
    }

    #[test]
    fn no_default_destination_leaves_every_queue_undefaulted() {
        let printers = parse_printers(LPSTAT_E, LPSTAT_P, "no system default destination\n");
        assert!(printers.iter().all(|p| !p.is_default));
    }

    #[test]
    fn a_localized_lpstat_still_yields_names_and_a_default() {
        // German CUPS. Only the prose is translated; the queue name is the
        // second token and the default line still has one token after its
        // colon, which is all the parsers rely on.
        let lpstat_p = "\
Drucker Office ist im Leerlauf.  Aktiviert seit Mo 01 Jan 2024 09:12:44 CET
Drucker PDF ist im Leerlauf.  Aktiviert seit Mo 01 Jan 2024 09:12:44 CET
";
        let lpstat_d = "Standardziel des Systems: Office\n";
        let printers = parse_printers("", lpstat_p, lpstat_d);
        let names: Vec<&str> = printers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(names, ["Office", "PDF"]);
        assert!(printers[0].is_default);
        // "disabled" never appears, so both read as accepting.
        assert!(printers.iter().all(|p| p.accepting_jobs));
    }

    #[test]
    fn missing_lpstat_is_an_empty_list_not_an_error() {
        assert!(parse_printers("", "", "").is_empty());
    }

    const LPOPTIONS: &str = "\
PageSize/Media Size: *A4 Letter Legal Executive
InputSlot/Media Source: *Auto Tray1 Tray2
Duplex/2-Sided Printing: *None DuplexNoTumble DuplexTumble
ColorModel/Output Mode: *RGB Gray
Resolution/Resolution: 300dpi *600dpi 1200dpi
";

    #[test]
    fn lpoptions_entries_split_keyword_choices_and_current() {
        let entries = parse_lpoptions_entries(LPOPTIONS);
        assert_eq!(entries.len(), 5);
        let duplex = entries
            .iter()
            .find(|e| e.keyword == "Duplex")
            .expect("a Duplex entry");
        assert_eq!(duplex.choices, ["None", "DuplexNoTumble", "DuplexTumble"]);
        assert_eq!(duplex.current.as_deref(), Some("None"));
    }

    #[test]
    fn lpoptions_yields_duplex_colour_and_resolution() {
        let caps = parse_lpoptions(LPOPTIONS);
        assert!(caps.supports_duplex);
        assert!(caps.supports_color);
        assert_eq!(caps.resolution_dpi, Some(600.0));
        assert!(caps.accepts_pdf, "CUPS always takes PDF via its filters");
        // Hardware margins are not in `lpoptions -l`, so the conservative
        // default has to stand.
        assert_eq!(
            caps.hardware_margins,
            lege_pdf_print::paper::Margins::uniform(
                lege_pdf_print::paper::DEFAULT_HARDWARE_MARGIN_PT
            )
        );
    }

    #[test]
    fn a_simplex_mono_queue_reports_neither() {
        let caps =
            parse_lpoptions("Duplex/2-Sided Printing: *None\nColorModel/Output Mode: *Gray\n");
        assert!(!caps.supports_duplex);
        assert!(!caps.supports_color);
        assert_eq!(caps.resolution_dpi, None);
    }

    #[test]
    fn an_ipp_style_queue_is_read_the_same_way() {
        let caps = parse_lpoptions(
            "sides/Sides: *one-sided two-sided-long-edge two-sided-short-edge\n\
             print-color-mode/Color Mode: *color monochrome\n\
             printer-resolution/Resolution: *1200x1200dpi\n",
        );
        assert!(caps.supports_duplex);
        assert!(caps.supports_color);
        assert_eq!(caps.resolution_dpi, Some(1200.0));
    }

    #[test]
    fn lines_without_a_colon_are_not_options() {
        assert!(
            parse_lpoptions_entries("lpoptions: no such printer\n")
                .iter()
                .all(|e| e.keyword != "no")
        );
        assert!(parse_lpoptions_entries("just some prose\n").is_empty());
    }

    #[test]
    fn duplex_two_copies_a4_monochrome_builds_the_expected_argv() {
        let options = PrintOptions {
            paper: PaperSize::A4,
            duplex: Duplex::LongEdge,
            copies: 2,
            collate: true,
            grayscale: true,
            ..PrintOptions::default()
        };
        let args = build_lp_args(
            &PrinterId::new("Office"),
            "Quarterly report",
            &options,
            LpMode::PassThroughPdf,
            &[],
        );
        assert_eq!(
            args,
            [
                "-d",
                "Office",
                "-t",
                "Quarterly report",
                "-n",
                "2",
                "-o",
                "Collate=True",
                "-o",
                "sides=two-sided-long-edge",
                "-o",
                "media=iso_a4_210x297mm",
                "-o",
                "fit-to-page",
                "-o",
                "print-color-mode=monochrome",
            ]
        );
    }

    #[test]
    fn a_single_copy_says_nothing_about_copies_or_collation() {
        let args = build_lp_args(
            &PrinterId::new("Office"),
            "",
            &PrintOptions::default(),
            LpMode::PassThroughPdf,
            &[],
        );
        assert!(!args.iter().any(|a| a == "-n"));
        assert!(!args.iter().any(|a| a.starts_with("Collate=")));
        assert!(!args.iter().any(|a| a == "-t"), "an empty title is omitted");
        assert!(args.iter().any(|a| a == "sides=one-sided"));
    }

    #[test]
    fn page_selection_and_order_only_reach_cups_on_the_pass_through_path() {
        let options = PrintOptions {
            range: PageRange::parse("1,3-5", 10).expect("range"),
            reverse: true,
            orientation: Orientation::Landscape,
            ..PrintOptions::default()
        };

        let pass_through = build_lp_args(
            &PrinterId::new("Office"),
            "Doc",
            &options,
            LpMode::PassThroughPdf,
            &[],
        );
        assert!(pass_through.iter().any(|a| a == "page-ranges=1,3-5"));
        assert!(pass_through.iter().any(|a| a == "outputorder=reverse"));
        assert!(pass_through.iter().any(|a| a == "orientation-requested=4"));

        // Imposition already selected, ordered and turned the pages; saying
        // so again would apply it twice.
        let composed = build_lp_args(
            &PrinterId::new("Office"),
            "Doc",
            &options,
            LpMode::ComposedSheets,
            &[PathBuf::from("/tmp/sheet-0001.png")],
        );
        assert!(!composed.iter().any(|a| a.starts_with("page-ranges=")));
        assert!(!composed.iter().any(|a| a.starts_with("outputorder=")));
        assert!(
            !composed
                .iter()
                .any(|a| a.starts_with("orientation-requested="))
        );
        assert_eq!(
            composed.last().map(String::as_str),
            Some("/tmp/sheet-0001.png")
        );
    }

    #[test]
    fn odd_and_even_become_a_page_set() {
        for (range, expected) in [
            (PageRange::Odd, "page-set=odd"),
            (PageRange::Even, "page-set=even"),
        ] {
            let options = PrintOptions {
                range,
                ..PrintOptions::default()
            };
            let args = build_lp_args(
                &PrinterId::new("Office"),
                "Doc",
                &options,
                LpMode::PassThroughPdf,
                &[],
            );
            assert!(
                args.iter().any(|a| a == expected),
                "{expected} missing from {args:?}"
            );
        }
    }

    #[test]
    fn a_custom_paper_becomes_a_cups_custom_media_name_in_points() {
        let options = PrintOptions {
            paper: PaperSize::Custom {
                width: 300.0,
                height: 500.5,
            },
            ..PrintOptions::default()
        };
        let args = build_lp_args(
            &PrinterId::new("Office"),
            "Doc",
            &options,
            LpMode::PassThroughPdf,
            &[],
        );
        assert!(
            args.iter().any(|a| a == "media=Custom.300x500.5"),
            "{args:?}"
        );
    }

    #[test]
    fn percent_scaling_is_forwarded_only_when_cups_does_the_scaling() {
        let options = PrintOptions {
            scaling: Scaling::Percent(0.5),
            ..PrintOptions::default()
        };
        let pass_through = build_lp_args(
            &PrinterId::new("Office"),
            "Doc",
            &options,
            LpMode::PassThroughPdf,
            &[],
        );
        assert!(
            pass_through.iter().any(|a| a == "scaling=50"),
            "{pass_through:?}"
        );

        let composed = build_lp_args(
            &PrinterId::new("Office"),
            "Doc",
            &options,
            LpMode::ComposedSheets,
            &[],
        );
        assert!(!composed.iter().any(|a| a.starts_with("scaling=")));
        assert!(composed.iter().any(|a| a == "fit-to-page"));
    }

    #[test]
    fn actual_size_leaves_the_filter_chain_alone() {
        let options = PrintOptions {
            scaling: Scaling::ActualSize,
            ..PrintOptions::default()
        };
        let args = build_lp_args(
            &PrinterId::new("Office"),
            "Doc",
            &options,
            LpMode::PassThroughPdf,
            &[],
        );
        assert!(!args.iter().any(|a| a == "fit-to-page"));
        assert!(!args.iter().any(|a| a.starts_with("scaling=")));
    }

    #[test]
    fn the_job_id_is_read_out_of_lps_request_line() {
        assert_eq!(
            parse_lp_job_id("request id is Office-42 (1 file(s))\n"),
            Some(JobId("Office-42".to_string()))
        );
        // A hyphenated queue name keeps its hyphens.
        assert_eq!(
            parse_lp_job_id("request id is HP-LaserJet-1000 (3 file(s))\n"),
            Some(JobId("HP-LaserJet-1000".to_string()))
        );
        // Localized prose around the id changes nothing.
        assert_eq!(
            parse_lp_job_id("Anforderungs-ID ist Office-7 (1 Datei(en))\n"),
            Some(JobId("Office-7".to_string()))
        );
        assert_eq!(parse_lp_job_id(""), None);
    }

    #[test]
    fn a_job_still_listed_is_not_complete() {
        let job = JobId("Office-42".to_string());
        let listed = "Office-42            dk       12288  Mon 01 Jan 2024 09:12:44 AM CET\n";
        assert_eq!(parse_job_status(listed, &job), JobStatus::Processing);
        assert_eq!(parse_job_status("", &job), JobStatus::Completed);
        // Another job's line must not be mistaken for this one.
        assert_eq!(
            parse_job_status("Office-43            dk       12288  Mon\n", &job),
            JobStatus::Completed
        );
    }
}
