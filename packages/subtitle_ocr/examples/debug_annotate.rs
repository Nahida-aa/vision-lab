//! 一次性调试工具：在字幕帧原图上标注三类区域，输出到文件。
//!
//! - 绿线：bottom_only ROI 上边界（y = 0.6 * H），ROI 即 [0.6H, H]，送 OCR 的区域。
//! - 蓝框：subtitle_only 过滤区间（y 中心须落在 [0.85H, 0.99H]），即 `center.y/H in [0.85,0.99]`。
//! - 红框：OCR 实际识别到的文字框（来自 `SubtitleOcr::ocr_image`，套用同一 OcrOptions）。
//!
//! 用法：
//!   cargo run -p subtitle_ocr --example debug_annotate -- <in.png> <out.png> [--subtitle-only]
//!
//! 当传入 `--subtitle-only` 时，OCR 用 subtitle_only=true，红框只显示落在
//! [0.85H,0.99H] 区间内的框，方便对比 subtitle-only 开/关的差异。

use image::{Rgba, RgbaImage};
use std::path::Path;
use subtitle_ocr::{OcrOptions, SubtitleOcr};

fn draw_hline(img: &mut RgbaImage, y: u32, color: Rgba<u8>) {
    let (w, _) = img.dimensions();
    for x in 0..w {
        img.put_pixel(x, y.min(img.height().saturating_sub(1)), color);
    }
}

fn draw_rect(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, color: Rgba<u8>) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let (ax, bx) = (x0.min(x1).max(0), x0.max(x1).min(w - 1));
    let (ay, by) = (y0.min(y1).max(0), y0.max(y1).min(h - 1));
    for x in ax..=bx {
        img.put_pixel(x as u32, ay as u32, color);
        img.put_pixel(x as u32, by as u32, color);
    }
    for y in ay..=by {
        img.put_pixel(ax as u32, y as u32, color);
        img.put_pixel(bx as u32, y as u32, color);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut pos = vec![];
    let mut subtitle_only = false;
    for a in &args[1..] {
        if a == "--subtitle-only" {
            subtitle_only = true;
        } else {
            pos.push(a.clone());
        }
    }
    if pos.len() < 2 {
        eprintln!("用法: debug_annotate <in.png> <out.png> [--subtitle-only]");
        std::process::exit(1);
    }
    let in_path = &pos[0];
    let out_path = &pos[1];

    let root = repo_root();
    let model_dir = root.join(rapidocr_ort::DEFAULT_MODEL_DIR);

    let img = image::open(Path::new(in_path)).expect("读图失败").to_rgb8();
    let (w, h) = img.dimensions();
    let mut out = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    // 复制原图作底
    for y in 0..h {
        for x in 0..w {
            let p = img.get_pixel(x, y);
            out.put_pixel(x, y, Rgba([p[0], p[1], p[2], 255]));
        }
    }

    let h_f = h as f32;

    // ---- 绿线：bottom_only ROI 上边界 y = 0.6H ----
    let roi_y = (h_f * 0.6) as u32;
    draw_hline(&mut out, roi_y, Rgba([0, 255, 0, 255]));

    // ---- 蓝框：subtitle_only 区间 [0.85H, 0.99H] ----
    let sub_y0 = (h_f * 0.85) as i32;
    let sub_y1 = (h_f * 0.99) as i32;
    draw_rect(
        &mut out,
        0,
        sub_y0,
        w as i32 - 1,
        sub_y1,
        Rgba([0, 150, 255, 255]),
    );

    // ---- 红框：OCR 识别到的文字框 ----
    let opts = OcrOptions {
        bottom_only: true,
        subtitle_only,
        use_nms: true,
        text_confidence_threshold: 0.5,
        use_warp_crop: false,
        ..Default::default()
    };
    let mut ocr = SubtitleOcr::from_profile(
        rapidocr_ort::ModelProfile::V4,
        &model_dir,
        opts,
    )
    .expect("加载引擎失败");

    let rgb = rapidocr_ort::load_image(Path::new(in_path)).expect("读图失败");
    let boxes = ocr.ocr_image(&rgb).expect("OCR 失败");

    println!(
        "subtitle_only={}  H={}  ROI上界={}  subtitle区间=[{},{}]",
        subtitle_only, h, roi_y, sub_y0, sub_y1
    );
    for b in &boxes {
        let x0 = b.x_range[0] as i32;
        let y0 = b.y_range[0] as i32;
        let x1 = b.x_range[1] as i32;
        let y1 = b.y_range[1] as i32;
        draw_rect(&mut out, x0, y0, x1, y1, Rgba([255, 0, 0, 255]));
        println!(
            "  框 text={:?} conf={:.3} y=[{},{}] center_y={:.0} ratio={:.3}",
            b.text,
            b.text_confidence,
            y0,
            y1,
            b.center[1],
            b.center[1] / h_f
        );
    }
    if boxes.is_empty() {
        println!("  (无文字框)");
    }

    out.save(Path::new(out_path)).expect("保存失败");
    println!("已写出: {}", out_path);
}

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .parent()
        .expect("parent")
        .to_path_buf()
}
