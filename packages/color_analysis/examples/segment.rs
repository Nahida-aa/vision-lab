//! 命令行：对一张图跑 `segment_by_color`，打印每个颜色连通域的统计。
//!
//!   cargo run -p color_analysis --example segment -- <image.png> [quant_bits] [merge_distance] [min_area_ratio]
//!
//! 输出每个区域的：代表色、包围盒 (x,y,w,h)、像素数、占全图比例。

use anyhow::Context as _;
use color_analysis::{SegmentOpts, segment_by_color};
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        anyhow::bail!("用法: segment <image.png> [quant_bits] [merge_distance] [min_area_ratio]");
    }
    let path = &args[1];
    let d = SegmentOpts::default();
    let opts = SegmentOpts {
        quant_bits: args
            .get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(d.quant_bits),
        merge_distance: args
            .get(3)
            .and_then(|s| s.parse().ok())
            .unwrap_or(d.merge_distance),
        min_area_ratio: args
            .get(4)
            .and_then(|s| s.parse().ok())
            .unwrap_or(d.min_area_ratio),
    };

    let img = image::open(Path::new(path))
        .with_context(|| format!("读取图片失败: {path}"))?
        .to_rgb8();
    let (w, h) = img.dimensions();
    let total = (w * h) as f32;

    let regions = segment_by_color(&img, opts);
    println!(
        "图像 {} ({}x{})：分割出 {} 个颜色区域（quant_bits={}, merge_distance={}, min_area_ratio={}）",
        path,
        w,
        h,
        regions.len(),
        opts.quant_bits,
        opts.merge_distance,
        opts.min_area_ratio
    );
    for (i, r) in regions.iter().enumerate() {
        let ratio = r.pixel_count as f32 / total * 100.0;
        println!(
            "  #{:<2} color=[{:3},{:3},{:3}] rect=({:3},{:3},{:3},{:3}) pixels={:>6} ({:5.2}%)",
            i,
            r.color[0],
            r.color[1],
            r.color[2],
            r.rect.0,
            r.rect.1,
            r.rect.2,
            r.rect.3,
            r.pixel_count,
            ratio
        );
    }
    Ok(())
}
