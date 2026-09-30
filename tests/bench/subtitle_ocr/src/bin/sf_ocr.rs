//! 跨包流程例子：subtitle_finder 提关键帧 → subtitle_ocr 识别 → 带时间轴字幕。
//!
//! 演示 `subtitle_finder`（吃视频出关键帧）与 `subtitle_ocr`（吃帧出字幕文本）
//! 两个独立包的协作，输出每段字幕的起止时间 + OCR 文本。
//!
//! 用法（仓库根）：
//!   cargo run -p bench_subtitle_ocr --bin sf_ocr -- <video.mp4> [--out <dir>]
//!   cargo run -p bench_subtitle_ocr --bin sf_ocr -- --from-dir <kf_dir> [--out <dir>]
//! 例：
//!   cargo run -p bench_subtitle_ocr --bin sf_ocr -- /tmp/clip5s.mp4
//!   cargo run -p bench_subtitle_ocr --bin sf_ocr -- video.mp4 --out /tmp/res
//!   cargo run -p bench_subtitle_ocr --bin sf_ocr -- --from-dir /tmp/kf --out /tmp/res  # 只 OCR，不重新提取
//!
//! 说明：
//! - subtitle_finder 的关键帧 = 每个字幕段的一张代表帧（原始 BGR，含背景）。
//! - 用 subtitle_ocr 的 `SubtitleOcr::ocr_image` 对代表帧识别文本（bottom_only 裁底部）。
//! - 关键帧自带 start_ms/end_ms 时间轴，直接拼成字幕行。
//! - 模型目录默认仓库根 `data/models/rapidocr`。
//! - `--from-dir <kf_dir>`：读已提取的关键帧 PNG（subtitle_finder 落盘格式
//!   `{start}_{end}_{i}.png`），**只跑 OCR，不重新提取**（解耦提取与识别）。
//! - `--out <dir>`：额外写 `ocr.json`（对齐 bench 的 result.segments 结构）到该目录。

use std::path::{Path, PathBuf};

use anyhow::Result;
use rapidocr_ort::{DEFAULT_MODEL_DIR, ModelProfile};
use subtitle_ocr::{OcrOptions, SubtitleOcr};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        anyhow::bail!("用法: sf_ocr <video.mp4> [--out <dir>]  |  sf_ocr --from-dir <kf_dir> [--out <dir>]");
    }
    let mut video_arg = None;
    let mut from_dir: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut label = "sf".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--from-dir" => {
                i += 1;
                from_dir = args.get(i).map(PathBuf::from);
            }
            "--out" => {
                i += 1;
                out_dir = args.get(i).map(PathBuf::from);
            }
            "--label" => {
                i += 1;
                label = args.get(i).cloned().unwrap_or(label);
            }
            _ => video_arg = Some(args[i].clone()),
        }
        i += 1;
    }

    // 2) 初始化 subtitle_ocr（模型目录仓库根 data/models/rapidocr）。
    let model_dir = bench_subtitle_ocr::repo_root().join(DEFAULT_MODEL_DIR);
    if !model_dir.exists() {
        anyhow::bail!("模型目录不存在: {}（用 --model 指定或放 data/models/rapidocr）", model_dir.display());
    }
    let mut ocr = SubtitleOcr::from_profile(ModelProfile::V3, &model_dir, OcrOptions::default())?;

    // 关键帧来源：--from-dir 读已提取的 PNG（只跑 OCR，不重新提取）；
    // 否则从视频提取关键帧（每个关键帧 = 一个字幕段）。
    let kfs: Vec<(u64, u64, ndarray::Array3<u8>)> = if let Some(dir) = from_dir.as_ref() {
        load_keyframes_from_dir(dir)?
    } else {
        let video = Path::new(video_arg.as_deref().ok_or_else(|| anyhow::anyhow!("缺少视频路径，或用 --from-dir"))?);
        if !video.exists() {
            anyhow::bail!("视频不存在: {}", video.display());
        }
        let params = subtitle_finder::params::Params::default();
        let kfs = subtitle_finder::find_keyframes(video, &params)?;
        println!("找到 {} 个关键帧", kfs.len());
        kfs.iter().map(|kf| (kf.start_ms, kf.end_ms, kf.frame.clone())).collect()
    };
    if from_dir.is_some() {
        println!("从目录加载 {} 个关键帧（仅 OCR，不重新提取）", kfs.len());
    }

    // 3) 对每个关键帧的原始帧 OCR，结合时间轴输出字幕，并收集 segments 供写 json。
    let mut segments = Vec::new();
    println!("---- 字幕时间轴 ----");
    for (i, (start_ms, end_ms, frame)) in kfs.iter().enumerate() {
        let lines = ocr.ocr_image(frame)?;
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.text.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let joined = text.join(" ");
        let t = if joined.is_empty() { "(未识别)".to_string() } else { joined.clone() };
        println!("[{}] {}-{}ms: {}", i, start_ms, end_ms, t);
        segments.push(serde_json::json!({
            "text": joined,
            "start": start_ms,
            "end": end_ms,
            "confidence": null,
        }));
    }

    // 4) 写 ocr.json + summary.json 到结果目录。
    //    默认 `tests/bench/subtitle_ocr/results/<label>/`（项目内，用户约定），
    //    也可用 --out <dir> 覆盖。
    let results_dir = match out_dir {
        Some(d) => d,
        None => bench_subtitle_ocr::repo_root()
            .join("tests")
            .join("bench")
            .join("subtitle_ocr")
            .join("results")
            .join(&label),
    };
    std::fs::create_dir_all(&results_dir)?;
    let merged: Vec<String> = segments
        .iter()
        .map(|s| s.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let ocr_output = serde_json::json!({
        "audio_info": { "duration": segments.last().map(|s| s["end"].as_u64().unwrap_or(0)).unwrap_or(0) },
        "result": { "text": merged.join(" "), "segments": segments },
        "_engine": "sf-keyframe",
        "_source": "video_hardsub",
        "_fps": 30,
        "_timingsMs": serde_json::Value::Null,
    });
    let ocr_path = results_dir.join("ocr.json");
    std::fs::write(&ocr_path, serde_json::to_string_pretty(&ocr_output).unwrap())?;
    println!("OCR 结果已写入: {}", ocr_path.display());

    // summary.json：对 GT（ref/ocr_manual.json）算 CER + 时序对齐。
    let gt_path = bench_subtitle_ocr::repo_root()
        .join("tests")
        .join("bench")
        .join("subtitle_ocr")
        .join("ref")
        .join("ocr_manual.json");
    if gt_path.exists() {
        let cer = bench_subtitle_ocr::eval_cer(&ocr_path, &gt_path);
        let align = bench_subtitle_ocr::align_segments(
            &bench_subtitle_ocr::load_timed_segments(&gt_path),
            &bench_subtitle_ocr::load_timed_segments(&ocr_path),
        );
        let summary_json = serde_json::json!({
            "label": label,
            "engine": "sf-keyframe",
            "keyframes": kfs.len(),
            "segments": kfs.len(),
            "cer": cer.norm,
            "cer_raw": cer.raw,
            "hyp_chars": cer.hyp_chars,
            "ref_chars": cer.ref_chars,
            "ocr_inference_s": 0.0,
            "ocr_frames": kfs.len(),
            "alignment": {
                "paired": align.pairs.len(),
                "missed": align.missed,
                "spurious": align.spurious,
                "zero_duration": align.zero_duration,
                "split": align.split,
                "merged": align.merged,
                "iou_mean": (align.iou_mean * 10000.0).round() / 10000.0,
                "paired_cer": align.paired_cer,
                "start_delta_ms": {
                    "mean": align.start_delta_mean.round(),
                    "median": align.start_delta_median.round(),
                    "p95_abs": align.start_delta_p95_abs.round(),
                },
                "end_delta_ms": {
                    "mean": align.end_delta_mean.round(),
                    "median": align.end_delta_median.round(),
                    "p95_abs": align.end_delta_p95_abs.round(),
                },
            },
            "ocr_timings_ms": serde_json::Value::Null,
        });
        let summary_path = results_dir.join("summary.json");
        std::fs::write(&summary_path, serde_json::to_string_pretty(&summary_json).unwrap())?;
        println!("summary 已写入: {}", summary_path.display());
        println!(
            "  CER(norm)={:.4} raw={:.4} | 时序: paired={} missed={} spurious={}",
            cer.norm, cer.raw, align.pairs.len(), align.missed, align.spurious
        );
    } else {
        println!("GT 不存在（{}），跳过 summary.json", gt_path.display());
    }

    Ok(())
}

/// 从关键帧 PNG 目录读取（subtitle_finder 落盘格式：原始帧在 `frames/` 子目录，
/// 文件名 `{start}_{end}.png`；掩码在 `mask/`，互不干扰）。从文件名解析时间轴，
/// 只做 OCR，不重新提取关键帧。
fn load_keyframes_from_dir(dir: &Path) -> Result<Vec<(u64, u64, ndarray::Array3<u8>)>> {
    // 原始关键帧统一在 frames/ 子目录下（掩码已分离到 mask/，无需后缀过滤）。
    let frames_dir = dir.join("frames");
    let mut files: Vec<_> = std::fs::read_dir(&frames_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "png").unwrap_or(false))
        .collect();
    // 按文件名里的 start_ms 数值排序（`{start}_{end}.png`），保证时间顺序。
    files.sort_by_key(|e| {
        e.file_name()
            .to_string_lossy()
            .split('_')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(u64::MAX)
    });

    let mut kfs = Vec::new();
    for e in files {
        let name = e.file_name().to_string_lossy().to_string();
        // `{start}_{end}.png` → (start, end)。
        let stem = name.trim_end_matches(".png");
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() != 2 {
            anyhow::bail!("无法从文件名解析时间轴（应为 start_end.png）: {}", name);
        }
        let start: u64 = parts[0].parse().map_err(|_| anyhow::anyhow!("start 解析失败: {}", name))?;
        let end: u64 = parts[1].parse().map_err(|_| anyhow::anyhow!("end 解析失败: {}", name))?;
        let img = image::open(e.path())?.to_rgb8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        // RGB → BGR Array3（对齐 subtitle_finder 关键帧的 BGR 格式）。
        let mut arr = ndarray::Array3::<u8>::zeros((h, w, 3));
        for y in 0..h {
            for x in 0..w {
                let p = img.get_pixel(x as u32, y as u32);
                arr[[y, x, 0]] = p[2]; // B
                arr[[y, x, 1]] = p[1]; // G
                arr[[y, x, 2]] = p[0]; // R
            }
        }
        kfs.push((start, end, arr));
    }
    Ok(kfs)
}
