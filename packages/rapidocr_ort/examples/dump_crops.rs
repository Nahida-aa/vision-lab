//! 一次性：dump 每个 det 框的 crop 为 PNG（供对比 Python rec），排查 rec 识别问题。
use std::path::Path;

use rapidocr_ort::{ModelProfile, OcrEngine};
use rapidocr_ort::pipeline::crop_for_rec;

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let img_path = &args[1];
    let out_dir = &args[2];
    let root = repo_root();
    let model_dir = root.join(rapidocr_ort::DEFAULT_MODEL_DIR);
    let img = rapidocr_ort::load_image(Path::new(img_path)).unwrap();
    let mut engine = OcrEngine::from_profile(ModelProfile::V4, &model_dir).unwrap();
    let boxes = engine.detect_raw_boxes(&img);
    println!("detected {} boxes", boxes.len());
    for (i, b) in boxes.iter().enumerate() {
        let poly = b.polygon;
        let crop = crop_for_rec(&img, &poly);
        let (ch, cw, _cc) = crop.dim();
        let p = Path::new(out_dir).join(format!("crop_{}.png", i));
        let mut buf = vec![0u8; cw * ch * 4];
        for y in 0..ch {
            for x in 0..cw {
                let o = (y * cw + x) * 4;
                buf[o] = crop[[y, x, 0]];
                buf[o + 1] = crop[[y, x, 1]];
                buf[o + 2] = crop[[y, x, 2]];
                buf[o + 3] = 255;
            }
        }
        let im = image::RgbaImage::from_raw(cw as u32, ch as u32, buf).unwrap();
        im.save(&p).unwrap();
        eprintln!(
            "[crop] #{} score={:.4} crop={}x{} poly={:?} saved={}",
            i,
            b.score,
            cw,
            ch,
            poly.map(|v| [v.x, v.y]),
            p.display()
        );
    }
}
