//! subtitle_ocr 各 CLI bin（主 bin `subtitle_ocr` / DirectML 专用 bin `directml`）
//! 共用的执行流程。
//!
//! 「共用」的边界：**命令行旗标与二进制形态不共用**——CPU 主 bin 与 DirectML
//! 专用 bin 各自定义 clap Cli、各自分发，GPU 形态不挂在主 bin 的旗标上；仅共用
//! 解析后的执行流程（建引擎 → 建条目 → 逐条 OCR → 落盘/打印），后端差异收敛在
//! [`RunArgs::execution_backend`] / [`RunArgs::engine`] / [`RunArgs::device`]
//! 三个溯源字段上。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::util::{list_frames, BadNameAction};
use crate::{
    ExecutionBackend, ModelProfile, OcrDevice, OcrEntry, OcrFramesMeta, OcrFramesResult,
    OcrOptions, SubtitleOcr,
};

/// 各 bin 解析完 CLI 后交给 [`run`] 的执行参数。
pub struct RunArgs {
    pub model: ModelProfile,
    pub model_dir: String,
    /// 单图模式（与 `dir` 互斥），无时间戳。
    pub image: Option<String>,
    /// 批量模式：图片目录，文件名须为 `ms` / `ms_ms` 时间格式。
    pub dir: Option<String>,
    pub on_bad_name: BadNameAction,
    pub text_confidence_threshold: Option<f32>,
    pub subtitle_only: bool,
    pub no_nms: bool,
    pub full_frame: bool,
    pub warp_crop: bool,
    pub out: Option<String>,
    /// 推理执行后端（主 bin 固定 Cpu，directml bin 固定 DirectML）。
    pub execution_backend: ExecutionBackend,
    /// `OcrFramesMeta.engine` 溯源字段（如 "ort-rust" / "ort-rust-directml"）。
    pub engine: &'static str,
    /// `OcrFramesMeta.device` 溯源字段。
    pub device: OcrDevice,
}

/// 执行主流程：建引擎 → 构建条目 → 逐条 OCR（进度条）→ `--out` 落盘 / stdout 打印。
pub fn run(args: RunArgs) -> Result<()> {
    let repo_root = current_exe_repo_root()?;
    let model_dir = resolve_path(&repo_root, &args.model_dir);

    let opts = OcrOptions {
        bottom_only: !args.full_frame,
        subtitle_only: args.subtitle_only,
        use_nms: !args.no_nms,
        text_confidence_threshold: args.text_confidence_threshold.unwrap_or(0.5),
        use_warp_crop: args.warp_crop,
        execution_backend: args.execution_backend,
    };

    let mut ocr = SubtitleOcr::from_profile(args.model, &model_dir, opts)
        .context("构建字幕 OCR 引擎失败（确认 data/models/rapidocr 权重已就绪）")?;

    // 构建待识别条目：--dir 一张图可对应 1~2 个时刻（ms_ms 双时刻），
    // 单图 <image> 无时间。
    let entries: Vec<OcrEntry> = if let Some(dir) = &args.dir {
        let dir = resolve_path(&repo_root, dir);
        list_frames(&dir, args.on_bad_name)?
    } else if let Some(img) = &args.image {
        vec![OcrEntry {
            path: resolve_path(&repo_root, img),
            times: crate::FrameTimes::None,
        }]
    } else {
        anyhow::bail!("必须提供 <image> 或 --dir <dir>");
    };

    // 核心流程：逐 entry 跑 OCR（读图 → 识别 → 聚合 → 按时刻展开），
    // 用进度条反馈进度（UX 层，走 stderr，不污染 tracing 诊断日志）。
    let total = entries.len();
    let pb = indicatif::ProgressBar::new(total as u64);
    pb.set_style(
        indicatif::ProgressStyle::with_template(
            "[{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta})",
        )
        .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
        .progress_chars("=> "),
    );
    let mut frame_outs: Vec<crate::FrameResult> = Vec::with_capacity(total);
    for e in &entries {
        frame_outs.extend(crate::ocr_entry(&mut ocr, e)?);
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
    if let Some(out) = &args.out {
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
                engine: args.engine.to_string(),
                device: args.device,
                video_height,
            },
        };
        let json = serde_json::to_string_pretty(&result).context("序列化 OcrFramesResult 失败")?;
        std::fs::write(&path, json).with_context(|| format!("写入失败: {}", path.display()))?;
        tracing::info!(path = %path.display(), frames = result.frames.len(), "已写出帧");
    }

    // 主输出：与 cpp 同形状的 JSON 数组（逐图/批量，不带时间轴）。
    // 指定了 --out 时结果已落盘，不再向 stdout 重复打印（避免刷屏 + 与文件重复）。
    if args.out.is_none() {
        let arr: Vec<serde_json::Value> = frame_outs
            .iter()
            .map(|f| serde_json::to_value(f).unwrap_or(serde_json::Value::Null))
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::Value::Array(arr))?);
    }

    Ok(())
}

/// 初始化 tracing subscriber：日志打到 stderr，级别由 `RUST_LOG` 环境变量控制
/// （默认 `info`，显示诊断/写出提示；设 `warn` 可仅看警告，`debug` 看更细）。
/// 进度反馈走独立的 indicatif 进度条（见 [`run`]），不经由 tracing，避免被日志级别淹没。
pub fn init_tracing() {
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

/// 仓库根：二进制在 `<仓库根>/<两级目录>/`（如 `target/debug/subtitle_ocr`），
/// 上溯两级到 workspace 根。
pub fn current_exe_repo_root() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("获取当前可执行文件路径失败")?;
    let exe_dir = exe.parent().context("可执行文件无父目录")?.to_path_buf();
    let root = exe_dir
        .join("..") // 去掉 target/debug 或 target/release
        .join("..") // 去掉 packages/subtitle_ocr
        .canonicalize()
        .context("解析仓库根失败（确认从仓库内构建）")?;
    Ok(root)
}

/// 把路径解析为绝对路径：本身已是绝对路径则原样规范化；否则相对仓库根拼接。
pub fn resolve_path(repo_root: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    }
}
