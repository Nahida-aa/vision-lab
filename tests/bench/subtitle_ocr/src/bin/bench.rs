//! subtitle_ocr 性能基准（准确性 + 速度）。
//!
//! 移植自 LocalDub packages/benchmark/ocr/compute/benchmark-ocr-video.ts，
//! 用于精确复刻 `--engine cpp --only ocr-cpp-fps2-so-ts0.45` 路径以对比结果。
//!
//! 用法：
//!   cargo run -p bench_subtitle_ocr --bin bench -- --impl cpp [--fps 2] [--text-score 0.45] [--subtitle-only]
//!   cargo run -p bench_subtitle_ocr --bin bench -- --impl cpp --only ocr-cpp-fps2-so-ts0.45
//!
//! 当前精确复刻 cpp 单帧逐帧调用路径；py/rust 未实现时跳过。

use std::path::{Path, PathBuf};
use std::process::Command;

use bench_subtitle_ocr::{
    align_segments, extract_frames, list_frame_files, merge_frames, normalize_for_cer, compute_cer,
    repo_root, AlignReport, FrameResult, TimedText,
};

fn cpp_bin() -> PathBuf {
    repo_root()
        .join("packages")
        .join("subtitle_ocr_cpp")
        .join("build")
        .join("subtitle_ocr_ort_cpp")
}

fn models_dir() -> PathBuf {
    repo_root().join("data/models").join("rapidocr")
}

fn video_path() -> PathBuf {
    repo_root()
        .join("tests")
        .join("bench")
        .join("subtitle_ocr")
        .join("ref")
        .join("video_source.mp4")
}

fn gt_path() -> PathBuf {
    repo_root()
        .join("tests")
        .join("bench")
        .join("subtitle_ocr")
        .join("ref")
        .join("ocr_manual.json")
}

fn tmp_dir() -> PathBuf {
    repo_root().join("packages").join("tmp").join("ocr-bench")
}

/// 单帧模式调用 cpp 二进制，读帧级 `text` / 各框 `text_confidence` 均值。
///
/// cpp 与 rust 的输出形状现已一致（都是顶层 `text` + `boxes[]`，框内字段
/// `text_confidence` / `box_confidence` / `box`），故两侧解析口径统一。
///
/// 只保留 `total_ms`——推理耗时由 benchmark 自己在调用前后 `Instant::now()`
/// 测量（不依赖二进制把计时塞进 JSON；计时是旁路观测数据）。
#[derive(Default)]
struct CppFrame {
    text: String,
    confidence: f64,
    total_ms: f64,
}

/// 从一个帧对象（cpp / rust 同形状）取帧级文本与置信度。
///
/// `text` 直接读顶层；`confidence` 取 `boxes[]` 内各框 `text_confidence` 的均值
/// （cpp 顶层不输出帧级 confidence，rust 的顶层 confidence 也正是同一口径的均值，
/// 这样两侧数值可比）。无框时置信度为 0。
fn frame_text_conf(data: &serde_json::Value) -> (String, f64) {
    let text = data
        .get("text")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let confs: Vec<f64> = data
        .get("boxes")
        .and_then(|b| b.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("text_confidence").and_then(|x| x.as_f64()))
                .collect()
        })
        .unwrap_or_default();
    let confidence = if confs.is_empty() {
        0.0
    } else {
        confs.iter().sum::<f64>() / confs.len() as f64
    };
    (text, confidence)
}

fn ocr_frame_cpp(
    frame: &Path,
    text_score: Option<f64>,
    subtitle_only: bool,
    threads: Option<usize>,
) -> CppFrame {
    let bin = cpp_bin();
    let md = models_dir();
    let mut args: Vec<String> = vec![frame.to_str().unwrap().to_string()];
    if let Some(ts) = text_score {
        args.push(ts.to_string());
    }
    if subtitle_only {
        args.push("--subtitle-only".to_string());
    }
    if let Some(n) = threads {
        args.push("--threads".to_string());
        args.push(n.to_string());
    }
    let t0 = std::time::Instant::now();
    let out = Command::new(&bin)
        .args(&args)
        .env("OCR_MODELS_DIR", &md)
        .env("OCR_KEYS_PATH", md.join("ppocr_keys.json"))
        .output();
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    match out {
        Ok(o) if o.status.success() => {
            let parsed: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null);
            let data = if parsed.is_array() {
                parsed.get(0).cloned().unwrap_or(serde_json::Value::Null)
            } else {
                parsed
            };
            let (text, confidence) = frame_text_conf(&data);
            // 耗时由 benchmark 自身测量（子进程 wall time），不读 JSON 计时字段。
            CppFrame {
                text,
                confidence,
                total_ms: wall_ms,
            }
        }
        _ => {
            eprintln!("  cpp OCR 调用失败: {}", frame.display());
            CppFrame::default()
        }
    }
}

/// --dir 批量模式：一次性喂整个帧目录，返回每帧的 best segment（与单帧模式对称）。
/// 对齐原版 runOCRBenchmarkCppOpencv 的调用方式。
fn ocr_dir_cpp(
    frame_dir: &Path,
    text_score: Option<f64>,
    subtitle_only: bool,
    threads: Option<usize>,
) -> Vec<CppFrame> {
    let bin = cpp_bin();
    let md = models_dir();
    let mut args: Vec<String> = vec!["--dir".to_string(), frame_dir.to_str().unwrap().to_string()];
    if let Some(ts) = text_score {
        args.push(ts.to_string());
    }
    if subtitle_only {
        args.push("--subtitle-only".to_string());
    }
    if let Some(n) = threads {
        args.push("--threads".to_string());
        args.push(n.to_string());
    }
    let t0 = std::time::Instant::now();
    let out = Command::new(&bin)
        .args(&args)
        .env("OCR_MODELS_DIR", &md)
        .env("OCR_KEYS_PATH", md.join("ppocr_keys.json"))
        .output();
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    match out {
        Ok(o) if o.status.success() => {
            let parsed: serde_json::Value =
                serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null);
            let arr = parsed.as_array().cloned().unwrap_or_default();
            // 批量模式模型只加载一次，wall time 平摊到每帧作为 per-frame total_ms。
            let per = if arr.is_empty() { 0.0 } else { wall_ms / arr.len() as f64 };
            arr.iter()
                .map(|data| {
                    let (text, confidence) = frame_text_conf(data);
                    CppFrame {
                        text,
                        confidence,
                        total_ms: per,
                    }
                })
                .collect()
        }
        _ => {
            eprintln!("  cpp --dir OCR 调用失败: {}", frame_dir.display());
            Vec::new()
        }
    }
}

#[derive(Default)]
struct Summary {
    label: String,
    fps: f64,
    engine: String,
    frames: usize,
    segments: usize,
    audio_duration_s: f64,
    ocr_inference_s: f64,
    ocr_rtf: f64,
    cer_raw: f64,
    cer_norm: f64,
    hyp_chars: usize,
    ref_chars: usize,
    text_score: f64,
    subtitle_only: bool,
    use_dir: bool,
    threads: Option<usize>,
    timings: Option<Timings>,
}

/// OCR 推理耗时汇总（由 benchmark 自身测量，单位 ms）。
struct Timings {
    total: f64,
    avg_per_frame: f64,
}

/// rust 实现二进制路径（与 cpp 对称，在 packages/subtitle_ocr 下构建）。
/// 优先用 release 构建（与 cpp 的 release 二进制公平对比；debug 二进制因未优化
/// opencv/ort 会慢一个数量级，RTF 不可比）。`--rust-bin` 可显式指定（如 CPU/GPU
/// 两个不同 feature 构建的 exe 分别跑基准，避免相互覆盖）。
fn rust_bin(override_bin: Option<&Path>) -> PathBuf {
    if let Some(p) = override_bin {
        return p.to_path_buf();
    }
    let root = repo_root();
    let release = root.join("target").join("release").join("subtitle_ocr");
    if release.exists() {
        release
    } else {
        root.join("target").join("debug").join("subtitle_ocr")
    }
}

/// 对齐 ocrFrameCpp：单帧模式调用 rust 二进制（subtitle_ocr），取 segments 里最高
/// confidence 的一个作为该帧文本（丢弃 box，与 cpp 路径行为一致）。
fn ocr_frame_rust(
    frame: &Path,
    text_score: Option<f64>,
    subtitle_only: bool,
    _threads: Option<usize>,
    ep: &str,
    bin: &Path,
) -> CppFrame {
    let bin = bin.to_path_buf();
    let mut args: Vec<String> = vec![frame.to_str().unwrap().to_string()];
    if let Some(ts) = text_score {
        // rust CLI 的旗标名与 cpp 不同：--text-confidence-threshold（非 --text-score）。
        args.push("--text-confidence-threshold".to_string());
        args.push(ts.to_string());
    }
    if subtitle_only {
        args.push("--subtitle-only".to_string());
    }
    args.push("--ep".to_string());
    args.push(ep.to_string());
    let t0 = std::time::Instant::now();
    let out = Command::new(&bin).args(&args).output();
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    // 单帧模式：rust 仍输出单元素数组，取首个。
    parse_rust_json(out, "single-frame", wall_ms)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// 对齐 ocrDirCpp：批量模式调用 rust 二进制 `--dir`，返回与 cpp 同形状的帧结果。
fn ocr_dir_rust(
    frame_dir: &Path,
    text_score: Option<f64>,
    subtitle_only: bool,
    _threads: Option<usize>,
    warp_crop: bool,
    ep: &str,
    bin: &Path,
) -> Vec<CppFrame> {
    let bin = bin.to_path_buf();
    let mut args: Vec<String> = vec!["--dir".to_string(), frame_dir.to_str().unwrap().to_string()];
    if let Some(ts) = text_score {
        // rust CLI 的旗标名与 cpp 不同：--text-confidence-threshold（非 --text-score）。
        args.push("--text-confidence-threshold".to_string());
        args.push(ts.to_string());
    }
    if subtitle_only {
        args.push("--subtitle-only".to_string());
    }
    if warp_crop {
        args.push("--warp-crop".to_string());
    }
    // 执行后端透传（cpu/cuda/directml）；CPU 二进制也接受 --ep cpu（默认值）。
    args.push("--ep".to_string());
    args.push(ep.to_string());
    let t0 = std::time::Instant::now();
    let out = Command::new(&bin).args(&args).output();
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    // 批量模式模型只加载一次，wall time 平摊到每帧。
    let n = out.as_ref().ok().and_then(|o| {
        serde_json::from_slice::<serde_json::Value>(&o.stdout)
            .ok()
            .and_then(|v| v.as_array().map(|a| a.len()))
    }).unwrap_or(0);
    let per = if n == 0 { 0.0 } else { wall_ms / n as f64 };
    parse_rust_json(out, "dir", per)
}

/// 解析 rust 二进制输出（JSON 数组，每元素是一个 `FrameResult`）。
///
/// 与 cpp 共用 [`frame_text_conf`]：两侧输出形状已统一为顶层 `text` + `boxes[]`
/// （框内 `text_confidence`）。早前此处照搬 cpp 去取 `segments`，而两侧其实都
/// 已改用 `boxes`，导致解析恒为空、CER 拿空文本跟 GT 比。
///
/// `per_ms` 为 benchmark 自身测量的每帧耗时（调用方在 `Command::output()` 前后
/// 计 wall time 后平摊传入），不依赖二进制把计时塞进 JSON。
fn parse_rust_json(
    out: Result<std::process::Output, std::io::Error>,
    mode: &str,
    per_ms: f64,
) -> Vec<CppFrame> {
    match out {
        Ok(o) if o.status.success() => {
            let parsed: serde_json::Value =
                serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null);
            // --dir 模式下为数组；单帧模式也是单元素数组（与 cpp 对齐）。
            let arr = parsed.as_array().cloned().unwrap_or_default();
            arr.iter()
                .map(|data| {
                    let (text, confidence) = frame_text_conf(data);
                    CppFrame {
                        text,
                        confidence,
                        total_ms: per_ms,
                    }
                })
                .collect()
        }
        _ => {
            eprintln!("  rust subtitle_ocr 调用失败 ({} 模式)", mode);
            Vec::new()
        }
    }
}

/// 对齐 runBenchmarkCommon 的 cpp 路径（精确复刻 --only ocr-cpp-fps2-so-ts0.45）。
/// use_dir=true 时用 --dir 批量模式（对齐 runOCRBenchmarkCppOpencv），否则逐帧单帧
/// （对齐 runOCRBenchmarkCpp）。两种模式产出相同的 frame_results。
fn run_benchmark_cpp(
    label: &str,
    fps: f64,
    text_score: f64,
    subtitle_only: bool,
    use_dir: bool,
    threads: Option<usize>,
) {
    let vpath = video_path();
    let out_dir = tmp_dir().join(format!("frames-{}", label));
    std::fs::create_dir_all(&out_dir).unwrap();

    println!("\n=== OCR Benchmark: {} (fps={}, engine=ort-cpp, mode={}, threads={}) ===",
        label, fps, if use_dir { "--dir" } else { "single-frame" },
        threads.map(|n| n.to_string()).unwrap_or_else(|| "default".into()));
    println!("  extracting frames...");
    let (duration_s, step, src_fps) = extract_frames(&vpath, &out_dir, fps);
    let frame_files = list_frame_files(&out_dir);

    // 调用 cpp：--dir 批量 or 逐帧单帧（由 use_dir 控制，而非拆成两个 runner）
    let mut frame_results: Vec<FrameResult> = Vec::new();
    let mut total_ms = 0.0f64;

    if use_dir {
        // 批量模式：一次喂整个目录，按其返回顺序对应帧文件
        let results = ocr_dir_cpp(&out_dir, Some(text_score), subtitle_only, threads);
        for (i, r) in results.into_iter().enumerate() {
            let timestamp = (((i as f64) * (step as f64)) / src_fps * 1000.0).round() as u64;
            frame_results.push(FrameResult {
                text: r.text.clone(),
                timestamp,
                confidence: r.confidence,
                bbox: None,
            });
            total_ms += r.total_ms;
        }
    } else {
        // 逐帧单帧模式（对齐 ocrFrameCpp）
        for (i, f) in frame_files.iter().enumerate() {
            let r = ocr_frame_cpp(f, Some(text_score), subtitle_only, threads);
            let timestamp = (((i as f64) * (step as f64)) / src_fps * 1000.0).round() as u64;
            frame_results.push(FrameResult {
                text: r.text.clone(),
                timestamp,
                confidence: r.confidence,
                bbox: None, // 对齐 ocrFrameCpp 丢弃 box
            });
            total_ms += r.total_ms;
        }
    }

    // 合并帧（对齐 mergeFrames(frameResults, {})）
    let (merged_text, segments) = merge_frames(&frame_results);
    let inference_s = total_ms / 1000.0;
    let rtf = if duration_s > 0.0 {
        inference_s / duration_s
    } else {
        0.0
    };
    let has_timings = total_ms > 0.0;

    // 写 ocr.json（对齐 runBenchmarkCommon 的 ocrOutput 结构）
    let ocr_output = serde_json::json!({
        "audio_info": {
            "duration": if segments.is_empty() { (duration_s * 1000.0).round() as u64 }
                        else { segments.last().unwrap().end }
        },
        "result": {
            "text": merged_text,
            "segments": segments.iter().map(|s| serde_json::json!({
                "text": s.text,
                "start": s.start,
                "end": s.end,
                "confidence": s.confidence,
            })).collect::<Vec<_>>(),
        },
        "_engine": "ort-cpp",
        "_source": "video_hardsub",
        "_fps": fps,
        "_textScore": text_score,
        "_subtitleOnly": subtitle_only,
        "_timingsMs": if has_timings {
            serde_json::json!({
                "total": total_ms.round() as u64,
                "averagePerFrame": (total_ms / frame_results.len().max(1) as f64).round() as u64,
            })
        } else {
            serde_json::Value::Null
        },
    });

    let metadata_dir = tmp_dir().join(label).join("metadata");
    std::fs::create_dir_all(&metadata_dir).unwrap();
    let ocr_path = metadata_dir.join("ocr.json");
    std::fs::write(&ocr_path, serde_json::to_string_pretty(&ocr_output).unwrap()).unwrap();

    // 算 CER（对齐 runEvalOCR：读 ocr.json 的 result.segments，join 文本，比 GT）
    let cer = eval_cer(&ocr_path, &gt_path());
    // 时序对齐质量：拼串 CER 丢掉了时间戳，这里按时间重叠配对后另算一组指标
    let align = align_segments(
        &load_timed_segments(&gt_path()),
        &load_timed_segments(&ocr_path),
    );

    let summary = Summary {
        label: label.to_string(),
        fps,
        engine: "ort-cpp".to_string(),
        frames: frame_results.len(),
        segments: segments.len(),
        audio_duration_s: (duration_s * 10.0).round() / 10.0,
        ocr_inference_s: (inference_s * 1000.0).round() / 1000.0,
        ocr_rtf: (rtf * 10000.0).round() / 10000.0,
        cer_raw: cer.raw,
        cer_norm: cer.norm,
        hyp_chars: cer.hyp_chars,
        ref_chars: cer.ref_chars,
        text_score,
        subtitle_only,
        use_dir,
        threads,
        timings: if has_timings {
            Some(Timings {
                total: total_ms,
                avg_per_frame: total_ms / frame_results.len().max(1) as f64,
            })
        } else {
            None
        },
    };

    // 写 summary.json
    let summary_json = serde_json::json!({
        "label": summary.label,
        "fps": summary.fps,
        "engine": summary.engine,
        "frames": summary.frames,
        "segments": summary.segments,
        "audio_duration_s": summary.audio_duration_s,
        "ocr_inference_s": summary.ocr_inference_s,
        "ocr_rtf": summary.ocr_rtf,
        "wer": summary.cer_norm,
        "cer": summary.cer_norm,
        "hyp_chars": summary.hyp_chars,
        "ref_chars": summary.ref_chars,
        "textScore": summary.text_score,
        "subtitleOnly": summary.subtitle_only,
        // 复现所需的运行配置：批量/单帧模式与 ORT 线程数（null = cpp 侧默认）
        "mode": if summary.use_dir { "dir" } else { "single-frame" },
        "intraOpThreads": summary.threads,
        // 时序对齐质量（按时间重叠配对；delta 正值 = hyp 偏晚，单位 ms）
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
        "ocr_timings_ms": summary.timings.as_ref().map(|t| serde_json::json!({
            "total": t.total.round() as u64,
            "avgPerFrame": t.avg_per_frame.round() as u64,
        })),
    });
    std::fs::write(
        metadata_dir.join("summary.json"),
        serde_json::to_string_pretty(&summary_json).unwrap(),
    )
    .unwrap();

    // 控制台报告（对齐 benchmark-ocr-video.ts 的 log）
    println!(
        "  merged {} frames → {} segments",
        frame_results.len(),
        segments.len()
    );
    println!("  segs={} dur={:.1}s", segments.len(), duration_s);
    if has_timings {
        println!(
            "  total={}ms  avg/frame={}ms  inference={:.3}s RTF={:.4}",
            total_ms.round() as u64,
            (total_ms / frame_results.len().max(1) as f64).round() as u64,
            inference_s,
            rtf
        );
    }
    println!(
        "  CER(raw)={:.2}% CER(norm)={:.2}%",
        summary.cer_raw * 100.0,
        summary.cer_norm * 100.0
    );
    println!(
        "  hyp_chars={} ref_chars={}",
        summary.hyp_chars, summary.ref_chars
    );
    print_alignment(&align);

    // 清理抽帧目录（对齐 runBenchmarkCommon 的 rm -rf frameDir）
    let _ = std::fs::remove_dir_all(&out_dir);
}

/// 把抽帧目录的 `frame_%05d.jpg` 重命名成 rust `--dir` 的 `ms` 命名约定
/// （时间 = 序号 × 步长 / 源帧率，毫秒，零填充定宽保持排序稳定）。
///
/// rust CLI 的 `--dir` 按文件名解析时间戳，`frame_00001.jpg` 不符合 `ms`/`ms_ms`
/// 格式会直接报错；cpp/py 驱动只按文件名排序枚举，不受重命名影响。
fn rename_frames_to_ms(dir: &Path, step: u64, src_fps: f64) -> std::io::Result<()> {
    let files = list_frame_files(dir);
    for (i, f) in files.iter().enumerate() {
        let ms = (((i as f64) * (step as f64)) / src_fps * 1000.0).round() as u64;
        let new = dir.join(format!("{:07}.jpg", ms));
        std::fs::rename(f, &new)?;
    }
    Ok(())
}

/// 与 `run_benchmark_cpp` 完全对称，仅把 OCR 调用换成 rust 实现
/// （`ocr_dir_rust` / `ocr_frame_rust`），引擎标签改为 `ort-rust[-<ep>]`。
/// 帧抽取、合并、CER、时序对齐、summary 复用同一套代码，保证两实现可横比。
fn run_benchmark_rust(
    label: &str,
    fps: f64,
    text_score: f64,
    subtitle_only: bool,
    use_dir: bool,
    threads: Option<usize>,
    warp_crop: bool,
    ep: &str,
    bin: &Path,
) {
    let vpath = video_path();
    let out_dir = tmp_dir().join(format!("frames-{}", label));
    // 先清残留再抽帧：上次异常中断（如 Ctrl-C / kill）会留下旧帧，与本次
    // 重命名后的 ms 文件名混在一起，导致 --dir 识别到双份帧、CER 被污染。
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();

    let engine_tag = if ep == "cpu" {
        "ort-rust".to_string()
    } else {
        format!("ort-rust-{}", ep)
    };
    println!("\n=== OCR Benchmark: {} (fps={}, engine={}, mode={}, threads={}) ===",
        label, fps, engine_tag, if use_dir { "--dir" } else { "single-frame" },
        threads.map(|n| n.to_string()).unwrap_or_else(|| "default".into()));
    println!("  extracting frames...");
    let (duration_s, step, src_fps) = extract_frames(&vpath, &out_dir, fps);
    rename_frames_to_ms(&out_dir, step, src_fps).expect("帧重命名为 ms 失败");
    let frame_files = list_frame_files(&out_dir);

    let mut frame_results: Vec<FrameResult> = Vec::new();
    let mut total_ms = 0.0f64;

    if use_dir {
        let results = ocr_dir_rust(&out_dir, Some(text_score), subtitle_only, threads, warp_crop, ep, bin);
        for (i, r) in results.into_iter().enumerate() {
            let timestamp = (((i as f64) * (step as f64)) / src_fps * 1000.0).round() as u64;
            frame_results.push(FrameResult {
                text: r.text.clone(),
                timestamp,
                confidence: r.confidence,
                bbox: None,
            });
            total_ms += r.total_ms;
        }
    } else {
        for (i, f) in frame_files.iter().enumerate() {
            let r = ocr_frame_rust(f, Some(text_score), subtitle_only, threads, ep, bin);
            let timestamp = (((i as f64) * (step as f64)) / src_fps * 1000.0).round() as u64;
            frame_results.push(FrameResult {
                text: r.text.clone(),
                timestamp,
                confidence: r.confidence,
                bbox: None,
            });
            total_ms += r.total_ms;
        }
    }

    let (merged_text, segments) = merge_frames(&frame_results);
    let inference_s = total_ms / 1000.0;
    let rtf = if duration_s > 0.0 {
        inference_s / duration_s
    } else {
        0.0
    };
    let has_timings = total_ms > 0.0;

    let ocr_output = serde_json::json!({
        "audio_info": {
            "duration": if segments.is_empty() { (duration_s * 1000.0).round() as u64 }
                        else { segments.last().unwrap().end }
        },
        "result": {
            "text": merged_text,
            "segments": segments.iter().map(|s| serde_json::json!({
                "text": s.text,
                "start": s.start,
                "end": s.end,
                "confidence": s.confidence,
            })).collect::<Vec<_>>(),
        },
        "_engine": engine_tag,
        "_source": "video_hardsub",
        "_fps": fps,
        "_textScore": text_score,
        "_subtitleOnly": subtitle_only,
        "_ep": ep,
        "_timingsMs": if has_timings {
            serde_json::json!({
                "total": total_ms.round() as u64,
                "averagePerFrame": (total_ms / frame_results.len().max(1) as f64).round() as u64,
            })
        } else {
            serde_json::Value::Null
        },
    });

    let metadata_dir = tmp_dir().join(label).join("metadata");
    std::fs::create_dir_all(&metadata_dir).unwrap();
    let ocr_path = metadata_dir.join("ocr.json");
    std::fs::write(&ocr_path, serde_json::to_string_pretty(&ocr_output).unwrap()).unwrap();

    let cer = eval_cer(&ocr_path, &gt_path());
    let align = align_segments(
        &load_timed_segments(&gt_path()),
        &load_timed_segments(&ocr_path),
    );

    let summary = Summary {
        label: label.to_string(),
        fps,
        engine: engine_tag.clone(),
        frames: frame_results.len(),
        segments: segments.len(),
        audio_duration_s: (duration_s * 10.0).round() / 10.0,
        ocr_inference_s: (inference_s * 1000.0).round() / 1000.0,
        ocr_rtf: (rtf * 10000.0).round() / 10000.0,
        cer_raw: cer.raw,
        cer_norm: cer.norm,
        hyp_chars: cer.hyp_chars,
        ref_chars: cer.ref_chars,
        text_score,
        subtitle_only,
        use_dir,
        threads,
        timings: if has_timings {
            Some(Timings {
                total: total_ms,
                avg_per_frame: total_ms / frame_results.len().max(1) as f64,
            })
        } else {
            None
        },
    };

    let summary_json = serde_json::json!({
        "label": summary.label,
        "fps": summary.fps,
        "engine": summary.engine,
        "ep": ep,
        "frames": summary.frames,
        "segments": summary.segments,
        "audio_duration_s": summary.audio_duration_s,
        "ocr_inference_s": summary.ocr_inference_s,
        "ocr_rtf": summary.ocr_rtf,
        "wer": summary.cer_norm,
        "cer": summary.cer_norm,
        "hyp_chars": summary.hyp_chars,
        "ref_chars": summary.ref_chars,
        "textScore": summary.text_score,
        "subtitleOnly": summary.subtitle_only,
        "mode": if summary.use_dir { "dir" } else { "single-frame" },
        "intraOpThreads": summary.threads,
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
        "ocr_timings_ms": summary.timings.as_ref().map(|t| serde_json::json!({
            "total": t.total.round() as u64,
            "avgPerFrame": t.avg_per_frame.round() as u64,
        })),
    });
    std::fs::write(
        metadata_dir.join("summary.json"),
        serde_json::to_string_pretty(&summary_json).unwrap(),
    )
    .unwrap();

    println!(
        "  merged {} frames → {} segments",
        frame_results.len(),
        segments.len()
    );
    println!("  segs={} dur={:.1}s", segments.len(), duration_s);
    if has_timings {
        println!(
            "  total={}ms  avg/frame={}ms  inference={:.3}s RTF={:.4}",
            total_ms.round() as u64,
            (total_ms / frame_results.len().max(1) as f64).round() as u64,
            inference_s,
            rtf
        );
    }
    println!(
        "  CER(raw)={:.2}% CER(norm)={:.2}%",
        summary.cer_raw * 100.0,
        summary.cer_norm * 100.0
    );
    println!(
        "  hyp_chars={} ref_chars={}",
        summary.hyp_chars, summary.ref_chars
    );
    print_alignment(&align);

    let _ = std::fs::remove_dir_all(&out_dir);
}

/// 用 subtitle_finder 关键帧路径做基准（对比传统抽帧）。
///
/// 流程：subtitle_finder::find_keyframes 提关键帧（每个关键帧 = 一个字幕段，
/// 自带 start_ms/end_ms）→ 保存原始帧 → ocr_dir_rust 批量 OCR → 每个关键帧
/// 直接成一个段（无需 merge_frames）→ 对 GT 算 CER / 时序对齐。
///
/// 结果（ocr.json + summary.json）写到 `results/<label>/`（用户约定）。
fn run_benchmark_sf(
    label: &str,
    text_score: f64,
    subtitle_only: bool,
    threads: Option<usize>,
    warp_crop: bool,
) {
    let vpath = video_path();
    let out_dir = tmp_dir().join(format!("sf-frames-{}", label));
    std::fs::create_dir_all(&out_dir).unwrap();

    println!("\n=== SF Benchmark: {} (subtitle_finder 关键帧) ===", label);
    println!("  extracting keyframes (subtitle_finder)...");
    let params = subtitle_finder::params::Params::default();
    let kfs = subtitle_finder::find_keyframes(&vpath, &params).expect("subtitle_finder 提关键帧失败");
    println!("  {} 个关键帧（vs 抽帧传统 ~{} 帧）", kfs.len(), (kfs.len() * 80).max(10));

    // 保存关键帧原始帧（BGR Array3 → PNG）。
    for (i, kf) in kfs.iter().enumerate() {
        let path = out_dir.join(format!("kf_{:05}.png", i));
        save_keyframe_png(&path, &kf.frame);
    }
    let frame_files = list_frame_files(&out_dir);
    println!("  saved {} 关键帧到 {}", frame_files.len(), out_dir.display());

    // 批量 OCR（sf 路径固定用默认 CPU 二进制）。
    let results = ocr_dir_rust(
        &out_dir,
        Some(text_score),
        subtitle_only,
        threads,
        warp_crop,
        "cpu",
        &rust_bin(None),
    );
    let mut total_ms = 0.0f64;
    for r in &results {
        total_ms += r.total_ms;
    }

    // 每个关键帧 = 一个字幕段（用关键帧自带时间轴）。
    let segments: Vec<serde_json::Value> = kfs
        .iter()
        .enumerate()
        .map(|(i, kf)| {
            serde_json::json!({
                "text": results.get(i).map(|r| r.text.clone()).unwrap_or_default(),
                "start": kf.start_ms,
                "end": kf.end_ms,
                "confidence": results.get(i).map(|r| r.confidence).unwrap_or(0.0),
            })
        })
        .collect();
    let merged_text = segments
        .iter()
        .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    let ocr_output = serde_json::json!({
        "result": {
            "text": merged_text,
            "segments": segments,
        },
        "_engine": "subtitle_finder+ort-rust",
        "_source": "video_hardsub",
        "_keyframe_based": true,
        "_textScore": text_score,
        "_subtitleOnly": subtitle_only,
    });

    // 写结果到 results/<label>/。
    let results_dir = repo_root()
        .join("tests")
        .join("bench")
        .join("subtitle_ocr")
        .join("results")
        .join(label);
    std::fs::create_dir_all(&results_dir).unwrap();
    let ocr_path = results_dir.join("ocr.json");
    std::fs::write(&ocr_path, serde_json::to_string_pretty(&ocr_output).unwrap()).unwrap();

    let cer = eval_cer(&ocr_path, &gt_path());
    let align = align_segments(&load_timed_segments(&gt_path()), &load_timed_segments(&ocr_path));
    let inference_s = total_ms / 1000.0;
    let has_timings = total_ms > 0.0;

    let summary_json = serde_json::json!({
        "label": label,
        "engine": "subtitle_finder+ort-rust",
        "keyframes": kfs.len(),
        "segments": kfs.len(),
        "cer": cer.norm,
        "cer_raw": cer.raw,
        "hyp_chars": cer.hyp_chars,
        "ref_chars": cer.ref_chars,
        "ocr_inference_s": (inference_s * 1000.0).round() / 1000.0,
        "ocr_frames": results.len(),
        "textScore": text_score,
        "subtitleOnly": subtitle_only,
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
        "ocr_timings_ms": if has_timings {
            serde_json::json!({
                "total": total_ms.round() as u64,
                "avgPerFrame": (total_ms / results.len().max(1) as f64).round() as u64,
            })
        } else {
            serde_json::Value::Null
        },
    });
    std::fs::write(
        results_dir.join("summary.json"),
        serde_json::to_string_pretty(&summary_json).unwrap(),
    )
    .unwrap();

    println!("  关键帧 {} 个 → {} 段", kfs.len(), kfs.len());
    println!("  segs={}", kfs.len());
    if has_timings {
        println!("  OCR 推理 {:.1}s, {:.1}ms/帧", inference_s, total_ms / kfs.len().max(1) as f64);
    }
    print_alignment(&align);
    println!("  结果写至 {}", results_dir.display());

    let _ = std::fs::remove_dir_all(&out_dir);
}

/// 把 subtitle_finder 关键帧的 BGR `Array3`（H×W×3）存为 PNG（转 RGB）。
fn save_keyframe_png(path: &std::path::Path, frame: &ndarray::Array3<u8>) {
    let (h, w, _) = frame.dim();
    let mut rgb = Vec::with_capacity(h * w * 3);
    for y in 0..h {
        for x in 0..w {
            rgb.push(frame[[y, x, 2]]); // R
            rgb.push(frame[[y, x, 1]]); // G
            rgb.push(frame[[y, x, 0]]); // B
        }
    }
    let img = image::RgbImage::from_raw(w as u32, h as u32, rgb).expect("构造 RgbImage 失败");
    img.save(path).expect("保存关键帧 PNG 失败");
}

/// 对齐 eval-ocr.ts main()：读 hyp/GT 的 result.segments，join 文本，算 raw + norm CER。
struct CerResult {
    raw: f64,
    norm: f64,
    hyp_chars: usize,
    ref_chars: usize,
}

fn load_segments_text(path: &Path) -> Vec<String> {
    let content = std::fs::read_to_string(path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    v.get("result")
        .and_then(|r| r.get("segments"))
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
                .map(|t| t.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// 打印时序对齐质量。delta 正值 = hyp 偏晚，负值 = 偏早。
fn print_alignment(a: &AlignReport) {
    println!(
        "  aligned: paired={} missed={} spurious={} split={} merged={} zero-dur={}",
        a.pairs.len(),
        a.missed,
        a.spurious,
        a.split,
        a.merged,
        a.zero_duration
    );
    println!(
        "  IoU(mean)={:.4}  CER(paired)={:.2}%",
        a.iou_mean,
        a.paired_cer * 100.0
    );
    println!(
        "  start Δms: mean={:+.0} median={:+.0} p95|Δ|={:.0}",
        a.start_delta_mean, a.start_delta_median, a.start_delta_p95_abs
    );
    println!(
        "  end   Δms: mean={:+.0} median={:+.0} p95|Δ|={:.0}",
        a.end_delta_mean, a.end_delta_median, a.end_delta_p95_abs
    );
}

/// 读带时间戳的 segments，用于时序对齐评估。
fn load_timed_segments(path: &Path) -> Vec<TimedText> {
    let content = std::fs::read_to_string(path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    v.get("result")
        .and_then(|r| r.get("segments"))
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    Some(TimedText {
                        text: s.get("text").and_then(|t| t.as_str())?.trim().to_string(),
                        start: s.get("start").and_then(|t| t.as_u64())?,
                        end: s.get("end").and_then(|t| t.as_u64())?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn eval_cer(hyp_path: &Path, gt_path: &Path) -> CerResult {
    let gt_segs = load_segments_text(gt_path);
    let hyp_segs = load_segments_text(hyp_path);
    let gt_full: String = gt_segs.concat();
    let hyp_full: String = hyp_segs.concat();

    let raw = compute_cer(&strip_ws(&gt_full), &strip_ws(&hyp_full));
    let norm = compute_cer(&normalize_for_cer(&gt_full), &normalize_for_cer(&hyp_full));

    CerResult {
        raw,
        norm,
        hyp_chars: strip_ws(&hyp_full).chars().count(),
        ref_chars: strip_ws(&gt_full).chars().count(),
    }
}

fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut impl_ = "cpp".to_string();
    let mut fps = 2.0f64;
    let mut text_score = 0.45f64;
    let mut subtitle_only = true;
    let mut only: Option<String> = None;
    let mut use_dir = false;
    // None = 不传 --threads，由 cpp 侧默认值决定
    let mut threads: Option<usize> = None;
    // rust 侧用 --warp-crop 对齐 cpp 的透视矫正裁剪（实验）。
    let mut warp_crop = false;
    // rust 侧执行后端：cpu / cuda / directml（透传给 subtitle_ocr --ep）。
    let mut ep = "cpu".to_string();
    // rust 二进制覆盖（CPU/GPU 两个 feature 构建的 exe 分开跑基准时用）。
    let mut rust_bin_override: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--impl" => {
                if let Some(v) = args.get(i + 1) {
                    impl_ = v.clone();
                }
                i += 2;
            }
            "--fps" => {
                if let Some(v) = args.get(i + 1) {
                    fps = v.parse().unwrap_or(2.0);
                }
                i += 2;
            }
            "--text-score" => {
                if let Some(v) = args.get(i + 1) {
                    text_score = v.parse().unwrap_or(0.45);
                }
                i += 2;
            }
            "--subtitle-only" => {
                subtitle_only = true;
                i += 1;
            }
            "--threads" => {
                if let Some(v) = args.get(i + 1) {
                    threads = v.parse().ok().filter(|n| *n > 0);
                }
                i += 2;
            }
            "--dir" => {
                use_dir = true;
                i += 1;
            }
            "--only" => {
                if let Some(v) = args.get(i + 1) {
                    only = Some(v.clone());
                }
                i += 2;
            }
            "--warp-crop" => {
                warp_crop = true;
                i += 1;
            }
            "--ep" => {
                if let Some(v) = args.get(i + 1) {
                    ep = v.clone();
                }
                i += 2;
            }
            "--rust-bin" => {
                if let Some(v) = args.get(i + 1) {
                    rust_bin_override = Some(PathBuf::from(v));
                }
                i += 2;
            }
            _ => i += 1,
        }
    }

    // 对齐 label 生成规则：ocr-${engine}-fps${fps}${so?-so}${-ts${textScore}}
    let engine = match impl_.as_str() {
        "cpp" => "cpp".to_string(),
        "py" => "python".to_string(),
        "rust" if ep == "cpu" => "rust".to_string(),
        // GPU 后端进 label，避免同目录结果互相覆盖：ocr-rust-cuda-fps2-so-ts0.45
        "rust" => format!("rust-{}", ep),
        "sf" => "sf".to_string(),
        other => other.to_string(),
    };
    let ts_label = format!("-ts{}", text_score);
    let base_label = format!(
        "ocr-{}-fps{}{}{}",
        engine,
        fps,
        if subtitle_only { "-so" } else { "" },
        ts_label
    );

    let label = match &only {
        Some(o) => o.clone(),
        None => base_label,
    };

    match impl_.as_str() {
        "cpp" => run_benchmark_cpp(&label, fps, text_score, subtitle_only, use_dir, threads),
        "py" => {
            eprintln!("[py] 基准尚未实现（未装 rapidocr_onnxruntime）");
            std::process::exit(1);
        }
        "rust" => {
            let bin = rust_bin(rust_bin_override.as_deref());
            println!("  rust bin: {}", bin.display());
            run_benchmark_rust(
                &label,
                fps,
                text_score,
                subtitle_only,
                use_dir,
                threads,
                warp_crop,
                &ep,
                &bin,
            )
        }
        // subtitle_finder 关键帧路径（对比传统抽帧）：用独立 label，不用 fps。
        "sf" => {
            let sf_label = format!("sf-{}", text_score);
            run_benchmark_sf(&sf_label, text_score, subtitle_only, threads, warp_crop);
        }
        other => {
            eprintln!("未知实现: {}", other);
            std::process::exit(2);
        }
    }
}
