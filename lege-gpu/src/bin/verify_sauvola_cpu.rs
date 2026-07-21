use anyhow::Result;
use lege_gpu::vision::SauvolaCpuProcessor;
fn main() -> Result<()> {
    let model = std::env::args().nth(1).unwrap();
    let image_path = std::env::args().nth(2).unwrap();
    let out = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "sauvola_cpu_out.png".into());
    let proc = SauvolaCpuProcessor::from_model_path(&model)?;
    let img = image::open(&image_path)?.to_rgb8();
    println!("input {}x{}", img.width(), img.height());
    for i in 0..3 {
        let t = std::time::Instant::now();
        let mask = proc.binarize_rgb(&img)?;
        println!("run {i}: {:?}", t.elapsed());
        if i == 0 {
            mask.save(&out)?;
        }
    }
    Ok(())
}
