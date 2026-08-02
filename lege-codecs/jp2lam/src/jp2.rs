use crate::error::{Jp2LamError, Result};
use crate::model::{ColorEncoding, ComponentView, ImageView};
use std::io::Write;

const JP2_SIGNATURE_BOX_LEN: u32 = 12;
const JP2_FILE_TYPE_BOX_LEN: u32 = 20;
const JP2_IMAGE_HEADER_BOX_LEN: u32 = 22;
const JP2_ENUM_CMYK: u32 = 12;
const JP2_ENUM_SRGB: u32 = 16;
const JP2_ENUM_GRAY: u32 = 17;
const JP2_COMPRESSION_TYPE_J2K: u8 = 7;

pub(crate) fn wrap_codestream_view(
    image: &ImageView<'_>,
    color_encoding: &ColorEncoding,
    codestream: &[u8],
) -> Result<Vec<u8>> {
    wrap_codestream_metadata(
        image.width,
        image.height,
        &image.components,
        color_encoding,
        codestream,
    )
}

pub(crate) fn write_jp2_header_for_view<W: Write>(
    image: &ImageView<'_>,
    color_encoding: &ColorEncoding,
    codestream_len: usize,
    writer: &mut W,
) -> Result<()> {
    write_jp2_header_metadata(
        image.width,
        image.height,
        &image.components,
        color_encoding,
        codestream_len,
        writer,
    )
}

fn wrap_codestream_metadata(
    width: u32,
    height: u32,
    components: &[ComponentView<'_>],
    color_encoding: &ColorEncoding,
    codestream: &[u8],
) -> Result<Vec<u8>> {
    let codestream_box_len = box_len(codestream.len())?;
    let (shared_bpc, bpcc) = jp2_component_depths(components)?;
    let bpcc_box_len = bpcc.as_ref().map_or(0, |values| 8 + values.len() as u32);
    let color_box_len = color_spec_box_len(color_encoding)?;
    let jp2_header_box_len = 8 + JP2_IMAGE_HEADER_BOX_LEN + bpcc_box_len + color_box_len;
    let total_len = JP2_SIGNATURE_BOX_LEN as usize
        + JP2_FILE_TYPE_BOX_LEN as usize
        + jp2_header_box_len as usize
        + codestream_box_len as usize;
    let mut out = Vec::with_capacity(total_len);

    write_box_header(&mut out, JP2_SIGNATURE_BOX_LEN, b"jP  ");
    out.extend_from_slice(&[0x0d, 0x0a, 0x87, 0x0a]);

    write_box_header(&mut out, JP2_FILE_TYPE_BOX_LEN, b"ftyp");
    out.extend_from_slice(b"jp2 ");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"jp2 ");

    write_box_header(&mut out, jp2_header_box_len, b"jp2h");

    write_box_header(&mut out, JP2_IMAGE_HEADER_BOX_LEN, b"ihdr");
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&(components.len() as u16).to_be_bytes());
    out.push(shared_bpc);
    out.push(JP2_COMPRESSION_TYPE_J2K);
    out.push(0);
    out.push(0);

    if let Some(values) = bpcc {
        write_box_header(&mut out, 8 + values.len() as u32, b"bpcc");
        out.extend_from_slice(&values);
    }

    write_color_spec(&mut out, color_encoding)?;

    write_box_header(&mut out, codestream_box_len, b"jp2c");
    out.extend_from_slice(codestream);

    Ok(out)
}

fn write_jp2_header_metadata<W: Write>(
    width: u32,
    height: u32,
    components: &[ComponentView<'_>],
    color_encoding: &ColorEncoding,
    codestream_len: usize,
    writer: &mut W,
) -> Result<()> {
    let codestream_box_len = box_len(codestream_len)?;
    let (shared_bpc, bpcc) = jp2_component_depths(components)?;
    let bpcc_box_len = bpcc.as_ref().map_or(0, |values| 8 + values.len() as u32);
    let color_box_len = color_spec_box_len(color_encoding)?;
    let jp2_header_box_len = 8 + JP2_IMAGE_HEADER_BOX_LEN + bpcc_box_len + color_box_len;

    write_box_header_to(writer, JP2_SIGNATURE_BOX_LEN, b"jP  ")?;
    write_all(writer, &[0x0d, 0x0a, 0x87, 0x0a])?;

    write_box_header_to(writer, JP2_FILE_TYPE_BOX_LEN, b"ftyp")?;
    write_all(writer, b"jp2 ")?;
    write_all(writer, &0u32.to_be_bytes())?;
    write_all(writer, b"jp2 ")?;

    write_box_header_to(writer, jp2_header_box_len, b"jp2h")?;

    write_box_header_to(writer, JP2_IMAGE_HEADER_BOX_LEN, b"ihdr")?;
    write_all(writer, &height.to_be_bytes())?;
    write_all(writer, &width.to_be_bytes())?;
    write_all(writer, &(components.len() as u16).to_be_bytes())?;
    write_all(writer, &[shared_bpc, JP2_COMPRESSION_TYPE_J2K, 0, 0])?;

    if let Some(values) = bpcc {
        write_box_header_to(writer, 8 + values.len() as u32, b"bpcc")?;
        write_all(writer, &values)?;
    }

    write_color_spec_to(writer, color_encoding)?;

    write_box_header_to(writer, codestream_box_len, b"jp2c")
}

/// ISO/IEC 15444-1 Annex I.5.3.1/I.5.3.2: BPC is `precision - 1`
/// with signedness in the high bit. Mixed component depths use BPC=255 and a
/// `bpcc` box containing one equivalent value per SIZ component.
fn jp2_component_depths(components: &[ComponentView<'_>]) -> Result<(u8, Option<Vec<u8>>)> {
    let values = components
        .iter()
        .map(|component| {
            let precision = u8::try_from(component.precision).map_err(|_| {
                Jp2LamError::EncodeFailed("component precision exceeds JP2 BPC range".into())
            })?;
            Ok((if component.signed { 0x80 } else { 0 }) | (precision - 1))
        })
        .collect::<Result<Vec<_>>>()?;
    let first = *values
        .first()
        .ok_or_else(|| Jp2LamError::EncodeFailed("JP2 requires at least one component".into()))?;
    if values.iter().all(|&value| value == first) {
        Ok((first, None))
    } else {
        Ok((0xff, Some(values)))
    }
}

fn color_spec_box_len(color_encoding: &ColorEncoding) -> Result<u32> {
    match color_encoding {
        ColorEncoding::Gray | ColorEncoding::Srgb | ColorEncoding::Cmyk => Ok(15),
        ColorEncoding::IccProfile { bytes, .. } => u32::try_from(11usize + bytes.len())
            .map_err(|_| Jp2LamError::EncodeFailed("ICC colr box exceeds u32".into())),
    }
}

/// Enumerated `EnumCS` value for an enumerated color encoding.
fn enum_cs_value(color_encoding: &ColorEncoding) -> u32 {
    match color_encoding {
        ColorEncoding::Gray => JP2_ENUM_GRAY,
        ColorEncoding::Cmyk => JP2_ENUM_CMYK,
        _ => JP2_ENUM_SRGB,
    }
}

fn write_color_spec(out: &mut Vec<u8>, color_encoding: &ColorEncoding) -> Result<()> {
    let len = color_spec_box_len(color_encoding)?;
    write_box_header(out, len, b"colr");
    match color_encoding {
        ColorEncoding::Gray | ColorEncoding::Srgb | ColorEncoding::Cmyk => {
            out.extend_from_slice(&[1, 0, 0]);
            out.extend_from_slice(&enum_cs_value(color_encoding).to_be_bytes());
        }
        ColorEncoding::IccProfile { bytes, .. } => {
            out.extend_from_slice(&[2, 0, 0]);
            out.extend_from_slice(bytes);
        }
    }
    Ok(())
}

fn write_color_spec_to<W: Write>(writer: &mut W, color_encoding: &ColorEncoding) -> Result<()> {
    write_box_header_to(writer, color_spec_box_len(color_encoding)?, b"colr")?;
    match color_encoding {
        ColorEncoding::Gray | ColorEncoding::Srgb | ColorEncoding::Cmyk => {
            write_all(writer, &[1, 0, 0])?;
            write_all(writer, &enum_cs_value(color_encoding).to_be_bytes())
        }
        ColorEncoding::IccProfile { bytes, .. } => {
            write_all(writer, &[2, 0, 0])?;
            write_all(writer, bytes)
        }
    }
}

fn box_len(payload_len: usize) -> Result<u32> {
    let total_len = payload_len
        .checked_add(8)
        .ok_or_else(|| Jp2LamError::EncodeFailed("JP2 box length overflow".to_string()))?;
    u32::try_from(total_len)
        .map_err(|_| Jp2LamError::EncodeFailed("JP2 box length exceeds 32-bit limit".to_string()))
}

fn write_box_header(out: &mut Vec<u8>, len: u32, box_type: &[u8; 4]) {
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(box_type);
}

fn write_box_header_to<W: Write>(writer: &mut W, len: u32, box_type: &[u8; 4]) -> Result<()> {
    write_all(writer, &len.to_be_bytes())?;
    write_all(writer, box_type)
}

fn write_all<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    writer
        .write_all(bytes)
        .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{jp2_component_depths, wrap_codestream_metadata};
    use crate::model::{ColorEncoding, ComponentView};

    #[test]
    fn annex_i_5_3_component_depths_use_shared_bpc_or_bpcc() {
        let samples = [0u16; 1];
        let ten = ComponentView::planar_u16(1, 1, &samples, 10).expect("10-bit view");
        let twelve = ComponentView::planar_u16(1, 1, &samples, 12).expect("12-bit view");

        assert_eq!(
            jp2_component_depths(&[ten, ten]).expect("shared BPC"),
            (9, None)
        );
        assert_eq!(
            jp2_component_depths(&[ten, twelve]).expect("mixed BPC"),
            (0xff, Some(vec![9, 11]))
        );
    }

    #[test]
    fn buffered_mixed_precision_header_closes_ihdr_before_bpcc() {
        let samples = [0u16];
        let components = [8, 10, 12, 16]
            .into_iter()
            .map(|precision| {
                ComponentView::planar_u16(1, 1, &samples, precision).expect("component view")
            })
            .collect::<Vec<_>>();
        let bytes = wrap_codestream_metadata(
            1,
            1,
            &components,
            &ColorEncoding::Cmyk,
            &[0xff, 0x4f, 0xff, 0xd9],
        )
        .expect("buffered JP2");

        // Signature (12) + ftyp (20) + jp2h header (8) puts ihdr at 40.
        assert_eq!(&bytes[40..44], &22u32.to_be_bytes());
        assert_eq!(&bytes[44..48], b"ihdr");
        assert_eq!(bytes[61], 0, "IPR must be the final ihdr payload byte");
        assert_eq!(&bytes[62..66], &12u32.to_be_bytes());
        assert_eq!(&bytes[66..70], b"bpcc");
        assert_eq!(&bytes[70..74], &[7, 9, 11, 15]);
    }
}
