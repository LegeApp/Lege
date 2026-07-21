use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use image::GrayImage;
use jbig2enc_rust::jbig2halftone::{
    encode_halftone_document_auto, encode_halftone_pdf_split_auto, halftone_bilevel_preview,
};
use jbig2enc_rust::jbig2structs::GenericRegionParams;
use jbig2enc_rust::jbig2structs::Jbig2Config;
use jbig2enc_rust::jbig2sym::{BitImage, binary_pixels_to_bitimage};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, ValueEnum)]
enum DitherMode {
    Threshold,
    Ordered,
    Floyd,
}

#[derive(Clone, Debug, ValueEnum)]
enum GrayCodingMode {
    Arithmetic,
    Mmr,
}

#[derive(Clone, Debug, ValueEnum)]
enum DictCodingMode {
    Arithmetic,
    Mmr,
}

#[derive(Parser, Debug)]
#[command(author, version, about = "Isolated JBIG2 halftone encoder validator")]
struct Args {
    /// Source image: jpg/png/webp/tiff/pbm-compatible via the image crate.
    input: PathBuf,

    /// Standalone JBIG2 output.
    #[arg(short, long, default_value = "halftone-output.jb2")]
    output: PathBuf,

    /// PDF output embedding the JBIG2 fragment with /JBIG2Decode.
    #[arg(long, default_value = "halftone-output.pdf")]
    pdf_output: PathBuf,

    /// PBM of the preprocessed bilevel input fed to the halftone encoder.
    #[arg(long, default_value = "halftone-input.pbm")]
    prepared_pbm: PathBuf,

    /// PBM decoded back from the standalone JBIG2 file.
    #[arg(long, default_value = "halftone-decoded.pbm")]
    decoded_pbm: PathBuf,

    /// Optional PBM preview of the reconstructed halftone tile output.
    #[arg(long, default_value = "halftone-preview.pbm")]
    preview_pbm: PathBuf,

    #[arg(long, value_enum, default_value = "floyd")]
    dither: DitherMode,

    #[arg(long, default_value_t = 160)]
    threshold: u8,

    #[arg(long, default_value_t = 4)]
    grid_size: u32,

    #[arg(long, default_value_t = 16)]
    levels: u32,

    #[arg(long, default_value_t = 300)]
    dpi: u32,

    #[arg(long, default_value_t = 0.5)]
    sharpening: f32,

    #[arg(long, default_value_t = 0)]
    template: u8,

    #[arg(long)]
    lossless: bool,

    #[arg(long, value_enum, default_value = "arithmetic")]
    gray_coding: GrayCodingMode,

    #[arg(long, value_enum, default_value = "mmr")]
    dict_coding: DictCodingMode,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let gray = image::open(&args.input)
        .with_context(|| format!("failed to open {}", args.input.display()))?
        .to_luma8();

    let prepared = prepare_bilevel(&gray, &args.dither, args.threshold);
    let bitimage =
        binary_pixels_to_bitimage(&prepared, gray.width() as usize, gray.height() as usize)
            .map_err(|e| anyhow!(e))?;
    write_pbm(
        &args.prepared_pbm,
        &prepared,
        gray.width() as usize,
        gray.height() as usize,
    )?;

    let mut config = Jbig2Config::default();
    config.want_full_headers = true;
    config.symbol_mode = false;
    config.refine = false;
    config.generic.dpi = args.dpi;
    config.dpi = args.dpi;
    config.halftone.grid_size_m = args.grid_size;
    config.halftone.quant_levels_n = args.levels;
    config.halftone.sharpening_l = args.sharpening;
    config.halftone.template = args.template;
    config.halftone.lossless = args.lossless;
    config.halftone.gray_mmr = matches!(args.gray_coding, GrayCodingMode::Mmr);
    config.halftone.dict_mmr = matches!(args.dict_coding, DictCodingMode::Mmr);

    let region = GenericRegionParams::new(gray.width(), gray.height(), args.dpi);

    let standalone = encode_halftone_document_auto(&bitimage, &config, &region, 0, Some(1))
        .context("failed to encode standalone halftone JBIG2 document")?;
    fs::write(&args.output, &standalone)
        .with_context(|| format!("failed to write {}", args.output.display()))?;

    let (globals, fragment) =
        encode_halftone_pdf_split_auto(&bitimage, &config, &region, 1, Some(1))
            .context("failed to encode JBIG2 halftone PDF split stream")?;
    create_pdf(
        &args.pdf_output,
        Some(&globals),
        &fragment,
        gray.width(),
        gray.height(),
        args.dpi,
    )?;

    if let Ok(preview) = halftone_bilevel_preview(&bitimage, &config) {
        write_bitimage_pbm(&args.preview_pbm, &preview)?;
    }

    run_jbig2dec(&args.output, &args.decoded_pbm)?;
    run_pdftoppm(&args.pdf_output)?;

    println!("prepared: {}", args.prepared_pbm.display());
    println!("jb2: {}", args.output.display());
    println!("decoded: {}", args.decoded_pbm.display());
    println!("pdf: {}", args.pdf_output.display());
    println!("preview: {}", args.preview_pbm.display());
    Ok(())
}

fn prepare_bilevel(gray: &GrayImage, mode: &DitherMode, threshold: u8) -> Vec<u8> {
    match mode {
        DitherMode::Threshold => gray
            .pixels()
            .map(|pixel| if pixel[0] < threshold { 255 } else { 0 })
            .collect(),
        DitherMode::Ordered => ordered_dither(gray, threshold),
        DitherMode::Floyd => floyd_steinberg(gray, threshold),
    }
}

fn ordered_dither(gray: &GrayImage, threshold: u8) -> Vec<u8> {
    const BAYER_8X8: [[u8; 8]; 8] = [
        [0, 48, 12, 60, 3, 51, 15, 63],
        [32, 16, 44, 28, 35, 19, 47, 31],
        [8, 56, 4, 52, 11, 59, 7, 55],
        [40, 24, 36, 20, 43, 27, 39, 23],
        [2, 50, 14, 62, 1, 49, 13, 61],
        [34, 18, 46, 30, 33, 17, 45, 29],
        [10, 58, 6, 54, 9, 57, 5, 53],
        [42, 26, 38, 22, 41, 25, 37, 21],
    ];

    let mut out = Vec::with_capacity((gray.width() * gray.height()) as usize);
    for y in 0..gray.height() {
        for x in 0..gray.width() {
            let px = gray.get_pixel(x, y)[0] as u16;
            let local = (BAYER_8X8[(y % 8) as usize][(x % 8) as usize] as u16 * 255) / 63;
            let cutoff = ((threshold as u16) + local) / 2;
            out.push(if px < cutoff { 255 } else { 0 });
        }
    }
    out
}

fn floyd_steinberg(gray: &GrayImage, threshold: u8) -> Vec<u8> {
    let width = gray.width() as usize;
    let height = gray.height() as usize;
    let mut work: Vec<f32> = gray.pixels().map(|p| p[0] as f32).collect();
    let mut out = vec![0u8; work.len()];

    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            let old = work[idx];
            let new = if old < threshold as f32 { 0.0 } else { 255.0 };
            let error = old - new;
            out[idx] = if new == 0.0 { 255 } else { 0 };

            if x + 1 < width {
                work[idx + 1] += error * 7.0 / 16.0;
            }
            if y + 1 < height {
                if x > 0 {
                    work[idx + width - 1] += error * 3.0 / 16.0;
                }
                work[idx + width] += error * 5.0 / 16.0;
                if x + 1 < width {
                    work[idx + width + 1] += error * 1.0 / 16.0;
                }
            }
        }
    }

    out
}

fn create_pdf(
    path: &Path,
    jbig2_globals: Option<&[u8]>,
    jbig2_fragment: &[u8],
    width: u32,
    height: u32,
    dpi: u32,
) -> Result<()> {
    let width_pts = width as f32 * 72.0 / dpi as f32;
    let height_pts = height as f32 * 72.0 / dpi as f32;
    let globals_id = jbig2_globals.map(|_| 4usize);
    let image_id = if globals_id.is_some() { 5usize } else { 4usize };
    let content_id = image_id + 1;

    let content_stream = format!(
        "q\n{:.4} 0 0 {:.4} 0 0 cm\n/Im0 Do\nQ\n",
        width_pts, height_pts
    );

    let image_dict = if let Some(globals_ref) = globals_id {
        format!(
            "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /JBIG2Decode /DecodeParms << /JBIG2Globals {} 0 R >> /Length {} >>",
            width,
            height,
            globals_ref,
            jbig2_fragment.len()
        )
    } else {
        format!(
            "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /JBIG2Decode /Length {} >>",
            width,
            height,
            jbig2_fragment.len()
        )
    };

    let mut objects: Vec<(usize, Vec<u8>)> = vec![
        (
            1,
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        ),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        ),
        (
            3,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {:.4} {:.4}] /Resources << /XObject << /Im0 {} 0 R >> >> /Contents {} 0 R >>",
                width_pts, height_pts, image_id, content_id
            )
            .into_bytes(),
        ),
    ];

    if let Some(globals) = jbig2_globals {
        objects.push((
            4,
            make_stream_object_bytes(b"<< /Length ".to_vec(), globals),
        ));
    }

    objects.push((
        image_id,
        make_stream_with_dict(image_dict.as_bytes(), jbig2_fragment),
    ));
    objects.push((
        content_id,
        make_stream_with_dict(
            format!("<< /Length {} >>", content_stream.len()).as_bytes(),
            content_stream.as_bytes(),
        ),
    ));

    let mut pdf = b"%PDF-1.4\n%\xFF\xFF\xFF\xFF\n".to_vec();
    let mut offsets = vec![0usize];
    for (id, obj) in &objects {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", id).as_bytes());
        pdf.extend_from_slice(obj);
        pdf.extend_from_slice(b"\nendobj\n");
    }

    let xref_offset = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_offset
        )
        .as_bytes(),
    );

    fs::write(path, pdf).with_context(|| format!("failed to save {}", path.display()))?;
    Ok(())
}

fn make_stream_with_dict(dict: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(dict);
    out.extend_from_slice(b"\nstream\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"endstream");
    out
}

fn make_stream_object_bytes(prefix: Vec<u8>, data: &[u8]) -> Vec<u8> {
    let mut dict = prefix;
    dict.extend_from_slice(data.len().to_string().as_bytes());
    dict.extend_from_slice(b" >>");
    make_stream_with_dict(&dict, data)
}

fn run_jbig2dec(input: &Path, output: &Path) -> Result<()> {
    let status = Command::new("jbig2dec")
        .arg("--format")
        .arg("pbm")
        .arg("--output")
        .arg(output)
        .arg(input)
        .status()
        .context("failed to spawn jbig2dec")?;
    if !status.success() {
        bail!("jbig2dec failed for {}", input.display());
    }
    Ok(())
}

fn run_pdftoppm(input: &Path) -> Result<()> {
    let prefix = input.with_extension("");
    let status = Command::new("pdftoppm")
        .arg("-f")
        .arg("1")
        .arg("-singlefile")
        .arg(input)
        .arg(&prefix)
        .status()
        .context("failed to spawn pdftoppm")?;
    if !status.success() {
        bail!("pdftoppm failed for {}", input.display());
    }
    Ok(())
}

fn write_bitimage_pbm(path: &Path, image: &BitImage) -> Result<()> {
    let mut unpacked = vec![0u8; image.width * image.height];
    for y in 0..image.height {
        for x in 0..image.width {
            unpacked[y * image.width + x] = if image.get_usize(x, y) { 255 } else { 0 };
        }
    }
    write_pbm(path, &unpacked, image.width, image.height)
}

fn write_pbm(path: &Path, unpacked: &[u8], width: usize, height: usize) -> Result<()> {
    if unpacked.len() != width * height {
        bail!("PBM buffer size mismatch");
    }

    let row_bytes = width.div_ceil(8);
    let mut out = Vec::with_capacity(32 + row_bytes * height);
    out.extend_from_slice(format!("P4\n{} {}\n", width, height).as_bytes());

    for y in 0..height {
        for byte_x in 0..row_bytes {
            let mut byte = 0u8;
            for bit in 0..8 {
                let x = byte_x * 8 + bit;
                if x < width && unpacked[y * width + x] != 0 {
                    byte |= 1 << (7 - bit);
                }
            }
            out.push(byte);
        }
    }

    fs::write(path, out).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}
