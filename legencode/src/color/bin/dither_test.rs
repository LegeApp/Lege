// src/bin/dither_test.rs
// Tester binary to compare dithering methods for binarization on document images.
// Uses linearization and grayscale conversion from the crate, then applies various dithering algorithms
// from the dither crate to produce binary images. Outputs PNG files for visual comparison.
// Also includes Sauvola binarization from binarization.rs for reference.

use clap::Parser;
use image::{GrayImage, ImageBuffer};
use dither_lib::ditherer::{Dither, floyd_steinberg_ditherer, atkinson_ditherer, burkes_ditherer, sierra3_ditherer, stucki_ditherer};

// Import from crate
use color_ops::linearize::{linearize_rgb_bytes_to_f32, linearized_rgb_to_grayscale};
use color_ops::binarization::precision_binarize;
use color_ops::colorstructs::BinarizationOptions;


/// Dither test utility for document images
#[derive(Parser, Debug)]
#[command(author, version, about = "Compare dithering methods for binarization on document images. Supports PNG, JPEG, and PBM/PNM/PPM.", long_about = None)]
struct Args {
    /// Input image file (PNG, JPEG, PBM, PNM, PPM)
    input: String,
}

fn main() {
    let args = Args::parse();
    let input_path = &args.input;

    // Load image using image crate (supports PNG, JPEG, PBM, PGM, PPM with pnm feature)
    let img = match image::open(input_path) {
        Ok(img) => img.to_rgb8(),
        Err(e) => {
            eprintln!("Failed to open image: {}", e);
            std::process::exit(1);
        }
    };
    let (width, height) = img.dimensions();
    let w = width as usize;
    let h = height as usize;

    // Convert image to grayscale using our library's functions
    let rgb = img.into_raw();
    let linear = linearize_rgb_bytes_to_f32(&rgb);
    let mut gray_u8 = vec![0u8; w * h];
    linearized_rgb_to_grayscale(&linear, &mut gray_u8);

    // Save grayscale for reference
    let gray_img: GrayImage = ImageBuffer::from_raw(width, height, gray_u8.clone())
        .expect("Failed to create grayscale image");
    gray_img.save("grayscale.png").expect("Failed to save grayscale.png");

    // Define dithering methods
    let methods = [
        ("floyd_steinberg", floyd_steinberg_ditherer()),
        ("atkinson", atkinson_ditherer()),
        ("burkes", burkes_ditherer()),
        ("sierra3", sierra3_ditherer()),
        ("stucki", stucki_ditherer()),
    ];

    // Prepare source image in f64 format (0.0-1.0)
    let mut src = Vec::with_capacity(w * h);
    for &pixel in &gray_u8 {
        src.push(pixel as f64 / 255.0);
    }

    // Apply each dithering method
    for (name, ditherer) in methods.into_iter() {
        let buffer = src.clone();

        // Create a quantizer function that returns (quantized, error)
        let quantize = |v: f64| if v >= 0.5 { (1.0, v - 1.0) } else { (0.0, v) };

        // Wrap buffer in Img<f64>
        let img = dither_lib::img::Img::new(buffer, width).expect("Failed to create Img");

        // Apply dithering
        let dithered = ditherer.dither(img, quantize);

        // Convert back to u8 (0 or 255)
        let bin: Vec<u8> = dithered.into_vec()
            .into_iter()
            .map(|v| if v >= 0.5 { 255 } else { 0 })
            .collect();

        // Save result
        let bin_img: GrayImage = ImageBuffer::from_vec(width, height, bin)
            .expect("Failed to create binary image");
        let out_path = format!("output_{}.png", name);
        bin_img.save(&out_path).expect("Failed to save output");
        println!("Saved dithered binary image: {}", out_path);
    }

    // For comparison: Apply Sauvola binarization from binarization.rs
    let options = BinarizationOptions {
        window_size: 15,  // Typical value
        k_factor: 0.2,    // Typical Sauvola k
        sauvola_r: 128.0, // Typical R (dynamic range / 2)
        invert: false,
        custom_threshold: None,
    };
    
    let mut bin = vec![0u8; w * h];
    precision_binarize(&gray_u8, w, h, &options, &mut bin);
    
    let sauvola_img: GrayImage = ImageBuffer::from_vec(width, height, bin)
        .expect("Failed to create Sauvola image");
    sauvola_img.save("output_sauvola.png")
        .expect("Failed to save output_sauvola.png");
    println!("Saved Sauvola binary image: output_sauvola.png");

    println!("\nTesting complete! Generated files:");
    println!("- grayscale.png: Input image in grayscale");
    println!("- output_*.png: Various dithering method results");
    println!("- output_sauvola.png: Reference Sauvola binarization");
    println!("\nCompare these files to evaluate which dithering method works best for your document.");
}