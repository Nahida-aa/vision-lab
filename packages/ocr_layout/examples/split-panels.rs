//! 命令行：把多格漫画长图切成紧致的独立子图。
//!
//!   cargo run -p ocr_layout --example split-panels -- <图片或目录> [--out <dir>] [--threshold N] [--min-gap N] [--mode sprite|free|auto]
//!
//! 把「纯图片构成的 UI」交给布局层：每一格就是一条**无文字信号的 Widget**
//! （source=Color）。算法分三层：
//!
//! 1. **行投影切行**：`row_dark[y]` = 该行最暗像素亮度；整行无内容的连续行
//!    （亮度 >= `threshold`，需容忍 JPEG 脏白）达到 `min_gap` 视为格间分隔带，
//!    带间即行块。不做网格整齐排列假设。
//!
//! 2. **四边收缩**：行块边缘常有振铃/浅灰渐变白边（min 198–253 不等）。从每
//!    条边向内收缩「整行/列 min >= threshold」的浅色边缘——阈值 195 恰好分离
//!    两类区域：白边/画布 (min >= 195) 与卡片内容 (min <= 180，含灰蓝背景)。
//!    渐变过渡不需要「突变」：只要内容首行/列比白边深，收缩就会停在那里。
//!
//!    历史教训（都已实测）：梯度跳变检测在渐变边界上时灵时不灵（2-1 右侧
//!    11 列漏裁）；逐像素占比判据同因（渐变中相邻像素差不足）；顶/底紧致会
//!    被画面内斜线（桌沿）拖尾误裁 3-2 底部 37px——行块边界由分隔带给出后，
//!    这些花活全部不需要。
//!
//! 3. **模式**（`--mode`）：
//!    - `sprite`（精灵图）：同源卡片等大。各格收缩后的尺寸有 ±几 px 噪声
//!      （渐变带内边缘位置的抖动），统一到组内**最紧**（left=max、x1=min、
//!      height=min）——零白边保证；多裁的 1-4px 是边缘渐变带，无视觉内容。
//!    - `free`（自由拼图）：逐格保留自己的收缩结果，不等大。
//!    - `auto`（默认）：组内参与格的 left / x1 / 高度 极差都 <= 12px 视为
//!      等大卡片（sprite），否则按 free 处理。
//!
//!    参与格 = 边界可信的格子：收缩后 `x0 > 0` 且 `x1 < w` 且 `y1 < h`。
//!    贴源图边的格子（如 4.jpg 底格卡片超出源图、全宽到底）边界不可信，
//!    不参与统计也保持原样——统一它们会裁掉真实内容。
//!
//! 输出：总是同时产出 `<name>-preview.png`（带彩色框线的原图，annotate 复用，
//! 供人工核对）与裁剪的 `<name>-<序号>.png`，写入 `<输入所在目录>/out/`。

use anyhow::Context;
use clap::Parser;
use image_util::crop_rgb;
use ocr_layout::panels::{Mode, split_panels, unify_sizes};
use ocr_layout::annotate;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "split-panels",
    about = "把多格漫画长图切成紧致的独立子图 (纯图片构成的 UI)"
)]
struct Args {
    /// 图片文件，或包含图片的目录
    input: PathBuf,
    /// 输出目录；默认 <输入所在目录>/out/
    #[arg(short, long)]
    out: Option<PathBuf>,
    /// 白底判定阈值：整行/列 min >= 此值视为分隔带/白边。
    /// 195 恰好分离白边/画布 (>=195) 与卡片内容 (<=180)。
    #[arg(long, default_value_t = 195)]
    threshold: u8,
    /// 分隔带最小厚度（行/列数）
    #[arg(long, default_value_t = 6)]
    min_gap: u32,
    /// 拼接布局模式：sprite=卡片等大 / free=逐格独立 / auto=按边界极差判定
    #[arg(long, value_enum, default_value_t = ArgsMode::Auto)]
    mode: ArgsMode,
}

/// CLI 侧的 Mode 包装: derive ValueEnum (库 Mode 不依赖 clap)。
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ArgsMode {
    Sprite,
    Free,
    Auto,
}

impl From<ArgsMode> for Mode {
    fn from(m: ArgsMode) -> Self {
        match m {
            ArgsMode::Sprite => Mode::Sprite,
            ArgsMode::Free => Mode::Free,
            ArgsMode::Auto => Mode::Auto,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let Args {
        input,
        out: out_dir,
        threshold,
        min_gap,
        mode,
    } = args;
    let mode: Mode = mode.into();

    let inputs = collect_images(&input)?;
    if inputs.is_empty() {
        anyhow::bail!("{} 下没有找到图片", input.display());
    }

    let out_dir = out_dir.unwrap_or_else(|| {
        let base = if input.is_dir() {
            input.clone()
        } else {
            input.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        base.join("out")
    });
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("创建输出目录失败: {}", out_dir.display()))?;

    // 先对每张图跑切分，收集 (源名, 源宽, 源高, rects)。
    let mut results: Vec<(String, u32, u32, Vec<(u32, u32, u32, u32)>)> = Vec::new();
    for src in &inputs {
        let name = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "img".into());
        let img = image::open(src)
            .with_context(|| format!("读取图片失败: {}", src.display()))?
            .to_rgb8();

        let rects = split_panels(&img, threshold, min_gap);
        eprintln!(
            "[split] {} {}x{} → {} 块",
            src.display(),
            img.width(),
            img.height(),
            rects.len()
        );
        results.push((name, img.width(), img.height(), rects));
    }

    // 组内一致性（按模式）。
    unify_sizes(&mut results, mode);

    // 输出：预览总是产出 + 裁剪。
    let mut total = 0usize;
    for (name, _w, _h, rects) in &results {
        let src = inputs
            .iter()
            .find(|s| {
                s.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .as_deref()
                    == Some(name.as_str())
            })
            .context("源图丢失")?;
        let img = image::open(src)
            .with_context(|| format!("读取图片失败: {}", src.display()))?
            .to_rgb8();

        // 预览总是产出：带彩框原图，随时可人工核对切分框。
        let widgets = ocr_layout::panels::rects_to_widgets(rects, img.width() * img.height());
        let preview = out_dir.join(format!("{name}-preview.png"));
        annotate(&img, &widgets)
            .save(&preview)
            .with_context(|| format!("保存失败: {}", preview.display()))?;
        eprintln!("        预览 → {}", preview.display());

        for (idx, (x, y, cw, ch)) in rects.iter().enumerate() {
            let out = out_dir.join(format!("{name}-{}.png", idx + 1));
            crop_rgb(&img, *x, *y, *cw, *ch)
                .save(&out)
                .with_context(|| format!("保存失败: {}", out.display()))?;
            eprintln!("        {}-{}  rect=({x},{y},{cw},{ch})", name, idx + 1);
            total += 1;
        }
    }
    eprintln!("[done] 共 {total} 块 → {}", out_dir.display());
    Ok(())
}

/// 收集要处理的图片（单文件或目录下的常见格式）。
fn collect_images(input: &Path) -> anyhow::Result<Vec<PathBuf>> {
    const EXTS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "bmp"];
    if input.is_file() {
        return Ok(vec![input.to_path_buf()]);
    }
    let mut out: Vec<PathBuf> = std::fs::read_dir(input)
        .with_context(|| format!("读取目录失败: {}", input.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    out.sort();
    Ok(out)
}
