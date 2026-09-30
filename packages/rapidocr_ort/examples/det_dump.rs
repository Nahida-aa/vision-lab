//! 一次性：dump det heatmap（sigmoid 后）在指定 y 带的 prob 分布，排查检测漏检。
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
    let (h, w, _) = img.dim();
    let mut engine = OcrEngine::from_profile(ModelProfile::V4, &model_dir).expect("加载引擎失败");
    engine.dump_det_heatmap(&img);
}
