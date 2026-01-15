//! Example of converting a PNG file to JPEG using the toojpeg crate

use clap::Parser;
use image::GenericImageView;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use toojpeg::{encode_jpeg, EncodeOptions, ImageFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SamplingMode {
    /// Chroma 4:2:0 subsampling
    S420,
    /// Chroma 4:4:4 (no subsampling)
    S444,
}

impl std::str::FromStr for SamplingMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "420" => Ok(SamplingMode::S420),
            "444" => Ok(SamplingMode::S444),
            _ => Err(format!("Invalid sampling mode: {}. Use '420' or '444'", s)),
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input PNG file path
    input: PathBuf,

    /// Output JPEG file path (default: input filename with .jpg extension)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// JPEG quality (1-100, higher is better quality)
    #[arg(short, long, default_value_t = 90)]
    quality: u8,

    /// Chroma subsampling mode (420 or 444)
    #[arg(long, default_value = "420", value_parser = SamplingMode::from_str)]
    sampling_mode: SamplingMode,

    /// Output both original (4:4:4) and downsampled (4:2:0) JPEGs
    #[arg(long)]
    output_both: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Load the input image
    println!("Loading image: {}", args.input.display());
    let img = image::open(&args.input)?;

    // Get image dimensions
    let (width, height) = img.dimensions();
    println!("Image size: {}x{} pixels", width, height);

    // Convert to RGB if needed
    let rgb_img = img.to_rgb8();
    let pixels = rgb_img.as_raw();

    // Set up encoding options
    let base_options = EncodeOptions {
        width: width as u32,
        height: height as u32,
        format: ImageFormat::RGB,
        quality: args.quality,
        ..Default::default()
    };

    // Determine downsampling setting based on sampling_mode
    let downsample_enabled = match args.sampling_mode {
        SamplingMode::S420 => true,
        SamplingMode::S444 => false,
    };

    if args.output_both {
        // Encode with downsampling (4:2:0)
        let downsampled_options = EncodeOptions {
            downsample: true, // Always 4:2:0 for this branch
            ..base_options
        };
        let mut downsampled_output = Vec::new();
        encode_jpeg(pixels, downsampled_options, &mut downsampled_output)?;

        let mut downsampled_path = args.input.clone();
        downsampled_path.set_file_name(format!(
            "{}_downsampled.jpg",
            args.input.file_stem().unwrap().to_str().unwrap()
        ));
        File::create(&downsampled_path)?.write_all(&downsampled_output)?;
        println!(
            "Successfully wrote {} ({} bytes)",
            downsampled_path.display(),
            downsampled_output.len()
        );

        // Encode without downsampling (4:4:4)
        let original_options = EncodeOptions {
            downsample: false, // Always 4:4:4 for this branch
            ..base_options
        };
        let mut original_output = Vec::new();
        encode_jpeg(pixels, original_options, &mut original_output)?;

        let mut original_path = args.input.clone();
        original_path.set_file_name(format!(
            "{}_original.jpg",
            args.input.file_stem().unwrap().to_str().unwrap()
        ));
        File::create(&original_path)?.write_all(&original_output)?;
        println!(
            "Successfully wrote {} ({} bytes)",
            original_path.display(),
            original_output.len()
        );
    } else {
        let options = EncodeOptions {
            downsample: downsample_enabled,
            ..base_options
        };

        // Determine output path
        let output_path = match args.output {
            Some(path) => path,
            None => {
                let mut path = args.input.clone();
                path.set_extension("jpg");
                path
            }
        };

        // Encode to JPEG and save to file
        println!("Encoding to JPEG with quality {}...", args.quality);
        let mut output = Vec::new();
        encode_jpeg(pixels, options, &mut output)?;

        File::create(&output_path)?.write_all(&output)?;

        println!(
            "Successfully wrote {} ({} bytes)",
            output_path.display(),
            output.len()
        );
    }
    Ok(())
}
