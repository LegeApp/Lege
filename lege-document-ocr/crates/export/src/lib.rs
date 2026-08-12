//! Deterministic exporters over `lege-docir`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use lege_docir::{Document, PageSourceKind, RegionContent, TextView};
use lege_pdf_write::artifact::{
    ColorModel, PdfImageElement, PdfImageResource, PdfPageArtifact, PreparedTextLayer, TextFont,
    TextRun,
};
use lege_pdf_write::types::{Affine, PdfRect};
use lege_pdf_write::writer::DocumentWriter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExportFormat {
    Json,
    Text,
    Html,
    SearchablePdf,
    Docx,
    Alto,
    PageXml,
    Markdown,
    PdfA,
    Latex,
    Xlsx,
    Csv,
    Hocr,
}

impl FromStr for ExportFormat {
    type Err = ExportError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "json" => Ok(Self::Json),
            "text" | "txt" => Ok(Self::Text),
            "html" => Ok(Self::Html),
            "searchable-pdf" | "pdf" => Ok(Self::SearchablePdf),
            "docx" => Ok(Self::Docx),
            "alto" => Ok(Self::Alto),
            "page" | "page-xml" => Ok(Self::PageXml),
            "markdown" | "md" => Ok(Self::Markdown),
            "pdfa" | "pdf-a" => Ok(Self::PdfA),
            "latex" | "tex" => Ok(Self::Latex),
            "xlsx" => Ok(Self::Xlsx),
            "csv" => Ok(Self::Csv),
            "hocr" => Ok(Self::Hocr),
            other => Err(ExportError::UnknownFormat(other.to_string())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportRequest<'a> {
    pub document: &'a Document,
    pub output_dir: &'a Path,
    pub stem: &'a str,
    pub text_view: TextView,
    pub overwrite: bool,
    pub searchable_pdf_policy: SearchablePdfPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchablePdfPolicy {
    /// Preserve the original PDF bytes. Currently supported when every page
    /// already has trustworthy native text; scanned pages fail explicitly.
    PreserveSource,
    /// Render each page and create a new JPEG-backed PDF with an invisible text
    /// layer. This is opt-in because it changes the source representation.
    Rasterize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedArtifact {
    pub format: ExportFormat,
    pub path: PathBuf,
}

pub trait DocumentExporter: Send + Sync {
    fn format(&self) -> ExportFormat;
    fn export(&self, request: &ExportRequest<'_>) -> Result<ExportedArtifact, ExportError>;
}

pub fn exporter(format: ExportFormat) -> Result<Box<dyn DocumentExporter>, ExportError> {
    match format {
        ExportFormat::Json => Ok(Box::new(JsonExporter)),
        ExportFormat::Text => Ok(Box::new(TextExporter)),
        ExportFormat::Html => Ok(Box::new(HtmlExporter)),
        ExportFormat::SearchablePdf => Ok(Box::new(SearchablePdfExporter)),
        ExportFormat::Docx => Ok(Box::new(DocxExporter)),
        ExportFormat::Alto => Ok(Box::new(TextMarkupExporter(ExportFormat::Alto))),
        ExportFormat::PageXml => Ok(Box::new(TextMarkupExporter(ExportFormat::PageXml))),
        ExportFormat::Markdown => Ok(Box::new(TextMarkupExporter(ExportFormat::Markdown))),
        ExportFormat::Latex => Ok(Box::new(TextMarkupExporter(ExportFormat::Latex))),
        ExportFormat::Csv => Ok(Box::new(TextMarkupExporter(ExportFormat::Csv))),
        ExportFormat::Hocr => Ok(Box::new(TextMarkupExporter(ExportFormat::Hocr))),
        ExportFormat::Xlsx => Ok(Box::new(XlsxExporter)),
        other => Err(ExportError::NotImplemented(other)),
    }
}

pub fn export_all(
    request: &ExportRequest<'_>,
    formats: &[ExportFormat],
) -> Result<Vec<ExportedArtifact>, ExportError> {
    request
        .document
        .validate()
        .map_err(|error| ExportError::InvalidDocument(error.to_string()))?;
    formats
        .iter()
        .map(|format| exporter(*format)?.export(request))
        .collect()
}

struct JsonExporter;
impl DocumentExporter for JsonExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::Json
    }
    fn export(&self, request: &ExportRequest<'_>) -> Result<ExportedArtifact, ExportError> {
        let path = request
            .output_dir
            .join(format!("{}.lege.json", request.stem));
        let bytes = serde_json::to_vec_pretty(request.document)?;
        atomic_write(&path, &bytes, request.overwrite)?;
        let manifest_path = request
            .output_dir
            .join(format!("{}.processing-manifest.json", request.stem));
        atomic_write(
            &manifest_path,
            &serde_json::to_vec_pretty(&request.document.processing)?,
            request.overwrite,
        )?;
        let qa_path = request.output_dir.join(format!("{}.qa.json", request.stem));
        atomic_write(
            &qa_path,
            &serde_json::to_vec_pretty(&qa_report(request.document))?,
            request.overwrite,
        )?;
        Ok(ExportedArtifact {
            format: self.format(),
            path,
        })
    }
}

fn qa_report(document: &Document) -> serde_json::Value {
    let mut review = Vec::new();
    let mut applied_corrections = 0_u64;
    let mut suggested_corrections = 0_u64;
    for page in &document.pages {
        for region in &page.regions {
            if region
                .confidence
                .recognition
                .is_some_and(|value| value < 0.72)
            {
                review.push(serde_json::json!({
                    "page": page.index + 1,
                    "region_id": region.id,
                    "recognition_confidence": region.confidence.recognition,
                    "reason": "low-recognition-confidence"
                }));
            }
            match &region.content {
                RegionContent::Text(block) => count_block_corrections(
                    block,
                    &mut suggested_corrections,
                    &mut applied_corrections,
                ),
                RegionContent::Table(table) => {
                    for block in table.cells.iter().flat_map(|cell| &cell.blocks) {
                        count_block_corrections(
                            block,
                            &mut suggested_corrections,
                            &mut applied_corrections,
                        );
                    }
                }
                RegionContent::Formula(formula) => {
                    if let Some(block) = &formula.raw_ocr {
                        count_block_corrections(
                            block,
                            &mut suggested_corrections,
                            &mut applied_corrections,
                        );
                    }
                }
                _ => {}
            }
        }
    }
    serde_json::json!({
        "schema": "lege.ocr.qa",
        "version": 1,
        "document_id": document.id,
        "pages": document.pages.len(),
        "review_count": review.len(),
        "review": review,
        "corrections": {
            "suggested": suggested_corrections,
            "applied": applied_corrections
        },
        "warnings": document.processing.warnings,
    })
}

fn count_block_corrections(block: &lege_docir::TextBlock, suggested: &mut u64, applied: &mut u64) {
    for line in &block.lines {
        *suggested += line.text.corrections.len() as u64;
        *applied += line
            .text
            .corrections
            .iter()
            .filter(|correction| correction.applied)
            .count() as u64;
    }
}

struct TextExporter;
impl DocumentExporter for TextExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::Text
    }
    fn export(&self, request: &ExportRequest<'_>) -> Result<ExportedArtifact, ExportError> {
        let path = request.output_dir.join(format!("{}.txt", request.stem));
        atomic_write(
            &path,
            request.document.full_text(request.text_view).as_bytes(),
            request.overwrite,
        )?;
        Ok(ExportedArtifact {
            format: self.format(),
            path,
        })
    }
}

struct HtmlExporter;
impl DocumentExporter for HtmlExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::Html
    }
    fn export(&self, request: &ExportRequest<'_>) -> Result<ExportedArtifact, ExportError> {
        let directory = request.output_dir.join(format!("{}.html", request.stem));
        fs::create_dir_all(&directory)?;
        let path = directory.join("index.html");
        let title = request
            .document
            .metadata
            .title
            .as_deref()
            .unwrap_or(request.stem);
        let mut html = format!(
            "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>body{{max-width:72rem;margin:auto;padding:2rem;font:18px/1.55 system-ui}}section.page{{border-top:1px solid #bbb;margin-top:2rem;padding-top:1rem}}table{{border-collapse:collapse}}td,th{{border:1px solid #888;padding:.25rem}}</style></head><body><h1>{}</h1>\n",
            html_escape::encode_text(title),
            html_escape::encode_text(title)
        );
        for page in &request.document.pages {
            html.push_str(&format!(
                "<section class=\"page\" id=\"page-{}\" data-page=\"{}\"><h2>Page {}</h2>\n",
                page.index + 1,
                page.index + 1,
                page.index + 1
            ));
            for region_id in &page.reading_order {
                if let Some(region) = page.regions.iter().find(|region| &region.id == region_id) {
                    render_region(&mut html, &region.content, request.text_view);
                }
            }
            html.push_str("</section>\n");
        }
        html.push_str("</body></html>\n");
        atomic_write(&path, html.as_bytes(), request.overwrite)?;
        Ok(ExportedArtifact {
            format: self.format(),
            path: directory,
        })
    }
}

struct SearchablePdfExporter;
impl DocumentExporter for SearchablePdfExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::SearchablePdf
    }

    fn export(&self, request: &ExportRequest<'_>) -> Result<ExportedArtifact, ExportError> {
        let path = request
            .output_dir
            .join(format!("{}.searchable.pdf", request.stem));
        let all_native = request.document.pages.iter().all(|page| {
            matches!(
                page.source_kind,
                PageSourceKind::NativeText | PageSourceKind::Hybrid
            )
        });
        match request.searchable_pdf_policy {
            SearchablePdfPolicy::PreserveSource if all_native => {
                let bytes = fs::read(&request.document.source.path)?;
                atomic_write(&path, &bytes, request.overwrite)?;
            }
            SearchablePdfPolicy::PreserveSource => {
                return Err(ExportError::PreservingPdfOverlayUnsupported);
            }
            SearchablePdfPolicy::Rasterize => rasterized_searchable_pdf(request, &path)?,
        }
        Ok(ExportedArtifact {
            format: self.format(),
            path,
        })
    }
}

fn rasterized_searchable_pdf(request: &ExportRequest<'_>, path: &Path) -> Result<(), ExportError> {
    if path.exists() && !request.overwrite {
        return Err(ExportError::Exists(path.to_path_buf()));
    }
    let source: Arc<[u8]> = fs::read(&request.document.source.path)?.into();
    let session = lege_pdf_read::RenderSession::open(source, None)?;
    if session.page_count() as usize != request.document.pages.len() {
        return Err(ExportError::SourcePageCountChanged);
    }
    let parent = path
        .parent()
        .ok_or_else(|| ExportError::InvalidPath(path.to_path_buf()))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer =
            DocumentWriter::new(temporary.as_file_mut(), request.document.pages.len())?;
        for page in &request.document.pages {
            let compiled = session.compile(page.index)?;
            let rendered = session.render(
                &compiled,
                &lege_pdf_read::RasterProduct::rgb8(
                    page.source_size.width,
                    page.source_size.height,
                ),
            )?;
            let lege_pdf_read::RasterPlane::Rgb8(rgb) = rendered else {
                return Err(ExportError::UnexpectedRasterFormat);
            };
            let mut jpeg = Vec::new();
            let width = u16::try_from(rgb.width).map_err(|_| ExportError::JpegDimension)?;
            let height = u16::try_from(rgb.height).map_err(|_| ExportError::JpegDimension)?;
            let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, 90);
            encoder.set_optimized_huffman_tables(true);
            encoder
                .encode(&rgb.pixels, width, height, jpeg_encoder::ColorType::Rgb)
                .map_err(|error| ExportError::Jpeg(error.to_string()))?;
            let artifact = PdfPageArtifact {
                index: page.index,
                media_box: PdfRect::from_size(
                    page.page_size_points.width,
                    page.page_size_points.height,
                ),
                elements: Box::new([PdfImageElement {
                    transform: Affine::scale_translate(
                        page.page_size_points.width,
                        page.page_size_points.height,
                        0.0,
                        0.0,
                    ),
                    image: PdfImageResource::Jpeg {
                        data: Arc::from(jpeg),
                        width: rgb.width,
                        height: rgb.height,
                        color: ColorModel::Rgb,
                    },
                }]),
                text_layer: page_text_layer(page, request.text_view),
            };
            writer.add_page(&artifact)?;
        }
        let _ = writer.finalize()?;
    }
    temporary.as_file().sync_all()?;
    if request.overwrite && path.exists() {
        fs::remove_file(path)?;
    }
    temporary
        .persist(path)
        .map_err(|error| ExportError::Io(error.error))?;
    Ok(())
}

fn page_text_layer(page: &lege_docir::Page, view: TextView) -> Option<PreparedTextLayer> {
    let scale_x = page.page_size_points.width / f64::from(page.source_size.width.max(1));
    let scale_y = page.page_size_points.height / f64::from(page.source_size.height.max(1));
    let mut runs = Vec::new();
    for region in &page.regions {
        let RegionContent::Text(block) = &region.content else {
            continue;
        };
        for line in &block.lines {
            if line.words.is_empty() {
                if let Some((left, top, _, bottom)) = polygon_bounds(&line.polygon) {
                    runs.push(TextRun {
                        text: line.text.select(view),
                        x: f64::from(left) * scale_x,
                        y: page.page_size_points.height - f64::from(bottom) * scale_y,
                        size: (f64::from(bottom - top) * scale_y).max(1.0),
                    });
                }
            } else {
                for (index, word) in line.words.iter().enumerate() {
                    let Some((left, top, _, bottom)) = polygon_bounds(&word.polygon) else {
                        continue;
                    };
                    let mut text = word.text.select(view);
                    if index + 1 < line.words.len() {
                        text.push(' ');
                    }
                    runs.push(TextRun {
                        text,
                        x: f64::from(left) * scale_x,
                        y: page.page_size_points.height - f64::from(bottom) * scale_y,
                        size: (f64::from(bottom - top) * scale_y).max(1.0),
                    });
                }
            }
        }
    }
    (!runs.is_empty()).then_some(PreparedTextLayer {
        runs: runs.into_boxed_slice(),
        // The current product profile is English. A future writer-owned
        // glyphless font will lift this fallback's Windows-1252 limitation.
        font: TextFont::HelveticaFallback,
    })
}

fn polygon_bounds(polygon: &[lege_docir::Point]) -> Option<(f32, f32, f32, f32)> {
    let first = polygon.first()?;
    Some(polygon.iter().skip(1).fold(
        (first.x, first.y, first.x, first.y),
        |(left, top, right, bottom), point| {
            (
                left.min(point.x),
                top.min(point.y),
                right.max(point.x),
                bottom.max(point.y),
            )
        },
    ))
}

struct TextMarkupExporter(ExportFormat);
impl DocumentExporter for TextMarkupExporter {
    fn format(&self) -> ExportFormat {
        self.0
    }

    fn export(&self, request: &ExportRequest<'_>) -> Result<ExportedArtifact, ExportError> {
        let (extension, body) = match self.0 {
            ExportFormat::Markdown => ("md", render_markdown(request)),
            ExportFormat::Latex => ("tex", render_latex(request)),
            ExportFormat::Alto => ("alto.xml", render_alto(request)),
            ExportFormat::PageXml => ("page.xml", render_page_xml(request)),
            ExportFormat::Hocr => ("hocr.html", render_hocr(request)),
            ExportFormat::Csv => ("tables.csv", render_csv(request)),
            _ => return Err(ExportError::NotImplemented(self.0)),
        };
        let path = request
            .output_dir
            .join(format!("{}.{}", request.stem, extension));
        atomic_write(&path, body.as_bytes(), request.overwrite)?;
        Ok(ExportedArtifact {
            format: self.0,
            path,
        })
    }
}

fn render_markdown(request: &ExportRequest<'_>) -> String {
    let mut output = format!(
        "# {}\n\n",
        request
            .document
            .metadata
            .title
            .as_deref()
            .unwrap_or(request.stem)
    );
    for page in &request.document.pages {
        output.push_str(&format!("<!-- page {} -->\n\n", page.index + 1));
        for region_id in &page.reading_order {
            let Some(region) = page.regions.iter().find(|region| &region.id == region_id) else {
                continue;
            };
            let text = region
                .content
                .plain_text(request.text_view)
                .unwrap_or_default();
            if text.trim().is_empty() {
                continue;
            }
            match region.kind {
                lege_docir::RegionKind::Title => output.push_str(&format!("# {text}\n\n")),
                lege_docir::RegionKind::Heading => output.push_str(&format!("## {text}\n\n")),
                _ => output.push_str(&format!("{text}\n\n")),
            }
        }
    }
    output
}

fn render_latex(request: &ExportRequest<'_>) -> String {
    let mut output =
        String::from("\\documentclass{article}\n\\usepackage[utf8]{inputenc}\n\\begin{document}\n");
    for page in &request.document.pages {
        if page.index > 0 {
            output.push_str("\\newpage\n");
        }
        let text = page.plain_text(request.text_view);
        output.push_str(&latex_escape(&text).replace('\n', "\\\\\n"));
        output.push('\n');
    }
    output.push_str("\\end{document}\n");
    output
}

fn latex_escape(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\\' => "\\textbackslash{}".to_string(),
            '&' => "\\&".to_string(),
            '%' => "\\%".to_string(),
            '$' => "\\$".to_string(),
            '#' => "\\#".to_string(),
            '_' => "\\_".to_string(),
            '{' => "\\{".to_string(),
            '}' => "\\}".to_string(),
            '~' => "\\textasciitilde{}".to_string(),
            '^' => "\\textasciicircum{}".to_string(),
            other => other.to_string(),
        })
        .collect()
}

fn render_alto(request: &ExportRequest<'_>) -> String {
    let mut output = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<alto xmlns=\"http://www.loc.gov/standards/alto/ns-v4#\"><Layout>\n",
    );
    for page in &request.document.pages {
        output.push_str(&format!(
            "<Page ID=\"p{}\" WIDTH=\"{}\" HEIGHT=\"{}\"><PrintSpace>",
            page.index + 1,
            page.source_size.width,
            page.source_size.height
        ));
        for region in &page.regions {
            let Some((x0, y0, x1, y1)) = polygon_bounds(&region.polygon) else {
                continue;
            };
            output.push_str(&format!("<TextBlock ID=\"{}\" HPOS=\"{x0:.0}\" VPOS=\"{y0:.0}\" WIDTH=\"{:.0}\" HEIGHT=\"{:.0}\">", xml_attr(&region.id), x1-x0, y1-y0));
            if let RegionContent::Text(block) = &region.content {
                for line in &block.lines {
                    let Some((lx0, ly0, lx1, ly1)) = polygon_bounds(&line.polygon) else {
                        continue;
                    };
                    output.push_str(&format!("<TextLine HPOS=\"{lx0:.0}\" VPOS=\"{ly0:.0}\" WIDTH=\"{:.0}\" HEIGHT=\"{:.0}\">", lx1-lx0, ly1-ly0));
                    for word in &line.words {
                        let Some((wx0, wy0, wx1, wy1)) = polygon_bounds(&word.polygon) else {
                            continue;
                        };
                        output.push_str(&format!("<String CONTENT=\"{}\" HPOS=\"{wx0:.0}\" VPOS=\"{wy0:.0}\" WIDTH=\"{:.0}\" HEIGHT=\"{:.0}\"/>", xml_attr(&word.text.select(request.text_view)), wx1-wx0, wy1-wy0));
                    }
                    output.push_str("</TextLine>");
                }
            }
            output.push_str("</TextBlock>");
        }
        output.push_str("</PrintSpace></Page>\n");
    }
    output.push_str("</Layout></alto>\n");
    output
}

fn render_page_xml(request: &ExportRequest<'_>) -> String {
    let mut output = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<PcGts xmlns=\"http://schema.primaresearch.org/PAGE/gts/pagecontent/2019-07-15\">\n",
    );
    for page in &request.document.pages {
        output.push_str(&format!(
            "<Page imageFilename=\"{}\" imageWidth=\"{}\" imageHeight=\"{}\">",
            xml_attr(&request.document.source.path),
            page.source_size.width,
            page.source_size.height
        ));
        for region in &page.regions {
            let points = region
                .polygon
                .iter()
                .map(|p| format!("{:.0},{:.0}", p.x, p.y))
                .collect::<Vec<_>>()
                .join(" ");
            output.push_str(&format!("<TextRegion id=\"{}\"><Coords points=\"{points}\"/><TextEquiv><Unicode>{}</Unicode></TextEquiv></TextRegion>", xml_attr(&region.id), xml_text(&region.content.plain_text(request.text_view).unwrap_or_default())));
        }
        output.push_str("</Page>\n");
    }
    output.push_str("</PcGts>\n");
    output
}

fn render_hocr(request: &ExportRequest<'_>) -> String {
    let mut output = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>hOCR</title></head><body>\n",
    );
    for page in &request.document.pages {
        output.push_str(&format!(
            "<div class=\"ocr_page\" id=\"page_{}\" title=\"bbox 0 0 {} {}\">",
            page.index + 1,
            page.source_size.width,
            page.source_size.height
        ));
        for region in &page.regions {
            if let RegionContent::Text(block) = &region.content {
                for (line_index, line) in block.lines.iter().enumerate() {
                    let Some((x0, y0, x1, y1)) = polygon_bounds(&line.polygon) else {
                        continue;
                    };
                    output.push_str(&format!("<span class=\"ocr_line\" id=\"{}_{}\" title=\"bbox {x0:.0} {y0:.0} {x1:.0} {y1:.0}\">{}</span>\n", xml_attr(&region.id), line_index, xml_text(&line.text.select(request.text_view))));
                }
            }
        }
        output.push_str("</div>\n");
    }
    output.push_str("</body></html>\n");
    output
}

fn render_csv(request: &ExportRequest<'_>) -> String {
    let mut rows = vec!["page,table,row,column,row_span,column_span,text".to_string()];
    for page in &request.document.pages {
        for region in &page.regions {
            if let RegionContent::Table(table) = &region.content {
                for cell in &table.cells {
                    let text = cell
                        .blocks
                        .iter()
                        .map(|b| b.plain_text(request.text_view))
                        .collect::<Vec<_>>()
                        .join(" ");
                    rows.push(format!(
                        "{},{},{},{},{},{},{}",
                        page.index + 1,
                        csv_field(&region.id),
                        cell.row,
                        cell.column,
                        cell.row_span,
                        cell.column_span,
                        csv_field(&text)
                    ));
                }
            }
        }
    }
    rows.join("\r\n") + "\r\n"
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn xml_text(value: &str) -> String {
    html_escape::encode_text(value).into_owned()
}
fn xml_attr(value: &str) -> String {
    html_escape::encode_double_quoted_attribute(value).into_owned()
}

struct DocxExporter;
impl DocumentExporter for DocxExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::Docx
    }
    fn export(&self, request: &ExportRequest<'_>) -> Result<ExportedArtifact, ExportError> {
        let path = request.output_dir.join(format!("{}.docx", request.stem));
        let mut body = String::new();
        for (page_index, page) in request.document.pages.iter().enumerate() {
            for region_id in &page.reading_order {
                let Some(region) = page.regions.iter().find(|region| &region.id == region_id)
                else {
                    continue;
                };
                match &region.content {
                    RegionContent::Text(block) => {
                        let style = match region.kind {
                            lege_docir::RegionKind::Title => Some("Title"),
                            lege_docir::RegionKind::Heading => Some("Heading1"),
                            lege_docir::RegionKind::Caption => Some("Caption"),
                            _ => None,
                        };
                        for line in &block.lines {
                            docx_paragraph(&mut body, &line.text.select(request.text_view), style);
                        }
                    }
                    RegionContent::Table(table) => docx_table(&mut body, table, request.text_view),
                    RegionContent::Formula(formula) => {
                        let value = formula
                            .latex
                            .as_deref()
                            .or(formula.mathml.as_deref())
                            .unwrap_or_default();
                        docx_paragraph(&mut body, value, None);
                    }
                    RegionContent::Figure(figure) => {
                        if let Some(caption) = &figure.caption {
                            docx_paragraph(
                                &mut body,
                                &caption.plain_text(request.text_view),
                                Some("Caption"),
                            );
                        }
                    }
                    RegionContent::Separator | RegionContent::Unknown => {}
                }
            }
            if page_index + 1 < request.document.pages.len() {
                body.push_str("<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>");
            }
        }
        let document = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body>{body}<w:sectPr/></w:body></w:document>"
        );
        write_zip_atomic(
            &path,
            request.overwrite,
            &[
                (
                    "[Content_Types].xml",
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>",
                ),
                (
                    "_rels/.rels",
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>",
                ),
                ("word/document.xml", &document),
            ],
        )?;
        Ok(ExportedArtifact {
            format: self.format(),
            path,
        })
    }
}

fn docx_paragraph(output: &mut String, text: &str, style: Option<&str>) {
    output.push_str("<w:p>");
    if let Some(style) = style {
        output.push_str(&format!("<w:pPr><w:pStyle w:val=\"{style}\"/></w:pPr>"));
    }
    output.push_str(&format!(
        "<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
        xml_text(text)
    ));
}

fn docx_table(output: &mut String, table: &lege_docir::Table, view: TextView) {
    output.push_str("<w:tbl>");
    for row in 0..table.rows {
        output.push_str("<w:tr>");
        for column in 0..table.columns {
            output.push_str("<w:tc>");
            if let Some(cell) = table
                .cells
                .iter()
                .find(|cell| cell.row == row && cell.column == column)
            {
                if cell.column_span > 1 {
                    output.push_str(&format!(
                        "<w:tcPr><w:gridSpan w:val=\"{}\"/></w:tcPr>",
                        cell.column_span
                    ));
                }
                let text = cell
                    .blocks
                    .iter()
                    .map(|block| block.plain_text(view))
                    .collect::<Vec<_>>()
                    .join(" ");
                docx_paragraph(output, &text, None);
            } else {
                output.push_str("<w:p/>");
            }
            output.push_str("</w:tc>");
        }
        output.push_str("</w:tr>");
    }
    output.push_str("</w:tbl>");
}

struct XlsxExporter;
impl DocumentExporter for XlsxExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::Xlsx
    }
    fn export(&self, request: &ExportRequest<'_>) -> Result<ExportedArtifact, ExportError> {
        let path = request
            .output_dir
            .join(format!("{}.tables.xlsx", request.stem));
        let mut rows = Vec::new();
        for page in &request.document.pages {
            for region in &page.regions {
                if let RegionContent::Table(table) = &region.content {
                    for row in 0..table.rows {
                        let values = (0..table.columns)
                            .map(|column| {
                                table
                                    .cells
                                    .iter()
                                    .find(|cell| cell.row == row && cell.column == column)
                                    .map(|cell| {
                                        cell.blocks
                                            .iter()
                                            .map(|b| b.plain_text(request.text_view))
                                            .collect::<Vec<_>>()
                                            .join(" ")
                                    })
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>();
                        rows.push(values);
                    }
                }
            }
        }
        let mut sheet = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>",
        );
        for (row_index, row) in rows.iter().enumerate() {
            sheet.push_str(&format!("<row r=\"{}\">", row_index + 1));
            for (column, value) in row.iter().enumerate() {
                sheet.push_str(&format!(
                    "<c r=\"{}{}\" t=\"inlineStr\"><is><t>{}</t></is></c>",
                    spreadsheet_column(column),
                    row_index + 1,
                    xml_text(value)
                ));
            }
            sheet.push_str("</row>");
        }
        sheet.push_str("</sheetData></worksheet>");
        write_zip_atomic(
            &path,
            request.overwrite,
            &[
                (
                    "[Content_Types].xml",
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/></Types>",
                ),
                (
                    "_rels/.rels",
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>",
                ),
                (
                    "xl/workbook.xml",
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"Tables\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>",
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>",
                ),
                ("xl/worksheets/sheet1.xml", &sheet),
            ],
        )?;
        Ok(ExportedArtifact {
            format: self.format(),
            path,
        })
    }
}

fn spreadsheet_column(mut index: usize) -> String {
    let mut value = String::new();
    loop {
        value.insert(0, (b'A' + (index % 26) as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    value
}

fn write_zip_atomic(
    path: &Path,
    overwrite: bool,
    files: &[(&str, &str)],
) -> Result<(), ExportError> {
    use zip::write::SimpleFileOptions;
    if path.exists() && !overwrite {
        return Err(ExportError::Exists(path.to_path_buf()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ExportError::InvalidPath(path.to_path_buf()))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut archive = zip::ZipWriter::new(temporary.as_file_mut());
        for (name, body) in files {
            archive.start_file(*name, SimpleFileOptions::default())?;
            archive.write_all(body.as_bytes())?;
        }
        archive.finish()?;
    }
    temporary.as_file().sync_all()?;
    if overwrite && path.exists() {
        fs::remove_file(path)?;
    }
    temporary
        .persist(path)
        .map_err(|error| ExportError::Io(error.error))?;
    Ok(())
}

fn render_region(output: &mut String, content: &RegionContent, view: TextView) {
    match content {
        RegionContent::Text(block) => {
            output.push_str("<p>");
            output.push_str(
                &html_escape::encode_text(&block.plain_text(view)).replace('\n', "<br>\n"),
            );
            output.push_str("</p>\n");
        }
        RegionContent::Table(table) => {
            output.push_str("<table>\n");
            for row in 0..table.rows {
                output.push_str("<tr>");
                for column in 0..table.columns {
                    if let Some(cell) = table
                        .cells
                        .iter()
                        .find(|cell| cell.row == row && cell.column == column)
                    {
                        let tag = if cell.is_header { "th" } else { "td" };
                        output.push_str(&format!(
                            "<{tag} rowspan=\"{}\" colspan=\"{}\">{}</{tag}>",
                            cell.row_span.max(1),
                            cell.column_span.max(1),
                            html_escape::encode_text(
                                &cell
                                    .blocks
                                    .iter()
                                    .map(|block| block.plain_text(view))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            )
                        ));
                    } else {
                        output.push_str("<td></td>");
                    }
                }
                output.push_str("</tr>\n");
            }
            output.push_str("</table>\n");
        }
        RegionContent::Formula(formula) => {
            if let Some(mathml) = &formula.mathml {
                output.push_str(mathml);
                output.push('\n');
            } else if let Some(latex) = &formula.latex {
                output.push_str(&format!(
                    "<pre class=\"formula\">{}</pre>\n",
                    html_escape::encode_text(latex)
                ));
            }
        }
        RegionContent::Figure(figure) => {
            if let Some(caption) = &figure.caption {
                output.push_str(&format!(
                    "<figure><figcaption>{}</figcaption></figure>\n",
                    html_escape::encode_text(&caption.plain_text(view))
                ));
            }
        }
        RegionContent::Separator => output.push_str("<hr>\n"),
        RegionContent::Unknown => {}
    }
}

fn atomic_write(path: &Path, bytes: &[u8], overwrite: bool) -> Result<(), ExportError> {
    if path.exists() && !overwrite {
        return Err(ExportError::Exists(path.to_path_buf()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ExportError::InvalidPath(path.to_path_buf()))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    if overwrite && path.exists() {
        fs::remove_file(path)?;
    }
    temporary
        .persist(path)
        .map_err(|error| ExportError::Io(error.error))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("unknown export format `{0}`")]
    UnknownFormat(String),
    #[error("export format {0:?} is not implemented in this milestone")]
    NotImplemented(ExportFormat),
    #[error("output already exists: {0}")]
    Exists(PathBuf),
    #[error("invalid output path: {0}")]
    InvalidPath(PathBuf),
    #[error("invalid document: {0}")]
    InvalidDocument(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "source-preserving OCR overlay is not implemented for scanned pages; rerun with the rasterize PDF policy"
    )]
    PreservingPdfOverlayUnsupported,
    #[error("the source PDF page count changed after OCR")]
    SourcePageCountChanged,
    #[error("the renderer returned an unexpected raster format")]
    UnexpectedRasterFormat,
    #[error("page dimensions exceed JPEG limits")]
    JpegDimension,
    #[error("JPEG encoding failed: {0}")]
    Jpeg(String),
    #[error("PDF intake failed: {0}")]
    PdfRead(#[from] lege_pdf_read::ReadError),
    #[error("PDF writing failed: {0}")]
    PdfWrite(#[from] lege_pdf_write::types::WriteError),
    #[error("ZIP package writing failed: {0}")]
    Zip(#[from] zip::result::ZipError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_docir::*;
    use pdf_test_support::builder::PdfBuilder;

    fn empty_document() -> Document {
        Document::new(
            "d",
            SourceIdentity {
                path: "x.pdf".into(),
                content_hash: "x".into(),
                byte_len: 1,
                mime_type: "application/pdf".into(),
            },
            ProcessingManifest {
                pipeline_version: "test".into(),
                profile: ProcessingProfile::Search,
                quality: QualityMode::Thorough,
                configuration_hash: "c".into(),
                models: Vec::new(),
                warnings: Vec::new(),
            },
        )
    }

    #[test]
    fn exports_core_formats_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let document = empty_document();
        let request = ExportRequest {
            document: &document,
            output_dir: directory.path(),
            stem: "book",
            text_view: TextView::Corrected,
            overwrite: false,
            searchable_pdf_policy: SearchablePdfPolicy::PreserveSource,
        };
        let artifacts = export_all(
            &request,
            &[
                ExportFormat::Json,
                ExportFormat::Text,
                ExportFormat::Html,
                ExportFormat::Docx,
                ExportFormat::Alto,
                ExportFormat::PageXml,
                ExportFormat::Markdown,
                ExportFormat::Latex,
                ExportFormat::Xlsx,
                ExportFormat::Csv,
                ExportFormat::Hocr,
            ],
        )
        .unwrap();
        assert_eq!(artifacts.len(), 11);
        assert!(directory.path().join("book.lege.json").is_file());
        assert!(directory.path().join("book.qa.json").is_file());
        assert!(
            directory
                .path()
                .join("book.processing-manifest.json")
                .is_file()
        );
        assert!(directory.path().join("book.html/index.html").is_file());
        assert!(
            std::fs::read(directory.path().join("book.docx"))
                .unwrap()
                .starts_with(b"PK")
        );
    }

    #[test]
    fn rasterized_searchable_pdf_contains_a_text_layer() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.pdf");
        let mut pdf = PdfBuilder::new();
        pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
        pdf.add_object(
            2,
            "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
        );
        pdf.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R>>");
        pdf.add_stream(4, "", b"0.9 g 0 0 100 100 re f");
        pdf.finish_classic_xref("/Root 1 0 R");
        std::fs::write(&source_path, pdf.into_bytes()).unwrap();
        let mut document = empty_document();
        document.source.path = source_path.to_string_lossy().into_owned();
        document.pages.push(Page {
            index: 0,
            source_size: Size {
                width: 100,
                height: 100,
            },
            page_size_points: SizeF {
                width: 100.0,
                height: 100.0,
            },
            source_to_page: Transform::IDENTITY,
            source_kind: PageSourceKind::Rendered,
            image: None,
            regions: vec![Region {
                id: "p0-r0".into(),
                kind: RegionKind::Paragraph,
                polygon: rect_polygon(10.0, 10.0, 60.0, 30.0),
                confidence: RegionConfidence::default(),
                content: RegionContent::Text(TextBlock {
                    lines: vec![TextLine {
                        text: TextEvidence::raw("hello"),
                        polygon: rect_polygon(10.0, 10.0, 60.0, 30.0),
                        confidence: RecognitionConfidence::default(),
                        words: Vec::new(),
                        provenance: Provenance {
                            provider: "test".into(),
                            model: None,
                            preprocessing: None,
                            language: Some("eng".into()),
                        },
                    }],
                }),
                provenance: Provenance {
                    provider: "test".into(),
                    model: None,
                    preprocessing: None,
                    language: Some("eng".into()),
                },
            }],
            reading_order: vec!["p0-r0".into()],
            warnings: Vec::new(),
        });
        let request = ExportRequest {
            document: &document,
            output_dir: directory.path(),
            stem: "book",
            text_view: TextView::Raw,
            overwrite: false,
            searchable_pdf_policy: SearchablePdfPolicy::Rasterize,
        };
        exporter(ExportFormat::SearchablePdf)
            .unwrap()
            .export(&request)
            .unwrap();
        let bytes: Arc<[u8]> = std::fs::read(directory.path().join("book.searchable.pdf"))
            .unwrap()
            .into();
        let session = lege_pdf_read::RenderSession::open(bytes, None).unwrap();
        assert_eq!(lege_pdf_read::page_text(&session, 0).unwrap(), "hello");
    }
}
