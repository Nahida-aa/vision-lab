//! Rust 单帧分阶段耗时探针（测量代码，不改被测实现）。
//!
//! `probe.rs` 只报整帧 `ocr_entry` 总耗时，无法回答「Python 的 1.2~1.3× 差距
//! 来自哪一段」。本探针把 Rust 侧拆成与 Python `elapse = [det, cls, rec]`
//! 可比的阶段：
//!
//! - `preprocess_det`：det 输入的 resize（1280×288 → 736×3264）+ 归一化 + CHW。
//!   Python 同名阶段（`DetPreProcess.__call__`，其中 numpy `normalize` 占大头）
//!   在同帧上约 42ms。
//! - `detect_raw_boxes`：det 预处理 + det 推理 + DB 后处理（不含 rec）。
//! - `detect`：完整 det + cls + rec + 聚合（= Python `engine(roi)`）。
//! - `load_image`：JPEG 解码（= Python `cv2.imread`）。
//! - `roi_copy`：底 40% ROI 切片拷贝（Python `img[y:, :]` 是 view，不拷贝）。
//!
//! 各阶段各自独立计量、互不嵌套；`detect - detect_raw_boxes` 即 cls + rec，
//! `detect_raw_boxes - preprocess_det` 即 det 推理 + DB 后处理。
//!
//! 用法（仓库根）：
//!   cargo run --release -p bench_subtitle_ocr --bin phase_probe -- \
//!     --dir <只含 1 张图的目录> --model v3 --reps 20

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use ndarray::s;
use rapidocr_ort::{ModelProfile, OcrEngine, preprocess};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((s.len() - 1) as f64 * p).round() as usize;
    s[idx]
}

fn stats(v: &[f64]) -> serde_json::Value {
    let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
    serde_json::json!({ "avg": mean, "p50": pct(v, 0.5), "p95": pct(v, 0.95) })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(arg(&args, "--dir").context("缺少 --dir")?);
    let reps: usize = arg(&args, "--reps")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let warmup: usize = arg(&args, "--warmup")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let out_path = arg(&args, "--out").map(PathBuf::from);
    let (profile, model_name) = match arg(&args, "--model").as_deref() {
        Some("v3") => (ModelProfile::V3, "v3"),
        _ => (ModelProfile::V4, "v4"),
    };
    let model_dir: PathBuf = arg(&args, "--model-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| bench_subtitle_ocr::repo_root().join("data/models/rapidocr"));

    let frame_path = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("jpg")))
        .with_context(|| format!("目录里没有 jpg: {}", dir.display()))?;

    let t_init = Instant::now();
    let mut engine = OcrEngine::from_profile(profile, &model_dir)?;
    let engine_init_ms = t_init.elapsed().as_secs_f64() * 1000.0;

    let rgb = rapidocr_ort::load_image(&frame_path)?;
    let (h, _, _) = rgb.dim();
    let y_offset = (h as f32 * 0.6) as usize;
    let roi = rgb.slice(s![y_offset.., .., ..]).to_owned();

    // 预热：先跑完整链路，把 det/cls/rec 三个 session 的 kernel 都焐热。
    for _ in 0..warmup {
        let _ = engine.detect(&roi)?;
    }

    // 各阶段**分开循环**测量（不在同一轮里先跑 det_raw 再跑 detect）：否则
    // detect 里的 det 紧跟在 det_raw 的 det 之后，arena/缓存全热，会把
    // 「detect − det_raw」低估成 rec。分开后每轮 det 的前置状态都接近真实流水线。
    let mut t_pre = Vec::with_capacity(reps);
    let mut t_raw = Vec::with_capacity(reps);
    let mut t_full = Vec::with_capacity(reps);
    let mut t_load = Vec::with_capacity(reps);
    let mut t_roi = Vec::with_capacity(reps);

    let mut last_res = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        last_res = engine.detect(&roi)?;
        t_full.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    if last_res.is_empty() {
        anyhow::bail!("detect 未返回任何文本框");
    }

    for _ in 0..reps {
        let t = Instant::now();
        let _boxes = engine.detect_raw_boxes(&roi);
        t_raw.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    for _ in 0..reps {
        let t = Instant::now();
        let _ = preprocess::preprocess_det(&roi);
        t_pre.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    for _ in 0..reps {
        let t = Instant::now();
        let img = rapidocr_ort::load_image(&frame_path)?;
        t_load.push(t.elapsed().as_secs_f64() * 1000.0);
        drop(img);

        let t = Instant::now();
        let r = rgb.slice(s![y_offset.., .., ..]).to_owned();
        t_roi.push(t.elapsed().as_secs_f64() * 1000.0);
        drop(r);
    }

    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let out = serde_json::json!({
        "impl": "rust-phase",
        "model": model_name,
        "frame": frame_path.display().to_string(),
        "roi": [roi.dim().1, roi.dim().0],
        "reps": reps,
        "warmup": warmup,
        "engine_init_ms": engine_init_ms,
        "preprocess_det_ms": stats(&t_pre),
        "detect_raw_boxes_ms": stats(&t_raw),
        "detect_ms": stats(&t_full),
        "load_image_ms": stats(&t_load),
        "roi_copy_ms": stats(&t_roi),
        "derived": {
            "det_infer_post_ms": avg(&t_raw) - avg(&t_pre),
            "cls_plus_rec_ms": avg(&t_full) - avg(&t_raw),
            "whole_frame_ms": avg(&t_full) + avg(&t_load) + avg(&t_roi),
        },
    });
    let s = serde_json::to_string_pretty(&out)?;
    if let Some(p) = &out_path {
        std::fs::write(p, &s)?;
    }
    println!("{s}");
    Ok(())
}
