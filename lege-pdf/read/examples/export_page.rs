//! Export one page of a PDF to PNG or JPEG.
//!
//! ```text
//! cargo run -p lege-pdf-read --example export_page -- in.pdf 0 out.png 300
//! ```

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, page, output] = &args[..4.min(args.len())] else {
        eprintln!("usage: export_page <in.pdf> <page-index> <out.png|out.jpg> [dpi]");
        std::process::exit(2);
    };
    let dpi: f64 = args.get(4).map_or(Ok(150.0), |value| value.parse())?;
    let page: u32 = page.parse()?;

    let bytes: std::sync::Arc<[u8]> = std::fs::read(input)?.into();
    let session = lege_pdf_read::RenderSession::open(bytes, None)?;

    let options = if output.ends_with(".jpg") || output.ends_with(".jpeg") {
        lege_pdf_read::ExportOptions::jpeg(dpi, 85)
    } else {
        lege_pdf_read::ExportOptions::png(dpi)
    };

    let (width, height) = session.page_pixel_size(page, dpi)?;
    let encoded = session.export_page(page, &options)?;
    std::fs::write(output, &encoded)?;
    println!(
        "{output}: {width}x{height} at {dpi} DPI, {} bytes",
        encoded.len()
    );
    Ok(())
}
