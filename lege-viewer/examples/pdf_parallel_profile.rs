//! Measure the production viewer's parallel PDF raster path.
//!
//! `LEGE_PDF_IMAGE_RENDERER=cpu|auto cargo run --release -p lege-gui \
//!   --example pdf_parallel_profile -- file.pdf 0 8 0 4 8 5`
//!
//! Arguments are: file, zero-based first page, page count, zoom bucket,
//! tiles per page, raster threads, and total passes. The first pass is cold;
//! the remaining passes report a warm median using persistent raster workers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

use lege_viewer::PageIndex;
use lege_viewer::document::pdf_engine::PdfEngine;
use lege_viewer::document::{
    CancellationFlag, ColorMode, CompiledArtifacts, DocumentEngine, RasterPass, TILE_SIZE,
    TileCoord, TileDemand, ZoomBucket,
};
use lege_viewer::geometry::{Affine, RectF, RectI};

#[derive(Clone)]
struct RasterJob {
    artifacts: Arc<CompiledArtifacts>,
    demand: TileDemand,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().ok_or(
        "usage: pdf_parallel_profile <file.pdf> [first-page] [pages] [bucket] \
         [tiles-per-page] [threads] [passes]",
    )?);
    let first_page = parse_or(&mut args, 0u32);
    let page_count = parse_or(&mut args, 8u32).max(1);
    let bucket = ZoomBucket(parse_or(&mut args, 0i16));
    // Zero means one whole-page request, useful for conversion/export
    // throughput. Positive values exercise the viewer's 256px tile path.
    let tiles_per_page = parse_or(&mut args, 4usize);
    let threads = parse_or(&mut args, 4usize).max(1);
    let passes = parse_or(&mut args, 5usize).max(2);

    let engine = Arc::new(PdfEngine::open(&path, None)?);
    let cancellation = CancellationFlag::default();
    let mut jobs = Vec::new();
    let compile_started = Instant::now();
    for number in first_page..first_page.saturating_add(page_count) {
        let page = PageIndex(number);
        let geometry = *engine
            .descriptor()
            .page_geometries
            .get(number as usize)
            .ok_or("page is out of range")?;
        let artifacts = engine.compile_page(page, Affine::IDENTITY, &cancellation)?;
        jobs.extend(
            tile_demands(page, geometry, bucket, tiles_per_page)
                .into_iter()
                .map(|demand| RasterJob {
                    artifacts: Arc::clone(&artifacts),
                    demand,
                }),
        );
    }
    let compile_elapsed = compile_started.elapsed();
    let jobs = Arc::new(jobs);
    let cursor = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(threads + 1));
    let (result_tx, result_rx) = mpsc::channel::<(usize, Result<(usize, u64), String>)>();
    let mut elapsed = Vec::with_capacity(passes);
    let mut pass_checksums = Vec::with_capacity(passes);

    std::thread::scope(|scope| -> Result<(), Box<dyn std::error::Error>> {
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let engine = Arc::clone(&engine);
            let jobs = Arc::clone(&jobs);
            let cursor = Arc::clone(&cursor);
            let barrier = Arc::clone(&barrier);
            let result_tx = result_tx.clone();
            let cancellation = cancellation.clone();
            handles.push(scope.spawn(move || {
                let mut worker = engine.create_raster_worker();
                for pass in 0..passes {
                    barrier.wait();
                    let mut checksum = 0u64;
                    let mut completed = 0usize;
                    let result = loop {
                        let index = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(index) else {
                            break Ok((completed, checksum));
                        };
                        let tile = match worker.raster_tile(
                            &job.artifacts,
                            bucket,
                            job.demand,
                            RasterPass::Final,
                            pass as u64 + 1,
                            &cancellation,
                        ) {
                            Ok(tile) => tile,
                            Err(error) => break Err(error.to_string()),
                        };
                        completed += 1;
                        let mut tile_checksum = 0xcbf2_9ce4_8422_2325u64;
                        for pixel in tile.pixels.pixels.iter() {
                            tile_checksum ^= u64::from(*pixel);
                            tile_checksum = tile_checksum.wrapping_mul(0x0000_0100_0000_01b3);
                        }
                        checksum ^= tile_checksum;
                    };
                    let _ = result_tx.send((pass, result));
                    barrier.wait();
                }
            }));
        }
        drop(result_tx);

        for pass in 0..passes {
            cursor.store(0, Ordering::Relaxed);
            let started = Instant::now();
            barrier.wait();
            let mut completed = 0usize;
            let mut checksum = 0u64;
            for _ in 0..threads {
                let (reported_pass, result) = result_rx.recv()?;
                if reported_pass != pass {
                    return Err(format!(
                        "worker reported pass {reported_pass} while collecting pass {pass}"
                    )
                    .into());
                }
                let (worker_completed, worker_checksum) = result
                    .map_err(|error| format!("raster worker failed during pass {pass}: {error}"))?;
                completed += worker_completed;
                checksum ^= worker_checksum;
            }
            barrier.wait();
            if completed != jobs.len() {
                return Err(
                    format!("pass {pass} completed {completed}/{} jobs", jobs.len()).into(),
                );
            }
            elapsed.push(started.elapsed());
            pass_checksums.push(checksum);
        }
        for handle in handles {
            handle.join().map_err(|_| "raster worker panicked")?;
        }
        Ok(())
    })?;

    let cold = elapsed[0];
    let warm = median(&mut elapsed[1..]);
    let warm_jobs_per_second = jobs.len() as f64 / warm.as_secs_f64();
    println!("file: {}", path.display());
    println!(
        "pages: {}..{}, bucket: {} (scale {:.4}), jobs: {} ({})",
        first_page,
        first_page + page_count,
        bucket.0,
        bucket.scale(),
        jobs.len(),
        if tiles_per_page == 0 {
            "whole pages".to_owned()
        } else {
            format!("{tiles_per_page} tiles/page")
        }
    );
    println!("threads: {threads}, passes: {passes}");
    println!("compile:            {:.3} ms", millis(compile_elapsed));
    println!("cold parallel set:  {:.3} ms", millis(cold));
    println!("warm set median:    {:.3} ms", millis(warm));
    let throughput_unit = if tiles_per_page == 0 {
        "pages/s"
    } else {
        "tiles/s"
    };
    println!("warm throughput:    {warm_jobs_per_second:.2} {throughput_unit}");
    println!("routing: {:?}", engine.image_renderer_telemetry());
    println!(
        "checksums: {}",
        pass_checksums
            .iter()
            .map(|checksum| format!("{checksum:016x}"))
            .collect::<Vec<_>>()
            .join("/")
    );
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
    if limit == 0 {
        return vec![TileDemand {
            page,
            coord: TileCoord { x: 0, y: 0 },
            page_device_rect: RectI {
                x: 0,
                y: 0,
                width,
                height,
            },
            page_document_rect: page_view_box,
            distance_from_viewport: 0.0,
            visible: true,
            page_view_box,
            color_mode: ColorMode::Original,
            variant: 0,
        }];
    }
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

fn median(values: &mut [Duration]) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
