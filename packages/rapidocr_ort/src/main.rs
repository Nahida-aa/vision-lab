//! 命令行：`rapidocr_ort --model v3 screenshot.png`
//!
//! 输出 JSON 数组，每个元素含 text / confidence / score / box（四点 `[[x,y];4]`） / center。

use anyhow::{Context, Result};
use clap::Parser;
use rapidocr_ort::{ModelProfile, OcrBoxResult, OcrEngine};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "rapidocr_ort", about = "PP-OCR 文字识别（基于 ONNX Runtime）")]
struct Cli {
    /// 模型套件：v3 / v6-tiny / v6-medium
    #[arg(long, value_enum, default_value_t = ModelProfile::V4)]
    model: ModelProfile,

    /// 输入图片路径
    image: String,

    /// 模型目录（默认仓库根 data/models/rapidocr，可经 RAPIDOCR_MODEL_DIR 覆盖）
    #[arg(long, env = "RAPIDOCR_MODEL_DIR", default_value = rapidocr_ort::DEFAULT_MODEL_DIR)]
    model_dir: String,
}

#[derive(Serialize)]
struct OcrImgResult {
    /// 使用的模型套件。
    model: String,
    /// 输入图片路径（解析为基于仓库根的绝对路径）。
    image: String,
    /// 输入图片宽度（像素）。
    width: usize,
    /// 输入图片高度（像素）。
    height: usize,
    /// 检测结果。
    results: Vec<OcrBoxResult>,
}

/// 仓库根：从当前可执行文件位置（如 `target/debug/rapidocr_ort`）上溯到
/// `crates/rapidocr_ort` 的父目录（即 workspace 根），再 canonicalize
/// （`cargo run` 经符号链接指向真正的 target 目录，需解析）。
fn current_exe_repo_root() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("获取当前可执行文件路径失败")?;
    let exe_dir = exe.parent().context("可执行文件无父目录")?.to_path_buf();
    let root = exe_dir
        .join("..") // 去掉 target/debug 或 target/release
        .join("..") // 去掉 crates/rapidocr_ort
        .canonicalize()
        .context("解析仓库根失败（确认从仓库内构建）")?;
    Ok(root)
}

/// 把路径解析为绝对路径：本身已是绝对路径则原样规范化；否则相对仓库根拼接。
fn resolve_path(repo_root: &Path, p: &str) -> Result<PathBuf> {
    let path = PathBuf::from(p);
    let abs = if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    };
    Ok(abs)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 把模型目录与输入图片都解析成基于仓库根的绝对路径，避免依赖调用时的 cwd。
    // 仓库根 = 二进制所在目录（target/debug 或 安装目录）往上两级（crates/rapidocr_ort）。
    let repo_root = current_exe_repo_root()?;
    let model_dir = resolve_path(&repo_root, &cli.model_dir)?;
    let image_path = resolve_path(&repo_root, &cli.image)?;

    let mut engine = OcrEngine::from_profile(cli.model, &model_dir).context("构建 OCR 引擎失败")?;

    // 读图 -> BGR HWC u8（对齐 cpp cv::imread / subtitle_ocr 约定，见 rapidocr_ort::load_image）。
    let arr = rapidocr_ort::load_image(&image_path)?;
    let (h, w, _) = arr.dim();

    let results = engine.detect(&arr).context("OCR 推理失败")?;

    let out = OcrImgResult {
        model: format!("{:?}", cli.model),
        image: image_path.display().to_string(),
        width: w,
        height: h,
        results,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&out).context("序列化结果失败")?
    );
    Ok(())
}
