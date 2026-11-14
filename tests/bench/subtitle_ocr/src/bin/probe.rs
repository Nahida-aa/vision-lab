//! 单帧耗时探针（测量代码，不改被测实现逻辑）。
//!
//! 目的：只对**同一帧**做多轮重复，拿到「纯单帧处理耗时」的 avg/P50/P95，
//! 与 `--dir` 模式（wall ÷ 帧数）严格区分开。
//!
//! 计时范围与 cpp 的 `imageLoadMs + detMs + postMs + recMs` 对齐：
//! `ocr_entry` = 读图(解码) + det 推理 + det 后处理 + cls + rec 推理 + 聚合。
//! 不含：进程启动、ORT session / 模型加载（单独报 `engine_init_ms`）。
//!
//! 用法（在仓库根）：
//!   cargo run -p bench_subtitle_ocr --release --bin probe -- \
//!     --dir <只含 1 张图的目录> --model v4 --reps 20 [--threshold 0.45] [--subtitle-only]

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use rapidocr_ort::ModelProfile;
use subtitle_ocr::util::{list_frames, BadNameAction};
use subtitle_ocr::{ocr_entry, OcrEntry, OcrOptions, SubtitleOcr};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(arg(&args, "--dir").context("缺少 --dir")?);
    let reps: usize = arg(&args, "--reps").and_then(|v| v.parse().ok()).unwrap_or(20);
    let warmup: usize = arg(&args, "--warmup").and_then(|v| v.parse().ok()).unwrap_or(2);
    let threshold: f32 = arg(&args, "--threshold")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.45);
    let subtitle_only = args.iter().any(|a| a == "--subtitle-only");
    let warp_crop = args.iter().any(|a| a == "--warp-crop");
    let out_path = arg(&args, "--out").map(PathBuf::from);
    let (profile, model_name) = match arg(&args, "--model").as_deref() {
        Some("v3") => (ModelProfile::V3, "v3"),
        Some("v6-tiny") => (ModelProfile::V6Tiny, "v6-tiny"),
        Some("v6-medium") => (ModelProfile::V6Medium, "v6-medium"),
        _ => (ModelProfile::V4, "v4"),
    };
    let model_dir: PathBuf = arg(&args, "--model-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| bench_subtitle_ocr::repo_root().join("data/models/rapidocr"));

    let opts = OcrOptions {
        bottom_only: true,
        subtitle_only,
        use_nms: true,
        text_confidence_threshold: threshold,
        use_warp_crop: warp_crop,
        ..Default::default()
    };

    // 引擎初始化（模型 + 3 个 ORT session）单独计时：这是「常驻 session」省下的那部分。
    let t_init = Instant::now();
    let mut ocr = SubtitleOcr::from_profile(profile, &model_dir, opts)?;
    let engine_init_ms = t_init.elapsed().as_secs_f64() * 1000.0;

    let entries: Vec<OcrEntry> = list_frames(&dir, BadNameAction::Error)?;
    anyhow::ensure!(!entries.is_empty(), "目录里没有可用帧: {}", dir.display());

    // 预热：ORT 首次 run 会做内存分配 / kernel 选择，不计入样本。
    for _ in 0..warmup {
        for e in &entries {
            ocr_entry(&mut ocr, e)?;
        }
    }

    let mut samples: Vec<f64> = Vec::with_capacity(reps * entries.len());
    let mut first_texts: Vec<String> = Vec::new();
    let t_all = Instant::now();
    for _ in 0..reps {
        for e in &entries {
            let t = Instant::now();
            let frames = ocr_entry(&mut ocr, e)?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            samples.push(ms);
            if first_texts.is_empty() {
                first_texts = frames
                    .iter()
                    .map(|f| f.text.clone())
                    .filter(|t| !t.is_empty())
                    .collect();
            }
        }
    }
    let wall_ms = t_all.elapsed().as_secs_f64() * 1000.0;

    let out = serde_json::json!({
        "impl": "rust",
        "model": model_name,
        "model_dir": model_dir.display().to_string(),
        "frames": entries.len(),
        "reps": reps,
        "warmup": warmup,
        "engine_init_ms": engine_init_ms,
        "wall_ms": wall_ms,
        "per_frame_ms": samples,
        "texts": first_texts,
    });
    let s = serde_json::to_string_pretty(&out)?;
    if let Some(p) = &out_path {
        std::fs::write(p, &s)?;
    }
    println!("{s}");
    Ok(())
}
