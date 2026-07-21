//! Typed image XObject writers, one per codec: DCTDecode (rgb/gray),
//! JPXDecode, JBIG2Decode (+globals, +ImageMask stencil), CCITTFaxDecode
//! (K -1, BlackIs1, Decode [1 0]), Indexed8 palette. PLAN.md §4.3 fidelity table.
//!
//! Each writer emits the exact dictionary the current `accumulator.rs` produces
//! for that codec, so output is a parity port validated differentially. The
//! writer never re-encodes: the stream body is the encoder's bytes verbatim.

use std::io::Write;

use crate::artifact::{ColorModel, PdfImageResource};
use crate::resources::ResourceRegistry;
use crate::serialize::{write_i64, write_name, write_ref};
use crate::sink::{PdfSink, StreamBody};
use crate::types::{ObjectId, Result};

/// Write the image XObject (and, for indexed images, its palette stream) and
/// return the image object id to be referenced from the page's `/XObject`
/// resources.
pub fn write_image_xobject<W: Write>(
    sink: &mut PdfSink<W>,
    registry: &mut ResourceRegistry,
    image: &PdfImageResource,
) -> Result<ObjectId> {
    match image {
        PdfImageResource::Jpeg {
            data,
            width,
            height,
            color,
        } => {
            let mut dict = Vec::new();
            begin_image(&mut dict, *width, *height, data.len());
            key(&mut dict, b"Interpolate");
            dict.extend_from_slice(b"true");
            colorspace_name(&mut dict, *color);
            key(&mut dict, b"BitsPerComponent");
            write_i64(&mut dict, 8);
            key(&mut dict, b"Filter");
            write_name(&mut dict, b"DCTDecode");
            end_dict(&mut dict);
            write_body(sink, &dict, data)
        }
        PdfImageResource::Jpx {
            data,
            width,
            height,
            color,
        } => {
            let mut dict = Vec::new();
            begin_image(&mut dict, *width, *height, data.len());
            key(&mut dict, b"Interpolate");
            dict.extend_from_slice(b"true");
            colorspace_name(&mut dict, *color);
            key(&mut dict, b"BitsPerComponent");
            write_i64(&mut dict, 8);
            key(&mut dict, b"Filter");
            write_name(&mut dict, b"JPXDecode");
            end_dict(&mut dict);
            write_body(sink, &dict, data)
        }
        PdfImageResource::Jbig2 {
            data,
            width,
            height,
            globals,
            image_mask,
        } => {
            // Resolve globals first so the object exists before the image dict.
            let globals_obj = match globals {
                Some(id) => Some(registry.ensure_written(sink, *id)?),
                None => None,
            };
            let mut dict = Vec::new();
            begin_image(&mut dict, *width, *height, data.len());
            if *image_mask {
                key(&mut dict, b"ImageMask");
                dict.extend_from_slice(b"true");
            } else {
                key(&mut dict, b"ColorSpace");
                write_name(&mut dict, b"DeviceGray");
            }
            key(&mut dict, b"BitsPerComponent");
            write_i64(&mut dict, 1);
            key(&mut dict, b"Interpolate");
            dict.extend_from_slice(b"false");
            key(&mut dict, b"Filter");
            write_name(&mut dict, b"JBIG2Decode");
            key(&mut dict, b"Decode");
            dict.extend_from_slice(b"[0 1]");
            if let Some(gobj) = globals_obj {
                key(&mut dict, b"DecodeParms");
                dict.extend_from_slice(b"<<");
                key(&mut dict, b"JBIG2Globals");
                write_ref(&mut dict, gobj);
                dict.extend_from_slice(b">>");
            }
            end_dict(&mut dict);
            write_body(sink, &dict, data)
        }
        PdfImageResource::CcittGroup4 {
            data,
            width,
            height,
            black_is_one,
        } => {
            let mut dict = Vec::new();
            begin_image(&mut dict, *width, *height, data.len());
            key(&mut dict, b"ColorSpace");
            write_name(&mut dict, b"DeviceGray");
            key(&mut dict, b"BitsPerComponent");
            write_i64(&mut dict, 1);
            key(&mut dict, b"Interpolate");
            dict.extend_from_slice(b"false");
            key(&mut dict, b"Filter");
            write_name(&mut dict, b"CCITTFaxDecode");
            key(&mut dict, b"DecodeParms");
            dict.extend_from_slice(b"<<");
            key(&mut dict, b"K");
            write_i64(&mut dict, -1);
            key(&mut dict, b"EndOfBlock");
            dict.extend_from_slice(b"false");
            key(&mut dict, b"EncodedByteAlign");
            dict.extend_from_slice(b"false");
            key(&mut dict, b"Columns");
            write_i64(&mut dict, *width as i64);
            key(&mut dict, b"Rows");
            write_i64(&mut dict, *height as i64);
            key(&mut dict, b"BlackIs1");
            dict.extend_from_slice(if *black_is_one { b"true" } else { b"false" });
            dict.extend_from_slice(b">>");
            key(&mut dict, b"Decode");
            dict.extend_from_slice(b"[1 0]");
            end_dict(&mut dict);
            write_body(sink, &dict, data)
        }
        PdfImageResource::Indexed8 {
            palette,
            indices,
            width,
            height,
        } => {
            // Palette gets its own stream object referenced by the colorspace.
            let palette_obj = sink.alloc_id();
            let mut pdict = Vec::new();
            pdict.extend_from_slice(b"<</Length ");
            crate::serialize::write_u64(&mut pdict, palette.len() as u64);
            pdict.extend_from_slice(b">>");
            sink.write_stream(palette_obj, &pdict, &StreamBody::Shared(palette.clone()))?;

            let mut dict = Vec::new();
            begin_image(&mut dict, *width, *height, indices.len());
            key(&mut dict, b"Interpolate");
            dict.extend_from_slice(b"true");
            key(&mut dict, b"ColorSpace");
            dict.extend_from_slice(b"[");
            write_name(&mut dict, b"Indexed");
            dict.push(b' ');
            write_name(&mut dict, b"DeviceRGB");
            dict.extend_from_slice(b" 255 ");
            write_ref(&mut dict, palette_obj);
            dict.push(b']');
            key(&mut dict, b"BitsPerComponent");
            write_i64(&mut dict, 8);
            end_dict(&mut dict);
            write_body(sink, &dict, indices)
        }
    }
}

/// Open an image dictionary with the entries common to every codec, including
/// the `/Length` the sink's stream framing requires.
fn begin_image(dict: &mut Vec<u8>, width: u32, height: u32, body_len: usize) {
    dict.extend_from_slice(b"<<");
    key(dict, b"Type");
    write_name(dict, b"XObject");
    key(dict, b"Subtype");
    write_name(dict, b"Image");
    key(dict, b"Width");
    write_i64(dict, width as i64);
    key(dict, b"Height");
    write_i64(dict, height as i64);
    key(dict, b"Length");
    write_i64(dict, body_len as i64);
}

fn end_dict(dict: &mut Vec<u8>) {
    dict.extend_from_slice(b">>");
}

fn colorspace_name(dict: &mut Vec<u8>, color: ColorModel) {
    key(dict, b"ColorSpace");
    match color {
        ColorModel::Rgb => write_name(dict, b"DeviceRGB"),
        ColorModel::Gray => write_name(dict, b"DeviceGray"),
    }
}

/// Write `/Key ` (name plus a separating space so the following value token is
/// not glued onto the key name).
fn key(dict: &mut Vec<u8>, k: &[u8]) {
    write_name(dict, k);
    dict.push(b' ');
}

fn write_body<W: Write>(
    sink: &mut PdfSink<W>,
    dict: &[u8],
    data: &std::sync::Arc<[u8]>,
) -> Result<ObjectId> {
    let obj = sink.alloc_id();
    sink.write_stream(obj, dict, &StreamBody::Shared(data.clone()))?;
    Ok(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::SharedResourceId;
    use std::sync::Arc;

    fn render(image: &PdfImageResource) -> String {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let mut reg = ResourceRegistry::new();
        write_image_xobject(&mut sink, &mut reg, image).unwrap();
        String::from_utf8_lossy(&sink.finish().unwrap()).into_owned()
    }

    #[test]
    fn jpeg_rgb_dictionary() {
        let t = render(&PdfImageResource::Jpeg {
            data: Arc::from(&[0xFFu8, 0xD8, 0xFF][..]),
            width: 640,
            height: 480,
            color: ColorModel::Rgb,
        });
        assert!(t.contains("/Subtype /Image"));
        assert!(t.contains("/Width 640"));
        assert!(t.contains("/Height 480"));
        assert!(t.contains("/Length 3"));
        assert!(t.contains("/ColorSpace /DeviceRGB"));
        assert!(t.contains("/BitsPerComponent 8"));
        assert!(t.contains("/Filter /DCTDecode"));
        assert!(t.contains("/Interpolate true"));
    }

    #[test]
    fn jbig2_with_globals_references_registry() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let mut reg = ResourceRegistry::new();
        let gid = SharedResourceId(5);
        reg.register(gid, Arc::from(&b"G"[..]));
        let img = PdfImageResource::Jbig2 {
            data: Arc::from(&[0u8; 4][..]),
            width: 8,
            height: 8,
            globals: Some(gid),
            image_mask: false,
        };
        let obj = write_image_xobject(&mut sink, &mut reg, &img).unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();
        assert!(t.contains("/Filter /JBIG2Decode"));
        assert!(t.contains("/Decode [0 1]"));
        assert!(t.contains("/ColorSpace /DeviceGray"));
        assert!(t.contains("/BitsPerComponent 1"));
        assert!(t.contains("/Interpolate false"));
        assert!(
            t.contains("/JBIG2Globals 1 0 R"),
            "globals ref missing: {t}"
        );
        assert_eq!(obj.num, 2, "image object after globals object");
    }

    #[test]
    fn jbig2_mask_has_no_colorspace() {
        let t = render(&PdfImageResource::Jbig2 {
            data: Arc::from(&[0u8; 4][..]),
            width: 8,
            height: 8,
            globals: None,
            image_mask: true,
        });
        assert!(t.contains("/ImageMask true"));
        assert!(
            !t.contains("/ColorSpace"),
            "mask must have no ColorSpace: {t}"
        );
        assert!(t.contains("/Decode [0 1]"));
    }

    #[test]
    fn ccitt_group4_params() {
        let t = render(&PdfImageResource::CcittGroup4 {
            data: Arc::from(&[0u8; 10][..]),
            width: 1728,
            height: 2200,
            black_is_one: true,
        });
        assert!(t.contains("/Filter /CCITTFaxDecode"));
        assert!(t.contains("/K -1"));
        assert!(t.contains("/Columns 1728"));
        assert!(t.contains("/Rows 2200"));
        assert!(t.contains("/BlackIs1 true"));
        assert!(t.contains("/EndOfBlock false"));
        assert!(t.contains("/EncodedByteAlign false"));
        assert!(t.contains("/Decode [1 0]"));
    }

    #[test]
    fn indexed8_splits_palette_stream() {
        let t = render(&PdfImageResource::Indexed8 {
            palette: Arc::from(&[1u8; 768][..]),
            indices: Arc::from(&[2u8; 50][..]),
            width: 10,
            height: 5,
        });
        assert!(
            t.contains("/ColorSpace [/Indexed /DeviceRGB 255 1 0 R]"),
            "{t}"
        );
        assert!(t.contains("/Length 768"), "palette stream length: {t}");
        assert!(t.contains("/Length 50"), "index stream length: {t}");
        assert!(t.contains("/BitsPerComponent 8"));
    }
}
