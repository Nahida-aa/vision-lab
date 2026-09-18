//! 命令行：`subtitle-ocr-post --frames <ocr.json> --out <dir> [--video-height PX] [--stop-at STEP] [--threshold T]`
//!
//! 统合字幕后处理的 5 个步骤（原 `subtitle-ocr` 包的 5 个独立 CLI），一条命令在
//! 内存中串起、按需落盘各中间产物：
//!
//!   1. adjust-box   ：框几何惩罚 + 标记离群      → `frames_box_adjust.json`
//!   2. filter-box   ：剔除离群框、重聚合干净帧    → `frames_box_filter.json`
//!   3. merge        ：逐帧 → 时间轴字幕段        → `frames_merged.json`
//!   4. adjust-segment：段置信度调整(Y 偏移/孤立惩罚) → `segment_adjust.json`
//!   5. filter-segment：按阈值丢弃低置信段         → `segment_filter.json`（最终结果）
//!
//! 各中间产物文件名沿用原 justfile 约定，均写到 `--out` 目录下（自动创建）。
//! `--stop-at` 可只跑到某一步（默认 `filter-segment` 全跑完）；`--threshold` 为
//! 最后一步的过滤阈值（默认 0.6，对齐 justfile filter-segment 默认）。
//!
//! 第 4 步需要的画面高度取自 `--frames` 的 `meta.video_height`（识别侧写入的图像
//! 坐标系高度），老 JSON 没有该字段时用 `--video-height <px>` 显式给；本 CLI 不读
//! 视频文件（为拿一个高度链接 ffmpeg 不划算）。

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use std::path::PathBuf;
use subtitle_ocr_post::{
    BoxAdjustedArgs, FrameResult, MergeFramesArgs, OcrBoxResult, OcrSegmentAdjustArgs,
    compute_box_x_stats, compute_box_y_stats, merge_frames, ocr_frames_adjust_box,
    ocr_frames_filter_box, ocr_segment_adjust, ocr_segment_filter_with_meta,
};
use tracing::info;

/// 后处理步骤：可 `--stop-at` 截断（对应各中间产物文件名）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Step {
    AdjustBox,
    FilterBox,
    Merge,
    AdjustSegment,
    FilterSegment,
}

/// 输入里单个框（仅反序列化 [`OcrBoxResult`] 实际消费的字段）。
#[derive(Debug, Deserialize)]
struct InputBox {
    text: String,
    #[serde(default)]
    text_confidence: f32,
    #[serde(default)]
    box_confidence: f32,
    /// 四个顶点（顺时针）；字段名为 `bbox`。
    #[serde(default, rename = "bbox")]
    bbox: [[f32; 2]; 4],
    #[serde(default)]
    x_range: [f32; 2],
    #[serde(default)]
    y_range: [f32; 2],
    #[serde(default)]
    center: [f32; 2],
}

impl InputBox {
    fn into_ocr_box_result(self) -> OcrBoxResult {
        OcrBoxResult {
            text: self.text,
            text_confidence: self.text_confidence,
            box_confidence: self.box_confidence,
            bbox: self.bbox,
            x_range: self.x_range,
            y_range: self.y_range,
            center: self.center,
        }
    }
}

/// 输入里单帧。
#[derive(Debug, Deserialize)]
struct InputFrame {
    text: String,
    #[serde(default)]
    text_confidence: f64,
    #[serde(default)]
    boxes: Vec<InputBox>,
    #[serde(default)]
    x_range: [f32; 2],
    #[serde(default)]
    y_range: [f32; 2],
    #[serde(default)]
    timestamp: u64,
}

impl InputFrame {
    fn into_frame_result(self) -> FrameResult {
        FrameResult {
            text: self.text,
            text_confidence: self.text_confidence,
            boxes: self
                .boxes
                .into_iter()
                .map(InputBox::into_ocr_box_result)
                .collect(),
            x_range: self.x_range,
            y_range: self.y_range,
            timestamp: self.timestamp,
        }
    }
}

/// 输入 meta 里本 CLI 消费的字段（其余忽略）。`video_height` 由识别侧写入。
#[derive(Debug, Deserialize)]
struct InputMeta {
    #[serde(default)]
    video_height: Option<u32>,
}

/// 兼容 frames 输入的两种形态：裸 `FrameResult[]`，或 `{ frames, meta }`
///（`subtitle-ocr --out` 输出形状）。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InputFrames {
    Wrapped {
        frames: Vec<InputFrame>,
        #[serde(default)]
        meta: Option<InputMeta>,
    },
    Bare(Vec<InputFrame>),
}

impl InputFrames {
    fn into_frames(self) -> Vec<FrameResult> {
        match self {
            InputFrames::Wrapped { frames, .. } => frames
                .into_iter()
                .map(InputFrame::into_frame_result)
                .collect(),
            InputFrames::Bare(frames) => frames
                .into_iter()
                .map(InputFrame::into_frame_result)
                .collect(),
        }
    }

    /// meta 里带的画面高度（老 JSON / 裸数组形态没有 → None）。
    fn meta_video_height(&self) -> Option<u32> {
        match self {
            InputFrames::Wrapped { meta, .. } => meta.as_ref().and_then(|m| m.video_height),
            InputFrames::Bare(_) => None,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "subtitle-ocr-post",
    about = "字幕后处理管线：adjust-box → filter-box → merge → adjust-segment → filter-segment"
)]
struct Cli {
    /// 逐帧 OCR JSON（含 boxes，`subtitle-ocr --out` 产出）。
    #[arg(long)]
    frames: PathBuf,

    /// 画面像素高度（段 Y 偏移惩罚的归一化分母）。
    ///
    /// 优先用 frames JSON 的 `meta.video_height`（识别侧写好的图像坐标系高度）；
    /// 老 JSON / cpp 产出没有该字段时，用本参数显式指定（如 `ffprobe -v error
    /// -select_streams v:0 -show_entries stream=height -of csv=p=0` 取到的值）。
    #[arg(long)]
    video_height: Option<u32>,

    /// 输出目录：各中间产物 JSON 写到此处（自动创建）。
    #[arg(long)]
    out: PathBuf,

    /// 只跑到某一步（默认跑完整管线到 filter-segment）。
    #[arg(long, value_enum, default_value_t = Step::FilterSegment)]
    stop_at: Step,

    /// filter-segment 的置信度阈值（默认 0.5，对齐 justfile）。
    #[arg(long, default_value_t = 0.5)]
    threshold: f32,

    /// merge：两帧（无采样节奏基线）时允许的最大间距(ms)，超过视为重复出现断段。
    /// 默认 6000（宽松，保留稀疏采样单句）。
    #[arg(long, default_value_t = 6000)]
    max_gap_two_frames: u32,
    /// merge：多帧相对簇断裂系数 K（当前间距 > K×典型间距 即断段）。默认 4.0。
    #[arg(long, default_value_t = 4.0)]
    split_ratio: f64,
    /// merge：多帧绝对硬上限(ms)，间距超过即断段（低于两帧宽松值）。默认 5000。
    #[arg(long, default_value_t = 5000)]
    max_gap_multi_hard: u32,
}

/// 仓库根：二进制在 `target/debug/subtitle-ocr-post`，上溯两级到 workspace 根。
fn current_exe_repo_root() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("获取当前可执行文件路径失败")?;
    let exe_dir = exe.parent().context("可执行文件无父目录")?.to_path_buf();
    let root = exe_dir
        .join("..") // target/debug 或 target/release
        .join("..") // packages/subtitle-ocr-post
        .canonicalize()
        .context("解析仓库根失败（确认从仓库内构建）")?;
    Ok(root)
}

fn resolve_path(repo_root: &std::path::Path, p: &std::path::Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        repo_root.join(p)
    }
}

/// 取画面高度（Y 偏移惩罚归一化分母）：显式参数 > frames meta > 报错。
fn resolve_video_height(
    explicit: Option<u32>,
    parsed: &InputFrames,
) -> Result<f32> {
    if let Some(h) = explicit {
        return Ok(h as f32);
    }
    if let Some(h) = parsed.meta_video_height() {
        return Ok(h as f32);
    }
    anyhow::bail!(
        "无法确定画面高度：frames JSON 的 meta.video_height 缺失，\
         请用 --video-height <px> 指定（ffprobe -v error -select_streams v:0 \
         -show_entries stream=height -of csv=p=0 <video>）"
    )
}

/// 序列化并写 JSON 到 out 目录，打印落盘位置。
fn write_json<T: serde::Serialize>(out_dir: &std::path::Path, name: &str, v: &T) -> Result<()> {
    let path = out_dir.join(name);
    let json = serde_json::to_string_pretty(v).context("序列化失败")?;
    std::fs::write(&path, json).with_context(|| format!("写入失败: {}", path.display()))?;
    info!(path = %path.display(), "已写出");
    Ok(())
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let repo_root = current_exe_repo_root()?;
    let frames_path = resolve_path(&repo_root, &cli.frames);
    let out_dir = resolve_path(&repo_root, &cli.out);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("创建输出目录失败: {}", out_dir.display()))?;

    // ─── 读逐帧 OCR 结果 ───
    let raw = std::fs::read_to_string(&frames_path)
        .with_context(|| format!("读取 frames 文件失败: {}", frames_path.display()))?;
    let parsed: InputFrames = serde_json::from_str(&raw)
        .context("解析 frames JSON 失败（需为 FrameResult[] 或 {frames,meta}）")?;
    let vh = resolve_video_height(cli.video_height, &parsed)?;
    let frames = parsed.into_frames();
    println!("读取 {} 帧", frames.len());

    // ─── 1. adjust-box ───
    let y_stats = compute_box_y_stats(&frames);
    let x_stats = compute_box_x_stats(&frames);
    let adjust = ocr_frames_adjust_box(&frames, &y_stats, &x_stats, &BoxAdjustedArgs::default());
    write_json(&out_dir, "frames_box_adjust.json", &adjust)?;
    println!(
        "[1/5] adjust-box: {} 帧，写出 frames_box_adjust.json",
        adjust.frames.len()
    );
    if cli.stop_at == Step::AdjustBox {
        return Ok(());
    }

    // ─── 2. filter-box ───
    let filtered = ocr_frames_filter_box(&adjust.frames);
    write_json(&out_dir, "frames_box_filter.json", &filtered)?;
    println!(
        "[2/5] filter-box: {} 帧，写出 frames_box_filter.json",
        filtered.frames.len()
    );
    if cli.stop_at == Step::FilterBox {
        return Ok(());
    }

    // ─── 3. merge ───
    let merge_args = MergeFramesArgs {
        max_gap_two_frames: Some(cli.max_gap_two_frames),
        split_ratio: Some(cli.split_ratio),
        max_gap_multi_hard: Some(cli.max_gap_multi_hard),
        ..MergeFramesArgs::default()
    };
    let merged = merge_frames(&filtered.frames, &merge_args);
    write_json(&out_dir, "frames_merged.json", &merged)?;
    println!(
        "[3/5] merge: {} 段，写出 frames_merged.json",
        merged.segments.len()
    );
    if cli.stop_at == Step::Merge {
        return Ok(());
    }

    // ─── 4. adjust-segment（frames 用 filter 后的干净帧，高度取自 frames meta / 参数）───
    let y_stats2 = compute_box_y_stats(&filtered.frames);
    let seg_adjust = ocr_segment_adjust(
        &merged.segments,
        &filtered.frames,
        &y_stats2,
        vh,
        &OcrSegmentAdjustArgs::default(),
    );
    write_json(&out_dir, "segment_adjust.json", &seg_adjust)?;
    println!(
        "[4/5] adjust-segment: {} 段，写出 segment_adjust.json",
        seg_adjust.len()
    );
    if cli.stop_at == Step::AdjustSegment {
        return Ok(());
    }

    // ─── 5. filter-segment ───
    let seg_filter = ocr_segment_filter_with_meta(&seg_adjust, cli.threshold);
    write_json(&out_dir, "segment_filter.json", &seg_filter)?;
    println!(
        "[5/5] filter-segment: 阈值 {:.2}，保留 {} 段，写出 segment_filter.json",
        cli.threshold, seg_filter.meta.segment_count
    );

    println!("输出目录: {}", out_dir.display());
    Ok(())
}

/// 初始化 tracing subscriber：日志打到 stderr，级别由 `RUST_LOG` 控制（默认 `warn`）。
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}
