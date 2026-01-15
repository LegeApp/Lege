use clap::Parser;
use color_ops::binarization::binarize_image_raw;
use color_ops::colorstructs::{BinarizationOptions, GrayscaleOptions};
use color_ops::grayscale::process_to_normalized_grayscale;
use image::{ImageBuffer, Luma};
use std::error::Error;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Input image file
    #[arg(short, long)]
    input: PathBuf,

    /// Output directory
    #[arg(short, long)]
    output_dir: PathBuf,

    /// Binarization threshold for the simple method
    #[arg(long, default_value_t = 130)]
    threshold: u8,
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    // --- Method 1: Grayscale + Simple Threshold ---
    println!("Running Method 1: Grayscale + Simple Threshold");
    let grayscale_options = GrayscaleOptions::default();
    let normalized_gray =
        process_to_normalized_grayscale(cli.input.to_str().unwrap(), &grayscale_options)?;

    let (width, height) = (normalized_gray.dim().1, normalized_gray.dim().0);
    let mut binarized_simple: ImageBuffer<Luma<u8>, Vec<u8>> =
        ImageBuffer::new(width as u32, height as u32);

    for (x, y, pixel) in binarized_simple.enumerate_pixels_mut() {
        let gray_value = normalized_gray[[y as usize, x as usize]];
        if gray_value < cli.threshold {
            *pixel = Luma([0]); // Black
        } else {
            *pixel = Luma([255]); // White
        }
    }

    let simple_output_path = cli.output_dir.join("binarized_simple_threshold.png");
    binarized_simple.save(&simple_output_path)?;
    println!("Saved simple threshold result to: {:?}", simple_output_path);

    // --- Method 2: Precision Binarize ---
    println!("\nRunning Method 2: Precision Binarize");
    let input_image = image::open(&cli.input)?.to_rgb8();
    let (width, height) = input_image.dimensions();
    let image_data = input_image.into_raw();

    let binarization_options = BinarizationOptions::default();
    let binarized_precision_raw = binarize_image_raw(
        &image_data,
        width as usize,
        height as usize,
        &binarization_options,
    );

    let precision_output_path = cli.output_dir.join("binarized_precision.png");
    let binarized_precision_img: ImageBuffer<Luma<u8>, _> =
        ImageBuffer::from_raw(width, height, binarized_precision_raw).unwrap();
    binarized_precision_img.save(&precision_output_path)?;
    println!(
        "Saved precision binarize result to: {:?}",
        precision_output_path
    );

    println!("\nComparison complete.");

    Ok(())
}
