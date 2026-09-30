//! 命令行：`subtitle_ocr <image>` 或 `subtitle_ocr --dir <dir> ...`
//!
//! 纯感知 OCR 工具，对标 cpp 的 `ocr_pipeline.cpp`：输出 JSON 数组，
//! 每个元素含 `text` / `text_confidence` / `boxes` / `timestamp`。
//!
//! 不含任何耗时字段——推理耗时是旁路观测数据，由调用方自行计时（CLI 在
//! `ocr_image` 调用前后 `Instant::now()` 测量，经 tracing 输出；benchmark 同理）。
//! 不污染 stdout 的 JSON 数组。
//!
//! 本 CLI 只做「逐图/批量 OCR」，不输出时间轴、不做帧合并——带时间戳的字幕段
//! 由知道视频结构的上游（自行补 `start`/`end` 后调用 `merge-frames`）负责。

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;
use std::path::{Path, PathBuf};
use subtitle_ocr::util::{BadNameAction, list_frames};
use subtitle_ocr::{OcrDevice, OcrEntry, OcrFramesMeta, OcrFramesResult, OcrOptions, SubtitleOcr};
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "subtitle_ocr",
    about = "字幕 OCR（基于 rapidocr_ort，PP-OCRv3）"
)]
struct Cli {
    /// 模型套件：v3 / v4 / v6-tiny / v6-medium
    #[arg(long, value_enum, default_value_t = rapidocr_ort::ModelProfile::V4)]
    model: rapidocr_ort::ModelProfile,

    /// 模型目录（默认仓库根 data/models/rapidocr，可经 RAPIDOCR_MODEL_DIR 覆盖）
    #[arg(long, env = "RAPIDOCR_MODEL_DIR", default_value = rapidocr_ort::DEFAULT_MODEL_DIR)]
    model_dir: String,

    /// 输入图片路径（单图模式，不携带时间戳，输出 timestampMs=0；与 --dir 互斥）
    image: Option<String>,

    /// 批量模式：输入图片目录（jpg/jpeg/png/bmp，按文件名排序逐张识别，与 <image> 互斥）。
    ///
    /// 文件名须为 `ms` 或 `ms_ms` 形式，编码该图对应的时刻（毫秒），可前置多余 0：
    /// - `001234.png`        → 单时刻 1234
    /// - `001234_001250.png` → 双时刻 [1234, 1250]（同一张图仅识别一次，产出两个结果）
    /// 不符合格式时按 `--on-bad-name` 处理（`skip` 跳过 / `error` 报错）。
    #[arg(long)]
    dir: Option<String>,

    /// 批量模式（`--dir`）下，文件名不符合 `ms` / `ms_ms` 时间格式时的处理：
    /// `error` 直接报错终止（默认，避免静默丢帧）；`skip` 跳过并警告。
    #[arg(long, value_enum, default_value_t = BadNameAction::Error)]
    on_bad_name: BadNameAction,

    /// 识别置信度下限（对应 cpp 的 text_score / 下游 --text-confidence-threshold，默认 0.5）
    #[arg(long)]
    text_confidence_threshold: Option<f32>,

    /// 仅保留画面底部比例区间的字幕框（cpp --subtitle-only）
    #[arg(long)]
    subtitle_only: bool,

    /// 关闭重叠框 NMS 去重（cpp --no-nms）
    #[arg(long)]
    no_nms: bool,

    /// 关闭 bottom_only：对整帧做 OCR（cpp 默认开启，故本包默认开启，此 flag 取反）
    #[arg(long)]
    full_frame: bool,

    /// 用 cpp 同款的透视矫正裁剪（warpPerspective）替代轴对齐包围盒。
    /// 与 det 几何 minAreaRect 耦合使用（实验对齐 cpp 用）。
    #[arg(long)]
    warp_crop: bool,

    /// 推理线程数（预留；当前 OcrEngine 固定 4，与 cpp 默认一致）
    #[arg(long)]
    threads: Option<usize>,

    /// 输出文件路径（完整文件名，由调用方决定，如 `asr_ocr_frames.json`）：写入
    /// `OcrFramesResult` 结构（各帧结果 + 溯源 meta），便于对接 LocalDub 的
    /// `asr_ocr_frames.json` / `sf_ocr_frames.json` 等。指定后结果仅落盘，不再向
    /// stdout 打印逐帧 JSON 数组（避免与文件重复刷屏）；不指定时仅向 stdout 打印。
    #[arg(long)]
    out: Option<String>,
}

/// 仓库根：二进制在 `target/debug/subtitle_ocr`，上溯两级到 workspace 根。
fn current_exe_repo_root() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("获取当前可执行文件路径失败")?;
    let exe_dir = exe.parent().context("可执行文件无父目录")?.to_path_buf();
    let root = exe_dir
        .join("..") // 去掉 target/debug 或 target/release
        .join("..") // 去掉 packages/subtitle_ocr
        .canonicalize()
        .context("解析仓库根失败（确认从仓库内构建）")?;
    Ok(root)
}

fn resolve_path(repo_root: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    }
}

/// 单帧输出直接复用库里的 [`subtitle_ocr::FrameResult`]（文本 / 聚合置信度 /
/// 各框明细 / 几何值域 / 对应时刻），不再另定义输出结构。
///
/// 不含输入文件名——调用方本就知道自己喂了哪张图，批量模式下时刻信息已体现在
/// `timestamp`；也不含耗时字段——推理耗时是旁路观测数据，由调用方自行计时
/// （在 `ocr_image` 调用前后 `Instant::now()` 测量），不污染 JSON 结构。
fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let repo_root = current_exe_repo_root()?;
    let model_dir = resolve_path(&repo_root, &cli.model_dir);

    let opts = OcrOptions {
        bottom_only: !cli.full_frame,
        subtitle_only: cli.subtitle_only,
        use_nms: !cli.no_nms,
        text_confidence_threshold: cli.text_confidence_threshold.unwrap_or(0.5),
        use_warp_crop: cli.warp_crop,
    };

    let mut ocr = SubtitleOcr::from_profile(cli.model, &model_dir, opts)
        .context("构建字幕 OCR 引擎失败（确认 data/models/rapidocr 权重已就绪）")?;

    // 构建待识别条目：--dir 一张图可对应 1~2 个时刻（ms_ms 双时刻），
    // 单图 <image> 无时间。
    let entries: Vec<OcrEntry> = if let Some(dir) = &cli.dir {
        let dir = resolve_path(&repo_root, dir);
        list_frames(&dir, cli.on_bad_name)?
    } else if let Some(img) = &cli.image {
        vec![OcrEntry {
            path: resolve_path(&repo_root, img),
            times: subtitle_ocr::FrameTimes::None,
        }]
    } else {
        anyhow::bail!("必须提供 <image> 或 --dir <dir>");
    };

    // 核心流程：逐 entry 跑 OCR（读图 → 识别 → 聚合 → 按时刻展开），
    // 用进度条反馈进度（UX 层，走 stderr，不污染 tracing 诊断日志）。
    let total = entries.len();
    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta})",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> "),
    );
    let mut frame_outs: Vec<subtitle_ocr::FrameResult> = Vec::with_capacity(total);
    for e in &entries {
        frame_outs.extend(subtitle_ocr::ocr_entry(&mut ocr, e)?);
        pb.inc(1);
    }
    pb.finish();

    // 画面高度：取首张输入图的尺寸（只读文件头，不解码像素）。`y_range` 是图像坐标系，
    // 故这里就是下游 Y 惩罚所需的分母；抽帧为原始尺寸时亦等于视频帧高。
    // 读不到（无图 / 格式异常）时留 None，由下游回退到显式传值。
    let video_height = entries.first().and_then(|e| match image::image_dimensions(&e.path) {
        Ok((_, h)) => Some(h),
        Err(err) => {
            tracing::warn!(path = %e.path.display(), %err, "读取图片尺寸失败，meta.video_height 留空");
            None
        }
    });

    // --out：额外落地 OcrFramesResult（文件名由调用方指定，如 asr_ocr_frames.json）
    if let Some(out) = &cli.out {
        let path = resolve_path(&repo_root, out);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建输出目录失败: {}", parent.display()))?;
            }
        }
        let result = OcrFramesResult {
            frames: frame_outs.clone(),
            meta: OcrFramesMeta {
                // 本包为 rust 实现；设备固定 cpu（cpp 侧才区分 cuda 等）。
                engine: "ort-rust".to_string(),
                device: OcrDevice::Cpu,
                video_height,
            },
        };
        let json = serde_json::to_string_pretty(&result).context("序列化 OcrFramesResult 失败")?;
        std::fs::write(&path, json).with_context(|| format!("写入失败: {}", path.display()))?;
        info!(path = %path.display(), frames = result.frames.len(), "已写出帧");
    }

    // 主输出：与 cpp 同形状的 JSON 数组（逐图/批量，不带时间轴）。
    // 指定了 --out 时结果已落盘，不再向 stdout 重复打印（避免刷屏 + 与文件重复）。
    if cli.out.is_none() {
        let arr: Vec<Value> = frame_outs
            .iter()
            .map(|f| serde_json::to_value(f).unwrap_or(Value::Null))
            .collect();
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
    }

    Ok(())
}

/// 初始化 tracing subscriber：日志打到 stderr，级别由 `RUST_LOG` 环境变量控制
/// （默认 `info`，显示诊断/写出提示；设 `warn` 可仅看警告，`debug` 看更细）。
/// 进度反馈走独立的 indicatif 进度条（见 main），不经由 tracing，避免被日志级别淹没。
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    // 默认 info 级别，但 ort 把 onnxruntime 的 C 日志桥接成 tracing 事件，
    // 噪声很大（每次建 Session 都刷一堆），单独压到 error 关掉。
    // RUST_LOG 设了就用用户的（可整体或单独覆盖 ort::logging）。
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,ort::logging=error"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}
