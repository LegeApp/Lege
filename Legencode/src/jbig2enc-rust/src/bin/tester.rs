use anyhow::{anyhow, Result};
use clap::Parser;
use env_logger::Builder;
use env_logger::Env;
use jbig2enc_rust::jbig2enc::{encode_generic_region, Jbig2Encoder};
use jbig2enc_rust::jbig2pdf;
use jbig2enc_rust::jbig2pdf::{Jbig2Input, Jbig2Roi};
use jbig2enc_rust::jbig2structs::Jbig2Config;
use jbig2enc_rust::jbig2sym::array_to_bitimage;
use log::info;
use log::LevelFilter;
use ndarray::Array2;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Input PBM file path
    #[clap(short, long, value_parser, default_value = "./test_image.pbm")]
    input: String,

    /// Output file path (JBIG2 for standalone, PDF for --pdf mode)
    #[clap(short, long, value_parser, default_value = "./output.jb2")]
    output: String,

    /// Enable PDF mode (generates fragments, tester wraps for jbig2dec)
    #[clap(short, long)]
    pdf_mode: bool,

    /// Enable symbol mode for encoding (default is generic region)
    #[clap(short, long)]
    symbol_mode: bool,

    /// Enable console output in addition to file logging
    #[clap(long)]
    console: bool,

    /// Override the log directory [default: ./logs]
    #[clap(long, value_parser)]
    log_dir: Option<String>,

    /// Override the log file name [default: jbig2_trace.log]
    #[clap(long, value_parser)]
    log_file: Option<String>,
}

fn init_logging(args: &Args) -> Result<()> {
    let default_log_level = if cfg!(debug_assertions) {
        // In debug builds, default to debug level
        "debug"
    } else {
        // In release builds, default to info level
        "info"
    };

    let mut builder = Builder::from_env(Env::new().default_filter_or(default_log_level));

    // Set appropriate log levels for our crates
    if cfg!(debug_assertions) {
        // In debug builds, enable debug logging for our crates
        builder.filter_module("jbig2", LevelFilter::Debug);
        builder.filter_module("jbig2_arith", LevelFilter::Debug);
    } else {
        // In release builds, respect the default level from RUST_LOG
        builder.filter_module("jbig2", LevelFilter::Info);
        builder.filter_module("jbig2_arith", LevelFilter::Info);
    }
    builder.filter_module("jbig2::jbig2enc", LevelFilter::Debug);
    builder.filter_module("jbig2_arith", LevelFilter::Debug); // Matches jbig2arith.rs module

    // Enable trace logs for specific modules only if trace features are enabled
    #[cfg(any(feature = "trace_encoder", feature = "trace_arith"))]
    {
        if cfg!(feature = "trace_encoder") {
            builder.filter_module("jbig2::jbig2enc", LevelFilter::Trace);
        }
        if cfg!(feature = "trace_arith") {
            builder.filter_module("jbig2_arith", LevelFilter::Trace);
        }
    }

    // If console is disabled, suppress console output (logs go to file only if configured)
    if !args.console {
        builder.is_test(true);
    }

    // Set up file logging if log_dir and log_file are specified
    if let Some(dir) = &args.log_dir {
        let log_file = args.log_file.as_deref().unwrap_or("jbig2_trace.log");
        let log_path = Path::new(dir).join(log_file);
        std::fs::create_dir_all(dir)?;
        let file = File::create(&log_path)?;
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }

    builder.init();
    info!(
        "Logging initialized with console={}, log_dir={:?}, log_file={:?}",
        args.console, args.log_dir, args.log_file
    );
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Set environment variables for logging configuration
    if args.console {
        std::env::set_var("RUST_LOG_CONSOLE", "1");
    }
    if let Some(dir) = &args.log_dir {
        std::env::set_var("JBIG2_LOG_DIR", dir);
    }
    if let Some(file) = &args.log_file {
        std::env::set_var("JBIG2_LOG_FILE", file);
    }

    // Initialize logging
    init_logging(&args)?;

    info!("JBIG2 Encoder Tester");
    info!(
        "Command line: {}",
        std::env::args().collect::<Vec<_>>().join(" ")
    );

    if args.pdf_mode {
        info!("PDF Mode: ENABLED (fragments will be wrapped for jbig2dec)");
    }

    if args.symbol_mode {
        info!("Encoding Mode: Symbol Dictionary");
    } else {
        info!("Encoding Mode: Generic Region");
    }

    // 1. Read and parse the input PBM image
    info!("Reading PBM file: {}", args.input);
    let (width, height, data) = read_pbm(&args.input)?;
    info!(
        "PBM dimensions: {}x{} ({} bytes)",
        width,
        height,
        data.len()
    );

    // 2. Convert raw PBM data to the library's internal BitImage format
    let image_array = Array2::<u8>::from_shape_fn((height, width), |(r, c)| {
        let byte_idx = r * ((width + 7) / 8) + (c / 8);
        let bit_idx_in_byte = c % 8;
        ((data[byte_idx] >> (7 - bit_idx_in_byte)) & 1) as u8
    });
    info!("Converted PBM to ndarray::Array2<u8>.");

    // 3. Configure the encoder
    let config = Jbig2Config {
        dpi: 300,
        symbol_mode: args.symbol_mode,
        ..Jbig2Config::default()
    };
    println!("[DEBUG] Jbig2Config.mmr = {}", config.generic.mmr);

    info!(
        "Using encoder config: symbol_mode={}, want_full_headers={}",
        config.symbol_mode, config.want_full_headers
    );

    // 4. Initialize and use the Jbig2Encoder
    let encoded_data = if args.pdf_mode {
        let mut encoder = Jbig2Encoder::new(&config);
        encoder.add_page(&image_array)?;
        info!("Finalizing JBIG2 encoding...");
        encoder.flush()?
    } else {
        info!("Standalone mode: Using encode_generic_region to produce full JBIG2 file.");
        let bit_image = array_to_bitimage(&image_array);
        encode_generic_region(&bit_image, &config)?
    };

    // 5. Write the output file
    if args.pdf_mode {
        let output_path = args.output.replace(".jb2", ".pdf");
        info!("PDF Mode: Creating PDF file at {}", output_path);

        let jbig2_roi = Jbig2Roi {
            stream: encoded_data,
            width: width as u32,
            height: height as u32,
            xres: 300,
            yres: 300,
            pdf_x: 0.0,
            pdf_y: 0.0,
            pdf_width: width as f32,
            pdf_height: height as f32,
            page_index: 0,
        };

        let jbig2_input = Jbig2Input::from_memory(None, vec![jbig2_roi]);
        let page_dims = vec![(width as f32, height as f32)];

        jbig2pdf::create_jbig2_pdf(jbig2_input, &output_path, &page_dims)?;
        info!("Successfully created PDF with JBIG2 fragment.");
    } else {
        let mut file = File::create(&args.output)?;
        file.write_all(&encoded_data)?;
        info!("Successfully wrote full JBIG2 file to {}", args.output);
    }

    info!("Encoded data written successfully.");
    Ok(())
}

fn read_pbm(path: &str) -> Result<(usize, usize, Vec<u8>)> {
    let mut file = File::open(path)?;
    let mut reader = BufReader::new(&mut file);

    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim() != "P4" {
        return Err(anyhow!("Unsupported PBM magic number: {}", line.trim()));
    }
    info!("PBM magic number: {}", line.trim());

    line.clear();
    loop {
        reader.read_line(&mut line)?;
        let trimmed_line = line.trim();
        if !trimmed_line.starts_with('#') && !trimmed_line.is_empty() {
            break;
        }
        line.clear();
    }
    let parts: Vec<&str> = line.trim().split_whitespace().collect();
    if parts.len() != 2 {
        return Err(anyhow!("Invalid PBM dimensions line: {}", line.trim()));
    }
    let width = parts[0].parse::<usize>()?;
    let height = parts[1].parse::<usize>()?;
    info!("PBM dimensions: {}x{}", width, height);

    let current_file_pos = reader.stream_position()?;
    file.seek(SeekFrom::Start(current_file_pos))?;

    let width_in_bytes = (width + 7) / 8;
    let expected_data_len = height * width_in_bytes;
    info!(
        "Calculated PBM data length ( H * ((W+7)/8) ): {} bytes",
        expected_data_len
    );
    let mut data = vec![0u8; expected_data_len];
    file.read_exact(&mut data)?;

    Ok((width, height, data))
}
