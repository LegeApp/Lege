use anyhow::{bail, Result};
use log::info;
use lopdf::{
    content::{Content, Operation},
    Dictionary, Document, Object, Stream,
};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

// These structs are defined within this module, so no need to import from crate::lib
// use crate::lib::{ContentType, ContentElement, Page, ImageFormat};
use crate::jbig2shared::get_f32_from_json;

/// Represents a page in the PDF document
#[derive(Debug)]
pub struct Page {
    pub content: Vec<u8>,
    pub width: f32,
    pub height: f32,
}

// Represents JBIG2 input data: a symbol dictionary and ROI streams with metadata.
#[derive(Debug)]
pub struct Jbig2Input {
    symbol_dict: Option<Vec<u8>>, // Global symbol dictionary stream
    rois: Vec<Jbig2Roi>,          // Per-ROI JBIG2 streams and metadata
}

#[derive(Debug)]
pub struct Jbig2Roi {
    pub stream: Vec<u8>, // JBIG2-encoded ROI data
    pub width: u32,      // Pixel width
    pub height: u32,     // Pixel height
    pub xres: u32,       // X-resolution (DPI)
    pub yres: u32,       // Y-resolution (DPI)
    pub pdf_x: f32,      // PDF coordinates (points)
    pub pdf_y: f32,
    pub pdf_width: f32,
    pub pdf_height: f32,
    pub page_index: usize, // Page assignment (from JSON)
}

impl Jbig2Input {
    // Creates input from in-memory JBIG2 streams (from jbig2.rs).
    pub fn from_memory(symbol_dict: Option<Vec<u8>>, rois: Vec<Jbig2Roi>) -> Self {
        Self { symbol_dict, rois }
    }

    // Creates input from files, mimicking jbig2topdf.py.
    pub fn from_files(sym_path: &str, roi_paths: &[String], json_data: &JsonValue) -> Result<Self> {
        let symbol_dict = if !sym_path.is_empty() {
            let mut file = File::open(sym_path)?;
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;
            Some(data)
        } else {
            None
        };

        let pages_json = json_data
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("JSON must be an array of page objects"))?;

        let mut rois = Vec::with_capacity(roi_paths.len());
        for (idx, path) in roi_paths.iter().enumerate() {
            let mut file = File::open(path)?;
            let mut stream = Vec::new();
            file.read_to_end(&mut stream)?;

            // Parse page header for dimensions and resolution (per jbig2topdf.py)
            if stream.len() < 27 {
                bail!("ROI file {} too short for header", path);
            }
            let header = &stream[11..27];
            let (width, height, xres, yres) = (
                u32::from_be_bytes(header[0..4].try_into()?),
                u32::from_be_bytes(header[4..8].try_into()?),
                u32::from_be_bytes(header[8..12].try_into()?),
                u32::from_be_bytes(header[12..16].try_into()?),
            );

            // Extract PDF coordinates from JSON
            let page_idx = idx / pages_json.len(); // Simple assignment; adjust as needed
            let det_idx = idx % pages_json.len();
            let page_json = &pages_json
                .get(page_idx)
                .ok_or_else(|| anyhow::anyhow!("Missing page {}", page_idx))?;
            let detections = page_json["detections"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Missing detections for page {}", page_idx))?;
            let det = detections
                .get(det_idx)
                .ok_or_else(|| anyhow::anyhow!("Missing detection {}", det_idx))?;
            let bbox_pdf = det["bbox_pdf"]
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("Missing bbox_pdf for detection {}", det_idx))?;

            let pdf_x = get_f32_from_json(bbox_pdf, "x", 0.0);
            let pdf_y = get_f32_from_json(bbox_pdf, "y", 0.0);
            let pdf_width = get_f32_from_json(bbox_pdf, "width", width as f32);
            let pdf_height = get_f32_from_json(bbox_pdf, "height", height as f32);

            rois.push(Jbig2Roi {
                stream,
                width,
                height,
                xres: xres.max(72), // Default DPI
                yres: yres.max(72),
                pdf_x,
                pdf_y,
                pdf_width,
                pdf_height,
                page_index: page_idx,
            });
        }

        Ok(Self { symbol_dict, rois })
    }
}

// Converts JBIG2 input to a vector of Page objects
pub fn jbig2_to_content_type(input: Jbig2Input, page_dims: &[(f32, f32)]) -> Result<Vec<Page>> {
    // Group ROIs by page_index
    let mut pages: BTreeMap<usize, Vec<&Jbig2Roi>> = BTreeMap::new();
    for roi in &input.rois {
        pages.entry(roi.page_index).or_default().push(roi);
    }

    let mut result = Vec::with_capacity(pages.len());

    for (page_idx, rois) in &pages {
        let (page_width, page_height) = page_dims.get(*page_idx).copied().unwrap_or((612.0, 792.0)); // Default 8.5x11 inches

        // Create a simple representation of the page content
        let content = format!(
            "Page {} with {} ROIs ({}x{} points)",
            page_idx,
            rois.len(),
            page_width,
            page_height
        )
        .into_bytes();

        result.push(Page {
            content,
            width: page_width,
            height: page_height,
        });
    }

    Ok(result)
}

// Creates a PDF from JBIG2 input, producing a file compatible with lib.rs.
pub fn create_jbig2_pdf(
    input: Jbig2Input,
    output_path: &str,
    page_dims: &[(f32, f32)],
) -> Result<()> {
    info!(
        "Creating JBIG2 PDF with {} ROIs at {}",
        input.rois.len(),
        output_path
    );

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut page_ids = Vec::new();

    // Group ROIs by page_index
    let mut pages: BTreeMap<usize, Vec<&Jbig2Roi>> = BTreeMap::new();
    for roi in &input.rois {
        pages.entry(roi.page_index).or_default().push(roi);
    }

    for (page_idx, rois) in &pages {
        let (page_width, page_height) = page_dims.get(*page_idx).copied().unwrap_or((612.0, 792.0)); // Default 8.5x11 inches

        let page_id = doc.new_object_id();
        let mut page_dict = Dictionary::new();
        page_dict.set("Type", Object::Name(b"Page".to_vec()));
        page_dict.set("Parent", Object::Reference(pages_id));
        page_dict.set(
            "MediaBox",
            vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(page_width),
                Object::Real(page_height),
            ],
        );

        let mut resources = Dictionary::new();
        let mut xobjects = Dictionary::new();

        let mut content = Content {
            operations: Vec::new(),
        };

        for (idx, roi) in rois.iter().enumerate() {
            let mut img_dict = Dictionary::new();
            img_dict.set("Type", Object::Name(b"XObject".to_vec()));
            img_dict.set("Subtype", Object::Name(b"Image".to_vec()));
            img_dict.set("Width", Object::Integer(roi.width as i64));
            img_dict.set("Height", Object::Integer(roi.height as i64));
            img_dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
            img_dict.set("BitsPerComponent", Object::Integer(1));
            img_dict.set("Filter", Object::Name(b"JBIG2Decode".to_vec()));

            // Add symbol dictionary if present
            if let Some(sym_data) = &input.symbol_dict {
                let mut sym_dict = Dictionary::new();
                sym_dict.set("Type", Object::Name(b"XObject".to_vec()));
                sym_dict.set("Subtype", Object::Name(b"JBIG2Decode".to_vec()));
                let sym_stream = Stream::new(sym_dict, sym_data.clone());
                let sym_id = doc.add_object(sym_stream);

                let mut decode_parms = Dictionary::new();
                decode_parms.set("JBIG2Globals", Object::Reference(sym_id));
                img_dict.set("DecodeParms", Object::Dictionary(decode_parms));
            }

            let img_id = doc.add_object(Stream::new(img_dict, roi.stream.clone()));
            let img_name = format!("Img{}", idx);
            xobjects.set(img_name.clone(), Object::Reference(img_id));

            // Scale to PDF coordinates
            let scale_x = roi.pdf_width * 72.0 / (roi.width as f32 * roi.xres as f32);
            let scale_y = roi.pdf_height * 72.0 / (roi.height as f32 * roi.yres as f32);

            content.operations.push(Operation::new("q", vec![]));
            content.operations.push(Operation::new(
                "cm",
                vec![
                    Object::Real(scale_x),
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(scale_y),
                    Object::Real(roi.pdf_x),
                    Object::Real(page_height - roi.pdf_y - roi.pdf_height),
                ],
            ));
            content.operations.push(Operation::new(
                "Do",
                vec![Object::Name(img_name.as_bytes().to_vec())],
            ));
            content.operations.push(Operation::new("Q", vec![]));
        }

        // flush page resources
        resources.set("XObject", Object::Dictionary(xobjects));
        page_dict.set("Resources", Object::Dictionary(resources));

        // Add content stream to page
        let content_stream = Stream::new(Dictionary::new(), content.encode()?);
        let content_id = doc.add_object(content_stream);
        page_dict.set("Contents", Object::Reference(content_id));

        // Add page to document
        doc.objects.insert(page_id, Object::Dictionary(page_dict));
        page_ids.push(Object::Reference(page_id));
    }

    // Create pages tree
    let pages_dict = Dictionary::from_iter([
        (b"Type".to_vec(), Object::Name(b"Pages".to_vec())),
        (b"Kids".to_vec(), Object::Array(page_ids)),
        (b"Count".to_vec(), Object::Integer(pages.len() as i64)),
    ]);
    doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

    // Create catalog
    let catalog_id = doc.new_object_id();
    let catalog_dict = Dictionary::from_iter([
        (b"Type".to_vec(), Object::Name(b"Catalog".to_vec())),
        (b"Pages".to_vec(), Object::Reference(pages_id)),
    ]);
    doc.objects
        .insert(catalog_id, Object::Dictionary(catalog_dict));
    doc.trailer.set("Root", Object::Reference(catalog_id));

    // Save the PDF
    doc.save(output_path)?;
    info!("Successfully created JBIG2 PDF at {}", output_path);
    Ok(())
}

// For testing: mimics jbig2topdf.py’s main function.
pub fn main_jbig2topdf(args: &[String], json_data: &str, page_dims: &[(f32, f32)]) -> Result<()> {
    let (sym_path, roi_paths) = if args.contains(&"-s".to_string()) {
        // Standalone mode
        (
            "".to_string(),
            args.iter()
                .filter(|&arg| arg != "-s")
                .cloned()
                .collect::<Vec<_>>(),
        )
    } else if args.len() == 2 {
        // Basename mode
        let base = &args[1];
        (
            format!("{}.sym", base),
            glob::glob(&format!("{}.[0-9]*", base))?
                .filter_map(|p| p.ok().and_then(|p| p.to_str().map(String::from)))
                .collect(),
        )
    } else if args.len() == 1 {
        // Default mode
        (
            "symboltable".to_string(),
            glob::glob("page-*")?
                .filter_map(|p| p.ok().and_then(|p| p.to_str().map(String::from)))
                .collect(),
        )
    } else {
        bail!("Invalid arguments: {:?}", args);
    };

    if !sym_path.is_empty() && !Path::new(&sym_path).exists() {
        bail!("Symbol table '{}' not found", sym_path);
    }
    if roi_paths.is_empty() {
        bail!("No ROI files found");
    }

    let json_value: JsonValue = serde_json::from_str(json_data)?;
    let input = Jbig2Input::from_files(&sym_path, &roi_paths, &json_value)?;
    create_jbig2_pdf(input, "output.pdf", page_dims)?;
    Ok(())
}
