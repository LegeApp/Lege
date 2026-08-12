//! DocumentWriter: write-on-arrival PDF assembly.
//!
//! `add_page` writes each artifact's objects to the sink the instant it is
//! submitted (any order), recording the page object id by logical index;
//! `finalize` writes the flat page tree, catalog, outline, metadata, xref, and
//! trailer. Object numbering is arrival-order (varies run-to-run); page order
//! and pixels do not. The async bounded-channel actor that drives this
//! (replacing the `spawn_pdf_writer_actor` body) arrives in M3.

use std::io::Write;
use std::sync::Arc;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::artifact::{PdfPageArtifact, TextFont};
use crate::content::ContentWriter;
use crate::font::{EmbeddedFont, write_embedded_font, write_helvetica};
use crate::images::write_image_xobject;
use crate::meta::{DocumentMeta, PdfProfile, write_info, write_output_intent};
use crate::outline::{OutlineItem, write_outline};
use crate::pages::{
    CatalogExtras, WrittenPageSlots, write_catalog, write_length_dict, write_page_dict,
    write_pages_root,
};
use crate::resources::{ResourceRegistry, SharedResourceId};
use crate::sink::{PdfSink, StreamBody};
use crate::text::emit_text_layer;
use crate::types::{ObjectId, ResourceName, Result, WriteError};
use crate::xref::write_xref_and_trailer;

/// Write-on-arrival PDF document assembler.
pub struct DocumentWriter<W: Write> {
    sink: PdfSink<W>,
    pages_root: ObjectId,
    slots: WrittenPageSlots,
    registry: ResourceRegistry,
    saw_text: bool,
    profile: PdfProfile,
    meta: DocumentMeta,
    metadata_explicit: bool,
    embedded_font: Option<EmbeddedFont>,
    /// Memoized Type0 font id (written on the first text-bearing page).
    font_id: Option<ObjectId>,
    /// Memoized Helvetica fallback id.
    helvetica_id: Option<ObjectId>,
    bookmarks: Vec<OutlineItem>,
}

impl<W: Write> DocumentWriter<W> {
    /// Create a writer using the default (PDF 1.7 image-only) profile.
    pub fn new(writer: W, total_pages: usize) -> Result<Self> {
        Self::with_profile(writer, total_pages, PdfProfile::default())
    }

    /// Create a writer with an explicit conformance profile. The profile fixes
    /// the file version, so it is chosen up front (the caller knows whether OCR
    /// is enabled for the job).
    pub fn with_profile(writer: W, total_pages: usize, profile: PdfProfile) -> Result<Self> {
        let sink = PdfSink::new(writer, profile.version())?;
        let mut w = Self {
            sink,
            pages_root: ObjectId::new(0),
            slots: WrittenPageSlots::new(total_pages),
            registry: ResourceRegistry::new(),
            saw_text: false,
            profile,
            meta: DocumentMeta {
                profile,
                ..Default::default()
            },
            metadata_explicit: false,
            embedded_font: None,
            font_id: None,
            helvetica_id: None,
            bookmarks: Vec::new(),
        };
        // Reserve the /Pages root so pages written on arrival can carry a valid
        // /Parent.
        w.pages_root = w.sink.alloc_id();
        Ok(w)
    }

    /// Provide the embedded OCR font (Lege's glyphless program + metrics).
    pub fn set_embedded_font(&mut self, font: EmbeddedFont) {
        self.embedded_font = Some(font);
    }

    /// Provide document metadata (dates, title, …). The profile is taken from
    /// the constructor; this preserves it.
    pub fn set_metadata(&mut self, mut meta: DocumentMeta) {
        meta.profile = self.profile;
        self.meta = meta;
        self.metadata_explicit = true;
    }

    /// Provide the outline (bookmark) tree, keyed by output page index.
    pub fn set_bookmarks(&mut self, bookmarks: Vec<OutlineItem>) {
        self.bookmarks = bookmarks;
    }

    /// Register a shared resource's bytes (e.g. JBIG2 globals) before any
    /// artifact references it.
    pub fn register_shared(&mut self, id: SharedResourceId, bytes: Arc<[u8]>) {
        self.registry.register(id, bytes);
    }

    /// Write one page's objects to the sink immediately, in arrival order.
    pub fn add_page(&mut self, artifact: &PdfPageArtifact) -> Result<()> {
        let mut xobjects = Vec::with_capacity(artifact.elements.len());
        let mut content = ContentWriter::new();

        for (i, element) in artifact.elements.iter().enumerate() {
            let img_id = write_image_xobject(&mut self.sink, &mut self.registry, &element.image)?;
            let name = ResourceName::Image((i + 1) as u32);
            xobjects.push((name, img_id));

            content.save();
            if element.image.is_image_mask() {
                content.set_gray_fill(0.0);
            }
            content.concat_matrix(element.transform);
            content.draw_xobject(name);
            content.restore();
        }

        // Text layer: emit into the same content stream and select fonts.
        let mut fonts: Vec<(ResourceName, ObjectId)> = Vec::new();
        let has_text = artifact
            .text_layer
            .as_ref()
            .is_some_and(|tl| !tl.runs.is_empty());
        if has_text {
            let layer = artifact.text_layer.as_ref().unwrap();
            match layer.font {
                TextFont::Embedded => {
                    let fid = self.ensure_embedded_font()?;
                    fonts.push((ResourceName::Font(0), fid));
                }
                TextFont::HelveticaFallback => {
                    let hid = self.ensure_helvetica()?;
                    fonts.push((ResourceName::Font(1), hid));
                }
            }
            emit_text_layer(&mut content, layer);
            self.saw_text = true;
        }

        // Content stream: compress when it carries a text layer (parity with
        // the current writer), uncompressed for pure image pages.
        let content_bytes = content.into_bytes();
        let contents_id = self.sink.alloc_id();
        if has_text {
            let compressed = deflate(&content_bytes)?;
            let mut cdict = Vec::new();
            cdict.extend_from_slice(b"<</Filter /FlateDecode /Length ");
            crate::serialize::write_u64(&mut cdict, compressed.len() as u64);
            cdict.extend_from_slice(b">>");
            self.sink
                .write_stream(contents_id, &cdict, &StreamBody::Owned(compressed))?;
        } else {
            let mut cdict = Vec::new();
            write_length_dict(&mut cdict, content_bytes.len());
            self.sink
                .write_stream(contents_id, &cdict, &StreamBody::Owned(content_bytes))?;
        }

        let page_id = self.sink.alloc_id();
        write_page_dict(
            &mut self.sink,
            page_id,
            self.pages_root,
            artifact.media_box,
            &xobjects,
            &fonts,
            contents_id,
        )?;

        self.slots.set(artifact.index, page_id)?;
        Ok(())
    }

    pub fn saw_text(&self) -> bool {
        self.saw_text
    }

    /// Finish the document: page tree, outline, metadata, catalog, xref,
    /// trailer. Errors (listing gaps) if any logical page never arrived.
    pub fn finalize(mut self) -> Result<W> {
        write_pages_root(&mut self.sink, self.pages_root, &self.slots)?;

        let outlines = if self.bookmarks.is_empty() {
            None
        } else {
            let items = std::mem::take(&mut self.bookmarks);
            write_outline(&mut self.sink, &self.slots, &items)?
        };

        let output_intent = if self.profile.wants_pdfa_metadata() {
            Some(write_output_intent(&mut self.sink)?)
        } else {
            None
        };
        let info = if self.profile.wants_pdfa_metadata() || self.metadata_explicit {
            Some(write_info(&mut self.sink, &self.meta)?)
        } else {
            None
        };

        let catalog = write_catalog(
            &mut self.sink,
            self.pages_root,
            CatalogExtras {
                outlines,
                output_intent,
                mark_info: self.profile.wants_pdfa_metadata(),
            },
        )?;

        write_xref_and_trailer(&mut self.sink, catalog, info)?;
        self.sink.finish()
    }

    // --- internals ------------------------------------------------------

    fn ensure_embedded_font(&mut self) -> Result<ObjectId> {
        if let Some(id) = self.font_id {
            return Ok(id);
        }
        let font = self.embedded_font.as_ref().ok_or_else(|| {
            WriteError::InvalidArtifact(
                "text layer uses the embedded font but none was set".to_string(),
            )
        })?;
        let id = write_embedded_font(&mut self.sink, font)?;
        self.font_id = Some(id);
        Ok(id)
    }

    fn ensure_helvetica(&mut self) -> Result<ObjectId> {
        if let Some(id) = self.helvetica_id {
            return Ok(id);
        }
        let id = write_helvetica(&mut self.sink)?;
        self.helvetica_id = Some(id);
        Ok(id)
    }
}

/// Deflate (zlib) a content stream.
fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).map_err(WriteError::from)?;
    enc.finish().map_err(WriteError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{
        ColorModel, PdfImageElement, PdfImageResource, PreparedTextLayer, TextRun,
    };
    use crate::types::{Affine, PdfRect};

    fn jpeg_page(index: u32) -> PdfPageArtifact {
        PdfPageArtifact {
            index,
            media_box: PdfRect::from_size(612.0, 792.0),
            elements: Box::new([PdfImageElement {
                transform: Affine::scale_translate(612.0, 792.0, 0.0, 0.0),
                image: PdfImageResource::Jpeg {
                    data: Arc::from(&[0xFFu8, 0xD8, 0xFF, 0xD9][..]),
                    width: 100,
                    height: 129,
                    color: ColorModel::Rgb,
                },
            }]),
            text_layer: None,
        }
    }

    fn sample_font() -> EmbeddedFont {
        EmbeddedFont {
            data: Arc::from(&[0u8; 32][..]),
            post_script_name: "Glyphless".to_string(),
            ascent: 1000,
            descent: -200,
            cap_height: 700,
            italic_angle: 0.0,
            bbox: [-100, -200, 1000, 900],
        }
    }

    #[test]
    fn out_of_order_pages_finalize_in_order() {
        let mut w = DocumentWriter::new(Vec::new(), 3).unwrap();
        w.add_page(&jpeg_page(2)).unwrap();
        w.add_page(&jpeg_page(0)).unwrap();
        w.add_page(&jpeg_page(1)).unwrap();
        let bytes = w.finalize().unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.starts_with("%PDF-1.7"));
        assert!(text.contains("/Type /Catalog"));
        assert!(text.contains("/Count 3"));
        assert!(text.trim_end().ends_with("%%EOF"));
    }

    #[test]
    fn missing_page_is_a_finalize_error() {
        let mut w = DocumentWriter::new(Vec::new(), 2).unwrap();
        w.add_page(&jpeg_page(0)).unwrap();
        let err = w.finalize().unwrap_err();
        assert!(matches!(
            err,
            WriteError::IncompletePages {
                missing: 1,
                total: 2
            }
        ));
    }

    #[test]
    fn pdf17_writes_info_only_when_identity_was_explicitly_set() {
        let mut plain = DocumentWriter::new(Vec::new(), 1).unwrap();
        plain.add_page(&jpeg_page(0)).unwrap();
        let plain = String::from_utf8_lossy(&plain.finalize().unwrap()).into_owned();
        assert!(!plain.contains("/Info "));

        let mut identified = DocumentWriter::new(Vec::new(), 1).unwrap();
        identified.set_metadata(DocumentMeta {
            title: "The Book".to_string(),
            author: "Ada Lovelace".to_string(),
            subject: String::new(),
            keywords: String::new(),
            ..Default::default()
        });
        identified.add_page(&jpeg_page(0)).unwrap();
        let identified = String::from_utf8_lossy(&identified.finalize().unwrap()).into_owned();
        assert!(identified.contains("/Info "));
        assert!(identified.contains("/Title (The Book)"));
        assert!(identified.contains("/Author (Ada Lovelace)"));
        assert!(!identified.contains("/Subject "));
    }

    #[test]
    fn duplicate_index_rejected() {
        let mut w = DocumentWriter::new(Vec::new(), 2).unwrap();
        w.add_page(&jpeg_page(0)).unwrap();
        let err = w.add_page(&jpeg_page(0)).unwrap_err();
        assert!(matches!(err, WriteError::DuplicatePage(0)));
    }

    #[test]
    fn text_page_embeds_font_and_pdfa_metadata() {
        let mut w = DocumentWriter::with_profile(Vec::new(), 1, PdfProfile::PdfA1b).unwrap();
        w.set_embedded_font(sample_font());
        let art = PdfPageArtifact {
            index: 0,
            media_box: PdfRect::from_size(612.0, 792.0),
            elements: Box::new([]),
            text_layer: Some(PreparedTextLayer {
                runs: Box::new([TextRun {
                    text: "hello".to_string(),
                    x: 72.0,
                    y: 700.0,
                    size: 10.0,
                }]),
                font: TextFont::Embedded,
            }),
        };
        w.add_page(&art).unwrap();
        assert!(w.saw_text());
        let bytes = w.finalize().unwrap();
        let t = String::from_utf8_lossy(&bytes).into_owned();

        assert!(t.starts_with("%PDF-1.4"), "PDF/A pins 1.4");
        assert!(t.contains("/Subtype /Type0"));
        assert!(t.contains("/F0 "), "page references the embedded font");
        assert!(t.contains("/FlateDecode"), "text content compressed");
        assert!(t.contains("/Type /OutputIntent"));
        assert!(t.contains("/MarkInfo <</Marked true>>"));
        assert!(t.contains("/Producer (Lege PDF Generator)"));
        assert!(t.contains("/Info "), "trailer references Info");
    }

    #[test]
    fn bookmarks_produce_outline() {
        let mut w = DocumentWriter::new(Vec::new(), 2).unwrap();
        w.set_bookmarks(vec![OutlineItem {
            title: "Cover".to_string(),
            page_index: 0,
            top: None,
            children: vec![],
        }]);
        w.add_page(&jpeg_page(0)).unwrap();
        w.add_page(&jpeg_page(1)).unwrap();
        let bytes = w.finalize().unwrap();
        let t = String::from_utf8_lossy(&bytes).into_owned();
        assert!(t.contains("/Type /Outlines"));
        assert!(t.contains("(Cover)"));
        assert!(t.contains("/Outlines "), "catalog links the outline");
    }
}
