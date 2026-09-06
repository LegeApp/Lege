//! Score an existing JP2 stream against its source, where the reader sees it.
//!
//! Prints one SSIMULACRA2 value: at source resolution by default, or
//! conditioned into a display box (`eink:WxH` / `tablet:WxH`) the way the
//! encoder's display target measures it.
//!
//! ```text
//! cargo run --release --example score_stream -- source.png stream.jp2 [eink:WxH|tablet:WxH]
//! ```

use std::path::Path;

use jp2lam::{DisplayColor, DisplayProfile, Image, StreamEvaluator};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        return Err("usage: score_stream <source.png> <stream.jp2> [eink:WxH|tablet:WxH]".into());
    }
    let source = load_png(Path::new(&args[0]))?;
    let stream = std::fs::read(&args[1]).map_err(|err| format!("read {}: {err}", args[1]))?;
    let display = args.get(2).map(|spec| parse_display(spec)).transpose()?;

    let view = source.as_view().map_err(|err| err.to_string())?;
    let mut evaluator = match display {
        Some(display) => StreamEvaluator::for_display(view, display),
        None => StreamEvaluator::from_view(view),
    }
    .map_err(|err| err.to_string())?;
    let observation = evaluator
        .score_stream(&stream)
        .map_err(|err| err.to_string())?;
    println!("{:.3}", observation.score);
    Ok(())
}

fn parse_display(spec: &str) -> Result<DisplayProfile, String> {
    let (mode, size) = spec
        .split_once(':')
        .ok_or_else(|| format!("display spec `{spec}` is not eink:WxH or tablet:WxH"))?;
    let (w, h) = size
        .split_once('x')
        .ok_or_else(|| format!("display size `{size}` is not WxH"))?;
    let color = match mode {
        "eink" => DisplayColor::Eink,
        "tablet" => DisplayColor::Tablet,
        other => return Err(format!("unknown display mode `{other}`")),
    };
    Ok(DisplayProfile {
        max_width: w.parse().map_err(|_| format!("bad width `{w}`"))?,
        max_height: h.parse().map_err(|_| format!("bad height `{h}`"))?,
        color,
        resample_source: false,
    })
}

fn load_png(path: &Path) -> Result<Image, String> {
    let dynamic = image::open(path).map_err(|err| format!("load {}: {err}", path.display()))?;
    if dynamic.color().has_color() {
        let rgb = dynamic.to_rgb8();
        let (width, height) = rgb.dimensions();
        Image::from_rgb_bytes(width, height, rgb.as_raw()).map_err(|err| err.to_string())
    } else {
        let gray = dynamic.to_luma8();
        let (width, height) = gray.dimensions();
        Image::from_gray_bytes(width, height, gray.as_raw()).map_err(|err| err.to_string())
    }
}
