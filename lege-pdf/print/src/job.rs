//! The one-call entry point: open a document, decide pass-through versus
//! composition, impose, and hand the result to a spooler.
//!
//! This is the orchestration seam. It owns exactly one decision — which of
//! the two pipelines in `PLAN.md` §2 a job takes — and then delegates: to the
//! spooler for pass-through, and to [`layout::impose`](crate::layout::impose)
//! plus the spooler for composition. Nothing here knows about paper geometry
//! or about any operating system.

use std::fmt;
use std::sync::Arc;

use crate::spool::{DeviceCapabilities, JobId, PrinterId, SpoolJob, SpoolPayload, Spooler};
use crate::{ComposeOptions, PrintError, PrintJob, PrintOptions};

/// The lowest composition resolution worth allowing.
pub const MIN_COMPOSE_DPI: f64 = 72.0;
/// The highest composition resolution worth allowing: above this the driver's
/// own scaling is indistinguishable at arm's length and costs nothing, while
/// the sheet allocation keeps growing quadratically.
pub const MAX_COMPOSE_DPI: f64 = 600.0;

/// Which pipeline a job *would* take.
///
/// [`PrintRoute`] answers "what happened", so it carries the sheet count a
/// submission actually produced. `RouteKind` answers "what would happen",
/// which is decided before any imposition has run — so it deliberately
/// carries no counts rather than reporting a `sheets: 0` that merely means
/// "not computed yet". The two types are kept separate for that reason;
/// [`PrintRoute::kind`] projects one onto the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// The original PDF bytes go to the spooler unmodified.
    PassThrough,
    /// Sheets are composed here.
    Composed,
}

impl RouteKind {
    /// The stable wire name, as the CLI and the GUI report it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PassThrough => "pass_through",
            Self::Composed => "composed",
        }
    }
}

impl fmt::Display for RouteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What `print_document` decided to do, so a caller can report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintRoute {
    /// The original PDF bytes went to the spooler unmodified.
    PassThrough,
    /// Sheets were composed here.
    Composed { sheets: u32 },
}

impl PrintRoute {
    #[must_use]
    pub const fn kind(&self) -> RouteKind {
        match self {
            Self::PassThrough => RouteKind::PassThrough,
            Self::Composed { .. } => RouteKind::Composed,
        }
    }

    /// Sheets composed, or `None` on the pass-through path — where the count
    /// is the printer's business, not ours.
    #[must_use]
    pub const fn sheets(&self) -> Option<u32> {
        match self {
            Self::PassThrough => None,
            Self::Composed { sheets } => Some(*sheets),
        }
    }
}

/// The outcome of a submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedJob {
    pub id: JobId,
    pub route: PrintRoute,
}

/// Everything a submission needs beyond the spooler itself.
///
/// [`print_document`] is the short form for the common case; this is the form
/// that lets a frontend override the composition resolution and pass a
/// document password.
#[derive(Debug)]
pub struct PrintRequest<'a> {
    pub printer: &'a PrinterId,
    /// Job title as it appears in the queue.
    pub title: &'a str,
    /// The document, verbatim. The pass-through path spools exactly these
    /// bytes; the composition path re-opens them.
    pub pdf_bytes: Arc<[u8]>,
    /// Password for an encrypted document. Never logged, never spooled.
    pub password: Option<&'a str>,
    pub options: &'a PrintOptions,
    /// Composition settings. `None` derives them from the device — see
    /// [`compose_options_for`].
    pub compose: Option<ComposeOptions>,
}

/// Decide the route without submitting anything.
///
/// [`PrintOptions::is_pass_through_capable`] is the necessary condition on the
/// job side — it is false as soon as anything changes page *geometry* — and
/// `accepts_pdf` is the necessary condition on the device side. Windows never
/// sets it, which is why Windows is always the raster path.
#[must_use]
pub fn route_for(options: &PrintOptions, capabilities: &DeviceCapabilities) -> RouteKind {
    if options.is_pass_through_capable() && capabilities.accepts_pdf {
        RouteKind::PassThrough
    } else {
        RouteKind::Composed
    }
}

/// Composition settings for a device, when the caller has no opinion.
///
/// The device's own resolution is preferred when it reports one, clamped to
/// [`MIN_COMPOSE_DPI`]..=[`MAX_COMPOSE_DPI`]; a 1200-DPI laser would otherwise
/// ask for a ~390 MB A4 sheet. Grayscale is forced on for a mono device
/// whatever the user asked for, since one byte per pixel is strictly cheaper
/// and the driver would discard the colour anyway.
#[must_use]
pub fn compose_options_for(
    options: &PrintOptions,
    capabilities: &DeviceCapabilities,
) -> ComposeOptions {
    let defaults = ComposeOptions::default();
    let dpi = capabilities
        .resolution_dpi
        .filter(|dpi| dpi.is_finite() && *dpi > 0.0)
        .map_or(defaults.dpi, |dpi| {
            dpi.clamp(MIN_COMPOSE_DPI, MAX_COMPOSE_DPI)
        });
    ComposeOptions {
        dpi,
        grayscale: options.grayscale || !capabilities.supports_color,
        ..defaults
    }
}

/// Print `pdf_bytes` to `printer` under `options`.
///
/// The short form: no password, and composition settings derived from the
/// device. See [`print_document_with`] for the rest.
pub fn print_document(
    spooler: &dyn Spooler,
    printer: &PrinterId,
    title: &str,
    pdf_bytes: Arc<[u8]>,
    options: &PrintOptions,
) -> Result<SubmittedJob, PrintError> {
    print_document_with(
        spooler,
        &PrintRequest {
            printer,
            title,
            pdf_bytes,
            password: None,
            options,
            compose: None,
        },
    )
}

/// Print a document, with explicit composition settings and password.
///
/// Validate, ask the device what it can do, decide the route, and submit. The
/// composition branch opens the document a second time — the pass-through
/// branch never opens it at all, which is the whole point of the branch.
///
/// [`layout::expand_copies`](crate::layout::expand_copies) is deliberately
/// **not** called here. Both platform spoolers take a native copy count and
/// map `copies`/`collate` onto it, so expanding as well would print copies²
/// pages. A backend with no such notion — the `file` one — is the only place
/// expansion belongs, and it is that backend's decision to make.
pub fn print_document_with(
    spooler: &dyn Spooler,
    request: &PrintRequest<'_>,
) -> Result<SubmittedJob, PrintError> {
    request.options.validate()?;
    let capabilities = spooler.capabilities(request.printer)?;

    match route_for(request.options, &capabilities) {
        RouteKind::PassThrough => {
            let id = spooler.submit(SpoolJob {
                printer: request.printer.clone(),
                title: request.title.to_owned(),
                options: request.options,
                payload: SpoolPayload::PassThroughPdf(request.pdf_bytes.as_ref()),
            })?;
            Ok(SubmittedJob {
                id,
                route: PrintRoute::PassThrough,
            })
        }
        RouteKind::Composed => {
            let session = lege_pdf_read::RenderSession::open(
                Arc::clone(&request.pdf_bytes),
                request.password,
            )?;
            let job = PrintJob::from_session(&session, request.options.clone())?;
            let sheets = crate::layout::impose(&job, capabilities.hardware_margins)?;
            let compose = request
                .compose
                .unwrap_or_else(|| compose_options_for(request.options, &capabilities));
            let id = spooler.submit(SpoolJob {
                printer: request.printer.clone(),
                title: request.title.to_owned(),
                options: request.options,
                payload: SpoolPayload::Sheets {
                    session: &session,
                    sheets: &sheets,
                    compose,
                },
            })?;
            Ok(SubmittedJob {
                id,
                route: PrintRoute::Composed {
                    sheets: u32::try_from(sheets.len()).unwrap_or(u32::MAX),
                },
            })
        }
    }
}

/// Read every page's extent out of `pdf_bytes`, touching no spooler.
///
/// The planning half of [`print_document`], split out so a `--dry-run` or a
/// GUI preview can learn the page count and geometry — which it needs before
/// it can resolve a page range — without naming the read crate itself.
pub fn source_pages(
    pdf_bytes: Arc<[u8]>,
    password: Option<&str>,
) -> Result<Vec<crate::SourcePage>, PrintError> {
    let session = lege_pdf_read::RenderSession::open(pdf_bytes, password)?;
    Ok(PrintJob::from_session(&session, PrintOptions::default())?.pages)
}

/// Impose without submitting: what `print_document` would compose. This is
/// what a GUI preview iterates over.
pub fn plan_sheets(
    job: &PrintJob,
    capabilities: &crate::spool::DeviceCapabilities,
) -> Result<Vec<crate::Sheet>, PrintError> {
    crate::layout::impose(job, capabilities.hardware_margins)
}
