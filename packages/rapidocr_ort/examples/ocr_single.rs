//! 一次性工具：OCR 单张图片，打印每行文字 + 置信度。
//! 用法：cargo run -p rapidocr_ort --example ocr_single -- <image.png>
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: ocr_single <image.png> [--warp-crop]");
        std::process::exit(1);
    }
    let warp = args.iter().any(|a| a == "--warp-crop");
    let img_path = &args[1];
    let root = repo_root();
    let model_dir = root.join(rapidocr_ort::DEFAULT_MODEL_DIR);
    let img = rapidocr_ort::load_image(Path::new(img_path)).expect("读取图片失败");
    let mut engine = rapidocr_ort::OcrEngine::from_profile(
        rapidocr_ort::ModelProfile::V4,
        &model_dir,
    )
    .expect("加载 v4 引擎失败")
    .with_warp_crop(warp);
    let results = engine.detect(&img).expect("OCR 推理失败");
    if results.is_empty() {
        println!("(无文字)");
    } else {
        for r in &results {
            println!("{}  [tc={:.2}]", r.text, r.text_confidence);
        }
    }
}

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .parent()
        .expect("parent")
        .to_path_buf()
}
