// Color checker utility for verifying DjVu encoding/decoding accuracy

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ColorCheckerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid PPM format: {0}")]
    InvalidFormat(String),
    #[error("Parse error: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn distance(&self, other: &RgbColor) -> u32 {
        let dr = (self.r as i32 - other.r as i32).abs() as u32;
        let dg = (self.g as i32 - other.g as i32).abs() as u32;
        let db = (self.b as i32 - other.b as i32).abs() as u32;
        dr + dg + db
    }
}

impl std::fmt::Display for RgbColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RGB({}, {}, {})", self.r, self.g, self.b)
    }
}

#[derive(Debug)]
pub struct PpmData {
    pub width: u32,
    pub height: u32,
    pub max_val: u32,
    pub pixels: Vec<u8>,
}

#[derive(Debug)]
pub struct ColorAnalysis {
    pub total_pixels: u32,
    pub unique_colors: u32,
    pub color_counts: HashMap<RgbColor, u32>,
    pub most_common_color: Option<(RgbColor, u32)>,
    pub sample_pixels: Vec<RgbColor>,
}

impl ColorAnalysis {
    pub fn check_expected_color(&self, expected: &RgbColor, tolerance: u32) -> ColorCheckResult {
        // First check for exact match
        if let Some(&count) = self.color_counts.get(expected) {
            let percentage = (count as f64 / self.total_pixels as f64) * 100.0;
            return ColorCheckResult::ExactMatch {
                color: expected.clone(),
                count,
                percentage,
            };
        }

        // Look for colors within tolerance
        let mut close_colors = Vec::new();
        for (color, &count) in &self.color_counts {
            let distance = expected.distance(color);
            if distance <= tolerance {
                let percentage = (count as f64 / self.total_pixels as f64) * 100.0;
                close_colors.push((color.clone(), count, percentage, distance));
            }
        }

        if !close_colors.is_empty() {
            // Sort by distance (closest first)
            close_colors.sort_by_key(|(_, _, _, distance)| *distance);
            return ColorCheckResult::CloseMatch {
                expected: expected.clone(),
                closest: close_colors,
            };
        }

        // Find the 3 closest colors for diagnosis
        let mut all_colors: Vec<_> = self
            .color_counts
            .iter()
            .map(|(color, &count)| {
                let distance = expected.distance(color);
                let percentage = (count as f64 / self.total_pixels as f64) * 100.0;
                (color.clone(), count, percentage, distance)
            })
            .collect();

        all_colors.sort_by_key(|(_, _, _, distance)| *distance);
        all_colors.truncate(3);

        ColorCheckResult::NoMatch {
            expected: expected.clone(),
            closest: all_colors,
        }
    }
}

#[derive(Debug)]
pub enum ColorCheckResult {
    ExactMatch {
        color: RgbColor,
        count: u32,
        percentage: f64,
    },
    CloseMatch {
        expected: RgbColor,
        closest: Vec<(RgbColor, u32, f64, u32)>, // (color, count, percentage, distance)
    },
    NoMatch {
        expected: RgbColor,
        closest: Vec<(RgbColor, u32, f64, u32)>, // (color, count, percentage, distance)
    },
}

impl ColorCheckResult {
    pub fn is_acceptable(&self, min_percentage: f64) -> bool {
        match self {
            ColorCheckResult::ExactMatch { percentage, .. } => *percentage >= min_percentage,
            ColorCheckResult::CloseMatch { closest, .. } => {
                closest.iter().map(|(_, _, pct, _)| *pct).sum::<f64>() >= min_percentage
            }
            ColorCheckResult::NoMatch { .. } => false,
        }
    }

    pub fn print_result(&self) {
        match self {
            ColorCheckResult::ExactMatch {
                color,
                count,
                percentage,
            } => {
                println!(
                    "✅ Exact match found: {} - {} pixels ({:.1}%)",
                    color, count, percentage
                );
            }
            ColorCheckResult::CloseMatch { expected, closest } => {
                println!(
                    "🟡 Close match for {}: {} similar colors found",
                    expected,
                    closest.len()
                );
                for (color, count, percentage, distance) in closest {
                    println!(
                        "   {} - {} pixels ({:.1}%) - distance: {}",
                        color, count, percentage, distance
                    );
                }
            }
            ColorCheckResult::NoMatch { expected, closest } => {
                println!("❌ No match for {}", expected);
                println!("   Closest colors:");
                for (color, count, percentage, distance) in closest {
                    println!(
                        "   {} - {} pixels ({:.1}%) - distance: {}",
                        color, count, percentage, distance
                    );
                }
            }
        }
    }
}

pub fn read_ppm<P: AsRef<Path>>(filename: P) -> Result<PpmData, ColorCheckerError> {
    let file = File::open(filename)?;
    let mut reader = BufReader::new(file);

    // Read magic number
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let magic = line.trim();
    if magic != "P6" {
        return Err(ColorCheckerError::InvalidFormat(format!(
            "Expected P6, got {}",
            magic
        )));
    }

    // Skip comments and read dimensions
    line.clear();
    reader.read_line(&mut line)?;
    while line.trim().starts_with('#') {
        line.clear();
        reader.read_line(&mut line)?;
    }

    let dimensions: Vec<&str> = line.trim().split_whitespace().collect();
    if dimensions.len() != 2 {
        return Err(ColorCheckerError::Parse(format!(
            "Invalid dimensions line: {}",
            line
        )));
    }

    let width: u32 = dimensions[0]
        .parse()
        .map_err(|_| ColorCheckerError::Parse(format!("Invalid width: {}", dimensions[0])))?;
    let height: u32 = dimensions[1]
        .parse()
        .map_err(|_| ColorCheckerError::Parse(format!("Invalid height: {}", dimensions[1])))?;

    // Read max value
    line.clear();
    reader.read_line(&mut line)?;
    let max_val: u32 = line
        .trim()
        .parse()
        .map_err(|_| ColorCheckerError::Parse(format!("Invalid max value: {}", line)))?;

    // Read pixel data
    let expected_bytes = (width * height * 3) as usize;
    let mut pixels = vec![0u8; expected_bytes];
    reader.read_exact(&mut pixels)?;

    Ok(PpmData {
        width,
        height,
        max_val,
        pixels,
    })
}

pub fn analyze_colors(ppm_data: &PpmData) -> ColorAnalysis {
    let mut color_counts = HashMap::new();
    let mut sample_pixels = Vec::new();

    let total_pixels = ppm_data.width * ppm_data.height;

    // Process pixels in chunks of 3 (RGB)
    for chunk in ppm_data.pixels.chunks_exact(3) {
        let color = RgbColor::new(chunk[0], chunk[1], chunk[2]);
        *color_counts.entry(color.clone()).or_insert(0) += 1;

        // Collect first 10 pixels as samples
        if sample_pixels.len() < 10 {
            sample_pixels.push(color);
        }
    }

    let unique_colors = color_counts.len() as u32;

    // Find most common color
    let most_common_color = color_counts
        .iter()
        .max_by_key(|(_, count)| *count)
        .map(|(color, count)| (color.clone(), *count));

    ColorAnalysis {
        total_pixels,
        unique_colors,
        color_counts,
        most_common_color,
        sample_pixels,
    }
}

pub fn check_solid_color<P: AsRef<Path>>(
    ppm_path: P,
    expected_color: RgbColor,
    tolerance: u32,
    min_percentage: f64,
) -> Result<bool, ColorCheckerError> {
    let ppm_data = read_ppm(ppm_path)?;
    let analysis = analyze_colors(&ppm_data);

    println!("Image dimensions: {}x{}", ppm_data.width, ppm_data.height);
    println!("Total pixels: {}", analysis.total_pixels);
    println!("Unique colors: {}", analysis.unique_colors);

    let result = analysis.check_expected_color(&expected_color, tolerance);
    result.print_result();

    Ok(result.is_acceptable(min_percentage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb_color_distance() {
        let red = RgbColor::new(255, 0, 0);
        let blue = RgbColor::new(0, 0, 255);
        let light_red = RgbColor::new(250, 5, 5);

        assert_eq!(red.distance(&red), 0);
        assert_eq!(red.distance(&light_red), 15); // |255-250| + |0-5| + |0-5| = 15
        assert!(red.distance(&blue) > red.distance(&light_red));
    }

    #[test]
    fn test_color_check_result() {
        let result = ColorCheckResult::ExactMatch {
            color: RgbColor::new(255, 0, 0),
            count: 100,
            percentage: 95.0,
        };

        assert!(result.is_acceptable(90.0));
        assert!(!result.is_acceptable(99.0));
    }
}
