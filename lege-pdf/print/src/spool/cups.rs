//! CUPS backend for Linux and macOS, driving `lp` / `lpstat` / `lpoptions`.
//!
//! The CLI rather than `libcups`: no native dependency, no build-time
//! headers, no linking question, and it covers pass-through printing
//! completely.
//!
//! Nothing here ever builds a shell string. Every invocation goes through
//! [`std::process::Command`] with an argument vector, so a printer name or a
//! document title containing `;` is data and not syntax.
//!
//! The parsers are deliberately structural rather than word-matching: CUPS
//! tools are localized, so keying off "printer" or "idle" breaks on a German
//! desktop. Where structure is not enough (a stopped queue) the English word
//! is used as a hint and the tolerant answer is the default.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    DeviceCapabilities, JobId, JobStatus, PrinterId, PrinterInfo, SpoolJob, SpoolPayload, Spooler,
};
use crate::paper::{Margins, PaperSize};
use crate::{Duplex, Orientation, PageRange, PrintError, PrintOptions, Scaling};

/// Drives the CUPS command-line tools.
#[derive(Debug, Default)]
pub struct CupsSpooler {
    _private: (),
}

impl CupsSpooler {
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Spooler for CupsSpooler {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PrintError> {
        // A missing `lpstat` is "no printers", not an error: a headless build
        // machine should be able to ask without handling a failure.
        let names = capture("lpstat", ["-e"]).unwrap_or_default();
        let states = capture("lpstat", ["-p"]).unwrap_or_default();
        let default = capture("lpstat", ["-d"]).unwrap_or_default();
        Ok(parse_printers(&names, &states, &default))
    }

    fn capabilities(&self, printer: &PrinterId) -> Result<DeviceCapabilities, PrintError> {
        let text = capture("lpoptions", ["-p", printer.as_str(), "-l"]).ok_or_else(|| {
            PrintError::NoSuchPrinter(format!(
                "{printer}: lpoptions is unavailable or the queue does not exist"
            ))
        })?;
        Ok(parse_lpoptions(&text))
    }

    fn submit(&self, job: SpoolJob<'_>) -> Result<JobId, PrintError> {
        match job.payload {
            SpoolPayload::PassThroughPdf(bytes) => {
                // `lp` reads the document from stdin when given no file
                // argument, which spares us a temporary file entirely.
                let args = build_lp_args(
                    &job.printer,
                    &job.title,
                    job.options,
                    LpMode::PassThroughPdf,
                    &[],
                );
                let stdout = run_lp(&args, Some(bytes))?;
                job_id_from_lp(&stdout)
            }
            SpoolPayload::Sheets {
                session,
                sheets,
                compose,
            } => {
                // CUPS takes PNG natively through its filter chain, so the
                // composed sheets are staged as images and handed to one `lp`
                // invocation in order.
                let staging = staging_dir()?;
                let result = self.submit_sheets(&job, session, sheets, compose, &staging);
                // `lp` copies the files into the spool directory before it
                // returns, so removing the staging directory now is safe.
                let _ = std::fs::remove_dir_all(&staging);
                result
            }
        }
    }

    fn status(&self, job: &JobId) -> Result<JobStatus, PrintError> {
        let Some(text) = capture("lpstat", ["-W", "not-completed", "-o", job.0.as_str()]) else {
            return Ok(JobStatus::Unknown);
        };
        Ok(parse_job_status(&text, job))
    }

    fn cancel(&self, job: &JobId) -> Result<(), PrintError> {
        let output = Command::new("cancel")
            .arg(&job.0)
            .output()
            .map_err(|e| PrintError::Spool(format!("cancel {job}: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(PrintError::Spool(format!(
                "cancel {job} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

impl CupsSpooler {
    fn submit_sheets(
        &self,
        job: &SpoolJob<'_>,
        session: &lege_pdf_read::RenderSession,
        sheets: &[crate::Sheet],
        compose: crate::ComposeOptions,
        staging: &Path,
    ) -> Result<JobId, PrintError> {
        if sheets.is_empty() {
            return Err(PrintError::EmptyRange);
        }
        let mut files = Vec::with_capacity(sheets.len());
        for (n, sheet) in sheets.iter().enumerate() {
            let raster = crate::compose::compose_sheet(session, sheet, &compose)?;
            let path = staging.join(format!("sheet-{:04}.png", n + 1));
            super::file::write_sheet_png(&path, &raster)?;
            files.push(path);
        }
        let args = build_lp_args(
            &job.printer,
            &job.title,
            job.options,
            LpMode::ComposedSheets,
            &files,
        );
        let stdout = run_lp(&args, None)?;
        job_id_from_lp(&stdout)
    }
}

// ---------------------------------------------------------------------------
// Option mapping
// ---------------------------------------------------------------------------

/// Which pipeline the job took, which changes what `lp` must be told.
///
/// A composed job has already had its pages selected, ordered, oriented and
/// scaled by [`crate::layout::impose`], so re-sending those as CUPS options
/// would apply them twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LpMode {
    /// The original PDF bytes; CUPS does the page work.
    PassThroughPdf,
    /// Rasters we composed; CUPS only places them on paper.
    ComposedSheets,
}

/// The `lp` argument vector for a job, files last.
///
/// Pure, and deliberately exposed: this is the one place where
/// [`PrintOptions`] becomes CUPS vocabulary, and the mapping is worth
/// asserting on without a printer attached.
#[must_use]
pub fn build_lp_args(
    printer: &PrinterId,
    title: &str,
    options: &PrintOptions,
    mode: LpMode,
    files: &[PathBuf],
) -> Vec<String> {
    let pass_through = mode == LpMode::PassThroughPdf;
    let mut args = Vec::new();
    args.push("-d".to_string());
    args.push(printer.as_str().to_string());
    if !title.is_empty() {
        args.push("-t".to_string());
        args.push(title.to_string());
    }

    if options.copies > 1 {
        args.push("-n".to_string());
        args.push(options.copies.to_string());
        // Collation only means anything with more than one copy.
        opt(
            &mut args,
            format!(
                "Collate={}",
                if options.collate { "True" } else { "False" }
            ),
        );
    }

    // Always explicit: the queue's own default may be duplex, and a user who
    // asked for simplex should get it.
    opt(&mut args, format!("sides={}", sides_value(options.duplex)));

    if let Some(media) = media_value(options.paper) {
        opt(&mut args, format!("media={media}"));
    }

    if pass_through {
        // A composed sheet already carries its orientation in the raster;
        // asking CUPS to rotate it again would turn it back.
        match options.orientation {
            Orientation::Landscape => opt(&mut args, "orientation-requested=4".to_string()),
            Orientation::Portrait => opt(&mut args, "orientation-requested=3".to_string()),
            Orientation::Auto => {}
        }
    }

    match (pass_through, options.scaling) {
        // `fit-to-page` is the closest CUPS has to either fitting mode. It
        // will also scale a small page *up*, which `ShrinkToFit` would not —
        // there is no shrink-only CUPS option, and clipping is the worse of
        // the two errors.
        (_, Scaling::FitToPage | Scaling::ShrinkToFit | Scaling::FillPage) => {
            opt(&mut args, "fit-to-page".to_string());
        }
        (true, Scaling::Percent(p)) => {
            opt(&mut args, format!("scaling={}", format_number(p * 100.0)));
        }
        // Actual size is the filter chain's default; saying so would be
        // redundant. A composed sheet is fitted, never percentage-scaled.
        //
        // UNSETTLED, and only a printer can settle it: a composed sheet is
        // already the full paper size with the hardware margin left blank,
        // and CUPS' image filter fits an image to the imageable area by
        // default -- so this may shrink the sheet a second time, by roughly
        // that margin again. `-o ppi=<compose dpi>` or composing only the
        // imageable area would each avoid it. See
        // @lege-ecosystem.question.cups-may-double-apply-the-hardware-margin-to-a-composed-sheet.
        (_, Scaling::ActualSize) | (false, Scaling::Percent(_)) => {
            if !pass_through {
                opt(&mut args, "fit-to-page".to_string());
            }
        }
    }

    if options.grayscale {
        opt(&mut args, "print-color-mode=monochrome".to_string());
    }

    if pass_through {
        match &options.range {
            PageRange::All => {}
            PageRange::Odd => opt(&mut args, "page-set=odd".to_string()),
            PageRange::Even => opt(&mut args, "page-set=even".to_string()),
            PageRange::Spans(spans) => {
                if let Some(value) = page_ranges_value(spans) {
                    opt(&mut args, format!("page-ranges={value}"));
                }
            }
        }
        if options.reverse {
            opt(&mut args, "outputorder=reverse".to_string());
        }
    }

    for file in files {
        args.push(file.display().to_string());
    }
    args
}

/// Push one `-o key=value` pair.
fn opt(args: &mut Vec<String>, value: String) {
    args.push("-o".to_string());
    args.push(value);
}

fn sides_value(duplex: Duplex) -> &'static str {
    match duplex {
        Duplex::None => "one-sided",
        Duplex::LongEdge => "two-sided-long-edge",
        Duplex::ShortEdge => "two-sided-short-edge",
    }
}

/// The `media=` value for a paper size. Named sizes use their IPP name;
/// anything else uses CUPS' `Custom.WIDTHxHEIGHT`, whose default unit is
/// points — the unit this crate works in throughout.
fn media_value(paper: PaperSize) -> Option<String> {
    if let Some(name) = paper.ipp_name() {
        return Some(name.to_string());
    }
    let (w, h) = paper.size();
    Some(format!(
        "Custom.{}x{}",
        format_number(w),
        format_number(h)
    ))
}

/// `page-ranges` takes one-based inclusive spans, `1-3,5`.
fn page_ranges_value(spans: &[(u32, u32)]) -> Option<String> {
    if spans.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(spans.len());
    for &(start, end) in spans {
        let start = start.max(1);
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}-{end}"));
        }
    }
    Some(parts.join(","))
}

/// Two decimals at most, with trailing zeroes trimmed, so `595.2755…` reads
/// as `595.28` and `612` stays `612`.
fn format_number(value: f64) -> String {
    let text = format!("{value:.2}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Build the queue list from `lpstat -e`, `lpstat -p` and `lpstat -d`.
///
/// `lpstat -e` is the authoritative, prose-free list — one destination per
/// line, no localized words at all. `lpstat -p` is only consulted for the
/// enabled/disabled state, and `lpstat -d` for the default queue.
#[must_use]
pub fn parse_printers(lpstat_e: &str, lpstat_p: &str, lpstat_d: &str) -> Vec<PrinterInfo> {
    let mut names = parse_queue_names(lpstat_e);
    if names.is_empty() {
        // No `lpstat -e` (very old CUPS, or the flag was rejected): fall back
        // to the second token of each unindented `lpstat -p` line, which is
        // the queue name in every locale because only the surrounding prose
        // is translated.
        names = lpstat_p
            .lines()
            .filter(|line| !line.starts_with(char::is_whitespace))
            .filter_map(|line| line.split_whitespace().nth(1))
            .map(str::to_string)
            .collect();
        names.dedup();
    }
    let default = parse_default_queue(lpstat_d);
    names
        .into_iter()
        .map(|name| {
            let accepting = queue_is_enabled(lpstat_p, &name);
            PrinterInfo {
                is_default: default.as_deref() == Some(name.as_str()),
                id: PrinterId::new(name),
                // `lpstat -e` carries no description or location, and
                // `lpstat -l -p` prints them behind localized labels. Not
                // worth a second invocation and a guess.
                description: None,
                location: None,
                accepting_jobs: accepting,
            }
        })
        .collect()
}

/// Queue names from `lpstat -e`: one bare destination name per line.
#[must_use]
pub fn parse_queue_names(lpstat_e: &str) -> Vec<String> {
    lpstat_e
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains(char::is_whitespace))
        .map(str::to_string)
        .collect()
}

/// The default destination out of `lpstat -d`.
///
/// The label is localized but the shape is not: a single line, a colon, and
/// exactly one token after it. "no system default destination" has no colon,
/// so it falls out on its own.
#[must_use]
pub fn parse_default_queue(lpstat_d: &str) -> Option<String> {
    lpstat_d.lines().find_map(|line| {
        let (_, tail) = line.rsplit_once(':')?;
        let tail = tail.trim();
        if tail.is_empty() || tail.contains(char::is_whitespace) {
            return None;
        }
        Some(tail.to_string())
    })
}

/// Whether `lpstat -p` reports `name` as enabled.
///
/// Structure cannot answer this one — CUPS prints "disabled since …" only in
/// prose — so the English word is a hint and "enabled" is the fallback. A
/// queue wrongly reported as accepting merely produces a job that waits.
fn queue_is_enabled(lpstat_p: &str, name: &str) -> bool {
    for line in lpstat_p.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let mut tokens = line.split_whitespace();
        if !tokens.any(|token| token == name) {
            continue;
        }
        return !line
            .split_whitespace()
            .any(|token| token.eq_ignore_ascii_case("disabled"));
    }
    true
}

/// One `lpoptions -l` line: a keyword, its human label, and its choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LpOption {
    /// The PPD keyword, e.g. `Duplex`.
    pub keyword: String,
    /// The choices offered, in the order printed.
    pub choices: Vec<String>,
    /// The choice marked current with a leading `*`, if any.
    pub current: Option<String>,
}

/// Parse `lpoptions -p <queue> -l`.
///
/// Each line is `Keyword/Human label: choice *current choice`. Lines without
/// a colon are not options and are skipped.
#[must_use]
pub fn parse_lpoptions_entries(text: &str) -> Vec<LpOption> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once(':') else {
            continue;
        };
        let keyword = left.split('/').next().unwrap_or(left).trim();
        if keyword.is_empty() || keyword.contains(char::is_whitespace) {
            continue;
        }
        let mut choices = Vec::new();
        let mut current = None;
        for token in right.split_whitespace() {
            if let Some(starred) = token.strip_prefix('*') {
                if starred.is_empty() {
                    continue;
                }
                current = Some(starred.to_string());
                choices.push(starred.to_string());
            } else {
                choices.push(token.to_string());
            }
        }
        if choices.is_empty() {
            continue;
        }
        out.push(LpOption {
            keyword: keyword.to_string(),
            choices,
            current,
        });
    }
    out
}

/// Everything `lpoptions -l` can honestly tell us about a device.
///
/// Hardware margins are *not* in this output — PPDs carry them in
/// `ImageableArea`, which `lpoptions -l` does not print — so the conservative
/// [`DEFAULT_HARDWARE_MARGIN_PT`](crate::paper::DEFAULT_HARDWARE_MARGIN_PT)
/// stands. Windows is the one platform that reports the real border.
#[must_use]
pub fn parse_lpoptions(text: &str) -> DeviceCapabilities {
    let entries = parse_lpoptions_entries(text);
    let find = |names: &[&str]| {
        entries
            .iter()
            .find(|e| names.iter().any(|n| e.keyword.eq_ignore_ascii_case(n)))
    };

    let supports_duplex = find(&["Duplex", "sides", "JCLDuplex"]).is_some_and(|entry| {
        entry
            .choices
            .iter()
            .any(|choice| !is_simplex_choice(choice))
    });

    // Absent a colour option, assume colour: that is what `DeviceCapabilities`
    // defaults to and a mono job on a colour queue still prints correctly.
    let supports_color = find(&["ColorModel", "print-color-mode", "ColorMode"])
        .is_none_or(|entry| entry.choices.iter().any(|choice| !is_mono_choice(choice)));

    let resolution_dpi = find(&["Resolution", "printer-resolution"]).and_then(|entry| {
        entry
            .current
            .as_deref()
            .and_then(parse_dpi)
            .or_else(|| entry.choices.iter().filter_map(|c| parse_dpi(c)).fold(None, |acc, dpi| {
                Some(acc.map_or(dpi, |best: f64| best.max(dpi)))
            }))
    });

    DeviceCapabilities {
        hardware_margins: Margins::uniform(crate::paper::DEFAULT_HARDWARE_MARGIN_PT),
        supports_duplex,
        supports_color,
        resolution_dpi,
        // CUPS turns PDF into the printer's language through its own filter
        // chain, on every queue. This is what makes pass-through possible.
        accepts_pdf: true,
    }
}

fn is_simplex_choice(choice: &str) -> bool {
    matches!(
        choice.to_ascii_lowercase().as_str(),
        "none" | "one-sided" | "simplex" | "false" | "off"
    )
}

fn is_mono_choice(choice: &str) -> bool {
    matches!(
        choice.to_ascii_lowercase().as_str(),
        "gray"
            | "grayscale"
            | "greyscale"
            | "mono"
            | "monochrome"
            | "black"
            | "bi-level"
            | "auto-monochrome"
            | "process-monochrome"
            | "kgray"
            | "w"
    )
}

/// `600dpi`, `600x600dpi`, `1200` — take the first number.
fn parse_dpi(choice: &str) -> Option<f64> {
    let digits: String = choice.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<f64>().ok().filter(|d| *d > 0.0)
}

/// The job id out of `lp`'s `request id is Office-42 (1 file(s))`.
///
/// Structural: the last whitespace token shaped `<name>-<digits>`. The
/// surrounding words are localized; the id is not.
#[must_use]
pub fn parse_lp_job_id(stdout: &str) -> Option<JobId> {
    stdout
        .split_whitespace()
        .rfind(|token| {
            let Some((name, number)) = token.rsplit_once('-') else {
                return false;
            };
            !name.is_empty() && !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
        })
        .map(|token| JobId(token.to_string()))
}

/// Read `lpstat -W not-completed -o <jobid>`: a line naming the job means it
/// is still in the queue, nothing means it has left it.
#[must_use]
pub fn parse_job_status(lpstat_o: &str, job: &JobId) -> JobStatus {
    let listed = lpstat_o
        .lines()
        .any(|line| line.split_whitespace().any(|token| token == job.0));
    if listed {
        JobStatus::Processing
    } else {
        JobStatus::Completed
    }
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

/// Run a CUPS tool and return its stdout, or `None` when the tool is missing
/// or exited non-zero. Argument vectors only — never a shell string.
fn capture<I, S>(program: &str, args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

/// Invoke `lp`, optionally feeding the document on stdin.
fn run_lp(args: &[String], stdin_bytes: Option<&[u8]>) -> Result<String, PrintError> {
    use std::io::Write as _;

    let mut command = Command::new("lp");
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if stdin_bytes.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = command
        .spawn()
        .map_err(|e| PrintError::Spool(format!("could not run lp: {e}")))?;
    if let Some(bytes) = stdin_bytes {
        let Some(mut stdin) = child.stdin.take() else {
            return Err(PrintError::Spool("lp stdin was not available".to_string()));
        };
        stdin
            .write_all(bytes)
            .map_err(|e| PrintError::Spool(format!("writing the document to lp failed: {e}")))?;
        drop(stdin);
    }
    let output = child
        .wait_with_output()
        .map_err(|e| PrintError::Spool(format!("waiting for lp failed: {e}")))?;
    if !output.status.success() {
        return Err(PrintError::Spool(format!(
            "lp exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn job_id_from_lp(stdout: &str) -> Result<JobId, PrintError> {
    parse_lp_job_id(stdout).ok_or_else(|| {
        PrintError::Spool(format!(
            "lp accepted the job but printed no request id: {:?}",
            stdout.trim()
        ))
    })
}

/// A private directory under the system temp dir for staging composed sheets.
///
/// The `tempfile` crate is a dev-dependency here and not worth promoting for
/// one directory: `lp` copies its input into the spool area before returning,
/// so the caller removes this directory immediately afterwards.
fn staging_dir() -> Result<PathBuf, PrintError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "lege-print-{}-{nanos}-{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
