//! 一次性：全图 OCR 并打印每个框的全部字段（text/tc/bc/bbox/center/x_range/y_range）。
use std::path::Path;

use rapidocr_ort::{ModelProfile, OcrEngine};

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .parent()
        .expect("parent")
        .to_path_buf()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let img_path = &args[1];
    let root = repo_root();
    let model_dir = root.join(rapidocr_ort::DEFAULT_MODEL_DIR);
    let img = rapidocr_ort::load_image(Path::new(img_path)).expect("读图失败");
    let mut engine = OcrEngine::from_profile(ModelProfile::V4, &model_dir)
        .expect("加载引擎失败")
        .with_warp_crop(false);
    let results = engine.detect(&img).expect("OCR 失败");
    if results.is_empty() {
        println!("(无文字)");
        return;
    }
    for r in &results {
        println!(
            "text={:?} tc={:.4} bc={:.4}\n  bbox={:?}\n  x_range={:?} y_range={:?} center={:?}",
            r.text, r.text_confidence, r.box_confidence, r.bbox, r.x_range, r.y_range, r.center
        );
    }
}
