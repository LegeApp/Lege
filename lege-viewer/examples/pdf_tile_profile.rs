//! Exercise the production viewer raster worker with a repeated visible-tile set.
//!
//! `LEGE_PDF_IMAGE_RENDERER=cpu|gpu|auto cargo run --release -p lege-viewer \
//!   --example pdf_tile_profile -- file.pdf 0 0 7 12`

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lege_viewer::PageIndex;
use lege_viewer::document::pdf_engine::PdfEngine;
use lege_viewer::document::{
    CancellationFlag, ColorMode, DocumentEngine, RasterPass, TILE_SIZE, TileCoord, TileDemand,
    ZoomBucket,
};
use lege_viewer::geometry::{Affine, RectF, RectI};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().ok_or(
        "usage: pdf_tile_profile <file.pdf> [zero-based-page] [zoom-bucket] [warm-passes] [tiles]",
    )?);
    let page = parse_or(&mut args, 0u32);
    let bucket = ZoomBucket(parse_or(&mut args, 0i16));
    let passes = parse_or(&mut args, 7usize).max(1);
    let tile_limit = parse_or(&mut args, 12usize).max(1);

    let open_started = Instant::now();
    let engine = PdfEngine::open(&path, None)?;
    let open_elapsed = open_started.elapsed();
    let page = PageIndex(page);
    let geometry = *engine
        .descriptor()
        .page_geometries
        .get(page.0 as usize)
        .ok_or("page is out of range")?;
    let cancellation = CancellationFlag::default();
    let compile_started = Instant::now();
    let artifacts = engine.compile_page(page, Affine::IDENTITY, &cancellation)?;
    let compile_elapsed = compile_started.elapsed();
    let demands = tile_demands(page, geometry, bucket, tile_limit);
    let mut worker = engine.create_raster_worker();

    let cold_started = Instant::now();
    let cold_checksum = render_pass(
        worker.as_mut(),
        &artifacts,
        bucket,
        &demands,
        1,
        &cancellation,
    )?;
    let cold_elapsed = cold_started.elapsed();

    let mut warm_times = Vec::with_capacity(passes);
    let mut warm_checksum = 0u64;
    for generation in 2..passes as u64 + 2 {
        let started = Instant::now();
        warm_checksum ^= render_pass(
            worker.as_mut(),
            &artifacts,
            bucket,
            &demands,
            generation,
            &cancellation,
        )?;
        warm_times.push(started.elapsed());
    }

    println!("file: {}", path.display());
    println!(
        "page: {}, bucket: {} (scale {:.4}), tiles: {}, warm passes: {}",
        page.0,
        bucket.0,
        bucket.scale(),
        demands.len(),
        passes
    );
    println!("open:              {:.3} ms", millis(open_elapsed));
    println!("compile:           {:.3} ms", millis(compile_elapsed));
    println!("cold tile set:     {:.3} ms", millis(cold_elapsed));
    println!(
        "warm tile median:  {:.3} ms",
        millis(median(&mut warm_times))
    );
    println!("routing: {:?}", engine.image_renderer_telemetry());
    println!("checksum: {cold_checksum:016x}/{warm_checksum:016x}");
    Ok(())
}

fn parse_or<T: std::str::FromStr>(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    default: T,
) -> T {
    args.next()
        .and_then(|value| value.to_string_lossy().parse::<T>().ok())
        .unwrap_or(default)
}

fn tile_demands(
    page: PageIndex,
    geometry: lege_viewer::document::PageGeometry,
    bucket: ZoomBucket,
    limit: usize,
) -> Vec<TileDemand> {
    let (page_width, page_height) = match geometry.rotation {
        90 | 270 => (geometry.crop.height, geometry.crop.width),
        _ => (geometry.crop.width, geometry.crop.height),
    };
    let page_view_box = RectF {
        x: 0.0,
        y: 0.0,
        width: page_width,
        height: page_height,
    };
    let scale = bucket.scale();
    let width = (page_width * scale).ceil().max(1.0) as u32;
    let height = (page_height * scale).ceil().max(1.0) as u32;
    let columns = width.div_ceil(TILE_SIZE);
    let rows = height.div_ceil(TILE_SIZE);
    let mut demands = Vec::with_capacity(limit.min((columns * rows) as usize));
    for y in 0..rows {
        for x in 0..columns {
            let device_x = x * TILE_SIZE;
            let device_y = y * TILE_SIZE;
            let tile_width = TILE_SIZE.min(width - device_x);
            let tile_height = TILE_SIZE.min(height - device_y);
            demands.push(TileDemand {
                page,
                coord: TileCoord {
                    x: x as i32,
                    y: y as i32,
                },
                page_device_rect: RectI {
                    x: device_x as i32,
                    y: device_y as i32,
                    width: tile_width,
                    height: tile_height,
                },
                page_document_rect: RectF {
                    x: f64::from(device_x) / scale,
                    y: f64::from(device_y) / scale,
                    width: f64::from(tile_width) / scale,
                    height: f64::from(tile_height) / scale,
                },
                distance_from_viewport: 0.0,
                visible: true,
                page_view_box,
                color_mode: ColorMode::Original,
                variant: 0,
            });
            if demands.len() == limit {
                return demands;
            }
        }
    }
    demands
}

fn render_pass(
    worker: &mut dyn lege_viewer::document::DocumentRasterWorker,
    artifacts: &lege_viewer::document::CompiledArtifacts,
    bucket: ZoomBucket,
    demands: &[TileDemand],
    generation: u64,
    cancellation: &CancellationFlag,
) -> Result<u64, lege_viewer::document::DocumentEngineError> {
    let mut checksum = 0xcbf2_9ce4_8422_2325u64;
    for demand in demands {
        let tile = worker.raster_tile(
            artifacts,
            bucket,
            *demand,
            RasterPass::Final,
            generation,
            cancellation,
        )?;
        for pixel in tile.pixels.pixels.iter() {
            checksum ^= u64::from(*pixel);
            checksum = checksum.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Ok(checksum)
}

fn median(values: &mut [Duration]) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
