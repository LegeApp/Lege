use color_ops::{process_image, GrayscaleOptions};
use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        println!("Usage: minimal_tester <input_folder> <output_folder>");
        return;
    }
    let input_dir = &args[1];
    let output_dir = &args[2];
    let input_path = Path::new(input_dir);
    let output_path = Path::new(output_dir);
    fs::create_dir_all(output_path).expect("Failed to create output directory");

    let valid_exts = ["png", "jpg", "jpeg"];
    let mut found = false;
    for entry in fs::read_dir(input_path).expect("Failed to read input directory") {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                println!("Error reading entry: {e}");
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !valid_exts.contains(&ext.as_str()) {
            continue;
        }
        found = true;
        println!("\n--- Processing {:?} ---", path);

        let options = GrayscaleOptions::default();

        let result = process_image(path.to_str().unwrap(), &options);
        match result {
            Ok(output_data) => {
                // Save the processed image
                let output_filename =
                    format!("processed_{}", path.file_name().unwrap().to_str().unwrap());
                let output_filepath = output_path.join(output_filename);
                if let Err(e) = fs::write(&output_filepath, &output_data) {
                    println!("✗ Failed to save {:?}: {}", path.file_name().unwrap(), e);
                } else {
                    println!(
                        "✓ Success: {:?} -> {:?}",
                        path.file_name().unwrap(),
                        output_filepath.file_name().unwrap()
                    );
                }
            }
            Err(e) => println!("✗ Error: {:?}: {}", path.file_name().unwrap(), e),
        }
    }
    if !found {
        println!("No images found in input folder.");
    }
}
