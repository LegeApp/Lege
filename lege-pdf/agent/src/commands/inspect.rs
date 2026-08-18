use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_object::PdfObject;
use pdf_read::{CompileStatus, DocumentReport, OpenOutcome};
use pdf_source::PdfSource;

use crate::bounds::Bounds;
use crate::open::{self, DocumentIdentity};
use crate::pages::{PageZero, parse_page_range};
use crate::schema::{Envelope, OutputMode, Status};
use crate::views::report::{
    AnnotationView, CompileView, EncryptionView, FeaturesView, InspectData, MetadataView, OpenView,
    PageView, XrefView,
};

#[derive(Debug)]
pub struct InspectArgs<'a> {
    pub path: &'a Path,
    pub password: Option<&'a str>,
    pub pages: Option<&'a str>,
    pub bounds: Bounds,
    pub output: OutputMode,
    pub snapshot: Option<Arc<DocumentSnapshot>>,
    pub identity: Option<DocumentIdentity>,
}

pub fn run(args: InspectArgs<'_>) -> Result<i32> {
    let (identity, source, snapshot) =
        prepare(args.path, args.password, args.snapshot, args.identity)?;
    let report = pdf_read::examine_with_password(
        Arc::clone(&source) as Arc<dyn PdfSource>,
        DocumentLimits::default(),
        args.password,
    );
    let document = identity.display_path();
    let mut warnings = Vec::new();

    if matches!(report.open, OpenOutcome::Failed { .. }) {
        let data = build_data(None, &report, &[], &mut warnings)?;
        let env = Envelope::ok(&document, serde_json::to_value(&data)?)
            .set_status(Status::Failed)
            .with_warnings(warnings);
        emit(&env, &data, args.output)?;
        return Ok(1);
    }

    let snapshot = match snapshot {
        Some(s) => s,
        // Reuse the bytes we already read above instead of reading the file
        // off disk a second time (the parse itself still runs a second time,
        // since `examine_with_password` above doesn't hand back the
        // `DocumentSnapshot` it built internally).
        None => Arc::new(open::open_from_source(
            &identity,
            Arc::clone(&source),
            args.password,
        )?),
    };

    let (page_indices, range_warnings) =
        parse_page_range(args.pages, snapshot.page_count(), args.bounds.max_pages)?;
    warnings.extend(range_warnings);

    let data = build_data(Some(&snapshot), &report, &page_indices, &mut warnings)?;
    let env = Envelope::ok(&document, serde_json::to_value(&data)?).with_warnings(warnings.clone());
    emit(&env, &data, args.output)?;
    Ok(0)
}

/// Value-only entry for serve.
pub fn inspect_value(
    identity: &DocumentIdentity,
    snapshot: &DocumentSnapshot,
    password: Option<&str>,
    pages: Option<&str>,
    bounds: Bounds,
) -> Result<serde_json::Value> {
    // The cache handed us an already-open snapshot; reuse its source instead
    // of re-reading the file from disk on every `pdf_inspect` call.
    let _ = identity;
    let source: Arc<dyn PdfSource> = Arc::clone(snapshot.source());
    let report = pdf_read::examine_with_password(source, DocumentLimits::default(), password);
    let mut warnings = Vec::new();
    let (page_indices, range_warnings) =
        parse_page_range(pages, snapshot.page_count(), bounds.max_pages)?;
    warnings.extend(range_warnings);
    let data = build_data(Some(snapshot), &report, &page_indices, &mut warnings)?;
    Ok(serde_json::to_value(data)?)
}

fn prepare(
    path: &Path,
    password: Option<&str>,
    snapshot: Option<Arc<DocumentSnapshot>>,
    identity: Option<DocumentIdentity>,
) -> Result<(
    DocumentIdentity,
    Arc<dyn PdfSource>,
    Option<Arc<DocumentSnapshot>>,
)> {
    if let Some(snapshot) = snapshot {
        let identity = identity.unwrap_or_else(|| DocumentIdentity {
            path: path.to_path_buf(),
            len: 0,
            modified: None,
        });
        // Reuse the already-open snapshot's source instead of re-reading the
        // file from disk (`source()` is infallible, so no fallback is
        // needed).
        let source: Arc<dyn PdfSource> = Arc::clone(snapshot.source());
        return Ok((identity, source, Some(snapshot)));
    }
    let (identity, owned) = open::load_source(path)?;
    let _ = password;
    Ok((identity, owned as Arc<dyn PdfSource>, None))
}

fn build_data(
    snapshot: Option<&DocumentSnapshot>,
    report: &DocumentReport,
    page_indices: &[PageZero],
    _warnings: &mut [String],
) -> Result<InspectData> {
    let open = match &report.open {
        OpenOutcome::Ok => OpenView::Ok,
        OpenOutcome::Recovered { how } => OpenView::Recovered {
            repairs: how.clone(),
        },
        OpenOutcome::Failed { error } => OpenView::Failed {
            error: error.clone(),
        },
    };

    let (page_count, version, outline_item_count, metadata, pages) =
        if let Some(snapshot) = snapshot {
            let version = {
                let v = snapshot.structure().version;
                Some(format!("{}.{}", v.major, v.minor))
            };
            let mut ctx = ParseContext::new();
            let outline = snapshot.outline(&mut ctx);
            let metadata = extract_metadata(snapshot, &mut ctx);
            let status_by: std::collections::HashMap<u32, &pdf_read::PageStatus> =
                report.pages.iter().map(|p| (p.index, p)).collect();
            let mut pages = Vec::new();
            for pz in page_indices {
                let pref = snapshot.page(PageIndex(pz.0))?;
                let status = status_by.get(&pz.0).copied();
                let compile = match status.map(|s| &s.compile) {
                    Some(CompileStatus::Ok { op_count }) => CompileView::Ok {
                        op_count: *op_count,
                    },
                    Some(CompileStatus::Degraded { op_count, detail }) => CompileView::Degraded {
                        op_count: *op_count,
                        detail: detail.clone(),
                    },
                    Some(CompileStatus::Failed { error }) => CompileView::Failed {
                        error: error.clone(),
                    },
                    None => CompileView::Unknown,
                };
                pages.push(PageView {
                    page: pz.one_based(),
                    page_index: pz.0,
                    media_box: pref.media_box,
                    crop_box: pref.crop_box,
                    rotate: pref.rotate,
                    media_box_present: status.map(|s| s.media_box_present).unwrap_or(true),
                    compile,
                });
            }
            (
                snapshot.page_count(),
                version,
                outline.items.len(),
                metadata,
                pages,
            )
        } else {
            (0, None, 0, MetadataView::default(), Vec::new())
        };

    Ok(InspectData {
        open,
        page_count,
        version,
        encryption: report.encryption.as_ref().map(|e| EncryptionView {
            version: e.version,
            revision: e.revision,
            method: e.method.clone(),
            user_password_empty: e.user_password_empty,
        }),
        xref: XrefView {
            recovery_used: report.xref.recovery_used,
            rebuilt: report.xref.rebuilt,
            revision_count: report.xref.revision_count,
        },
        features: FeaturesView {
            has_acroform: report.features.has_acroform,
            has_xfa: report.features.has_xfa,
            has_javascript: report.features.has_javascript,
            has_outlines: report.features.has_outlines,
            has_optional_content: report.features.has_optional_content,
            uses_object_streams: report.features.uses_object_streams,
        },
        annotations: AnnotationView {
            total: report.annotations.total(),
            by_subtype: report
                .annotations
                .count_per_subtype
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        },
        outline_item_count,
        metadata,
        pages,
    })
}

fn extract_metadata(snapshot: &DocumentSnapshot, ctx: &mut ParseContext) -> MetadataView {
    let Some(info_id) = snapshot.structure().trailer.info else {
        return MetadataView::default();
    };
    let Ok(info) = snapshot.objects().resolve(snapshot, info_id, ctx) else {
        return MetadataView::default();
    };
    let Some(dict) = info.as_dict() else {
        return MetadataView::default();
    };
    let names = snapshot.names();
    let mut read_string = |key: &[u8]| -> Option<String> {
        let value = dict.get(names.intern(key))?;
        let obj: Arc<PdfObject> = match value {
            PdfObject::Reference(id) => snapshot.objects().resolve(snapshot, *id, ctx).ok()?,
            other => Arc::new(other.clone()),
        };
        match &*obj {
            PdfObject::String(text) => Some(pdf_object::decode_text_string_lossy(text.as_bytes())),
            _ => None,
        }
    };
    MetadataView {
        title: read_string(b"Title"),
        author: read_string(b"Author"),
    }
}

fn emit(env: &Envelope, data: &InspectData, mode: OutputMode) -> Result<()> {
    match mode {
        OutputMode::Human => {
            print_human(env, data);
            Ok(())
        }
        OutputMode::Json => env.write_json(),
        OutputMode::Jsonl => env.write_jsonl(),
    }
}

fn print_human(env: &Envelope, data: &InspectData) {
    println!("document: {}", env.document);
    match &data.open {
        OpenView::Ok => println!("open: ok"),
        OpenView::Recovered { repairs } => {
            println!("open: recovered ({} repair(s))", repairs.len());
            for r in repairs {
                println!("  repair: {r}");
            }
        }
        OpenView::Failed { error } => println!("open: failed: {error}"),
    }
    println!("pages: {}", data.page_count);
    if let Some(v) = &data.version {
        println!("version: {v}");
    }
    if let Some(t) = &data.metadata.title {
        println!("title: {t}");
    }
    if let Some(a) = &data.metadata.author {
        println!("author: {a}");
    }
    if let Some(enc) = &data.encryption {
        println!(
            "encryption: {} (V={} R={}) empty_user={}",
            enc.method, enc.version, enc.revision, enc.user_password_empty
        );
    }
    println!(
        "features: acroform={} xfa={} js={} outlines={} ocg={} objstm={}",
        data.features.has_acroform,
        data.features.has_xfa,
        data.features.has_javascript,
        data.features.has_outlines,
        data.features.has_optional_content,
        data.features.uses_object_streams
    );
    println!("outline_items: {}", data.outline_item_count);
    println!("annotations: {}", data.annotations.total);
    println!(
        "xref: recovery={} rebuilt={} revisions={}",
        data.xref.recovery_used, data.xref.rebuilt, data.xref.revision_count
    );
    for page in &data.pages {
        let compile = match &page.compile {
            CompileView::Ok { op_count } => format!("ok ops={op_count}"),
            CompileView::Degraded { op_count, detail } => {
                format!("degraded ops={op_count} ({detail})")
            }
            CompileView::Failed { error } => format!("failed ({error})"),
            CompileView::Unknown => "unknown".into(),
        };
        println!(
            "page {} index {} rotate {} crop={:?} compile={compile}",
            page.page, page.page_index, page.rotate, page.crop_box
        );
    }
    for w in &env.warnings {
        eprintln!("warning: {w}");
    }
}
