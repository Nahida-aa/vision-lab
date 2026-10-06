//! 命令行：把多格漫画长图切成紧致的独立子图。
//!
//!   cargo run -p ocr_layout --example split-panels -- <图片或目录> [--out <dir>] [--threshold N] [--min-area N] [--bbox] [--content] [--mode sprite|free|auto] [--min-gap N] [--edge-trim N]
//!
//! 两种检测 (默认走 2D, 不再默认旧的 1D content):
//!
//! - **2D 连通域 (默认, `split_panels_cc`)**: 对「非白」掩膜 (每像素最暗通道 <
//!   `threshold`, 默认 245) 做 8-邻接连通域, 每张卡片当一个**实心区域** (填掉内部
//!   白洞, 如白色对话框); 卡片形状跟随实际轮廓 (圆角/抖动) → 按 alpha 贴纸裁切
//!   (卡片外透明、0 白边), 对浅色背景鲁棒、不会把一张卡打碎。加 `--bbox` 出矩形包围盒裁剪。
//! - **1D content (旧, `--content`)**: `split_panels` 暗 (min<195)=内容, 行投影+四边
//!   收缩; 适合深色背景卡片/内容嵌白布; 浅色背景会把背景误当缝、把卡打碎。`--mode`
//!   sprite|free|auto 选等大统一。
//!
//! 2D 模式细节:
//! 1. **非白掩膜**: 每像素最暗通道 >= `threshold` (默认 245) 判「白」(画布/白缝),
//!    其下 = 卡片体 (背景+内容+薄晕边)。
//! 2. **填洞**: 「触图像边界的白色 CC」= 外白布 (卡间白缝经边缘白布连通, 也是外);
//!    不触边的白 = 内部洞 (白色对话框等) → 填进卡片。
//! 3. **连通域**: 卡片掩膜上 8-邻接 CC, 每个够大的 CC = 一张卡 (`--min-area` 滤噪,
//!    默认源图面积 1/200)。
//! 4. **输出**: 默认 alpha 贴纸裁切 (卡片形不透明、外透明); `--bbox` 出矩形。总产
//!    `<name>_preview.png` (带彩色包围盒的原图, 供人工核对) 与 `<name>_<序号>.png`。

use anyhow::Context;
use clap::Parser;
use image::RgbaImage;
use image_util::crop_rgb;
use ocr_layout::panels::{Mode, FRAME_THRESHOLD, split_panels, split_panels_cc, unify_sizes};
use ocr_layout::annotate;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "split-panels",
    about = "把多格漫画长图切成紧致的独立子图 (纯图片构成的 UI; 默认 2D 连通域, alpha 0 白边)"
)]
struct Args {
    /// 图片文件，或包含图片的目录
    input: PathBuf,
    /// 输出目录；默认 <输入所在目录>/out/
    #[arg(short, long)]
    out: Option<PathBuf>,
    /// 非白/内容判定阈值: 每像素最暗通道 >= 此值为白。
    /// 2D 默认 245 (卡片框); --content (1D) 默认 195 (暗内容)。
    #[arg(long)]
    threshold: Option<u8>,
    /// 分隔带最小厚度（行/列数, 仅 1D content 模式）
    #[arg(long, default_value_t = 6)]
    min_gap: u32,
    /// 卡片连通域最小面积 (px), 滤噪点 (如白缝里的暗噪点)。默认源图面积 1/200。
    #[arg(long)]
    min_area: Option<u32>,
    /// 2D 模式: 出矩形包围盒裁剪, 而非 alpha 贴纸形状
    #[arg(long)]
    bbox: bool,
    /// 2D 卡片框 (alpha)——现为默认, 此旗标仅作向后兼容 (指定无副作用)
    #[arg(long)]
    frame: bool,
    /// 旧 1D content 模式 (暗 < 阈值 检测 + 四边收缩 + sprite/free/auto 统一)
    #[arg(long)]
    content: bool,
    /// 1D 拼接布局模式：sprite=卡片等大 / free=逐格独立 / auto=按边界极差判定
    #[arg(long, value_enum, default_value_t = ArgsMode::Auto)]
    mode: ArgsMode,
    /// 裁剪向内缩 N 像素去卡片边缘亮晕边 (bbox/content 模式; 2D alpha 无需)
    #[arg(long, default_value_t = 0)]
    edge_trim: u32,
    /// 2D 模式: 形态学膨胀桥接像素数 (合并断裂卡片/气泡). 默认 4.
    #[arg(long, default_value_t = 8)]
    bridge: u32,
    /// 2D 模式: alpha 掩膜腐蚀像素数 (去反锯齿白边, 0白边). 默认 1.
    #[arg(long, default_value_t = 1)]
    erode: u32,
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
        min_area,
        bbox,
        frame,
        content,
        mode,
        edge_trim,
        bridge,
        erode,
    } = args;
    if frame {
        eprintln!("[note] --frame 现为默认 (2D 卡片框), 此旗标无副作用");
    }

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

    // 默认 2D 卡片框 (非白 245); --content = 旧 1D 暗内容 (195)。
    let threshold = threshold.unwrap_or(if content { 195 } else { FRAME_THRESHOLD });

    if content {
        run_content(&inputs, &out_dir, threshold, min_gap, mode.into(), edge_trim)?;
    } else {
        run_cc(&inputs, &out_dir, threshold, min_area, bbox, edge_trim, bridge, erode)?;
    }
    Ok(())
}

/// 2D 连通域 (默认): 非白掩膜 + 填洞 + CC → alpha 贴纸裁切 (或 --bbox 矩形)。
fn run_cc(
    inputs: &[PathBuf],
    out_dir: &Path,
    threshold: u8,
    min_area: Option<u32>,
    bbox: bool,
    edge_trim: u32,
    bridge: u32,
    erode: u32,
) -> anyhow::Result<()> {
    let mut total = 0usize;
    for src in inputs {
        let name = stem(src);
        let img = image::open(src)
            .with_context(|| format!("读取图片失败: {}", src.display()))?
            .to_rgb8();
        let area = (img.width() as u32) * (img.height() as u32);
        let min_area = min_area.unwrap_or(area / 200);
        let panels = split_panels_cc(&img, threshold, min_area, bridge, erode);
        eprintln!(
            "[split] cc thr={threshold} min_area={min_area} {} {}x{} → {} 卡",
            src.display(),
            img.width(),
            img.height(),
            panels.len()
        );

        // 预览: 带彩色包围盒的原图 (bbox 近似卡片形), 总是产出供人工核对。
        let rects: Vec<(u32, u32, u32, u32)> = panels.iter().map(|p| p.bbox).collect();
        let widgets = ocr_layout::panels::rects_to_widgets(&rects, area);
        let preview = out_dir.join(format!("{name}_preview.png"));
        annotate(&img, &widgets)
            .save(&preview)
            .with_context(|| format!("保存失败: {}", preview.display()))?;
        eprintln!("        预览 → {}", preview.display());

        for (idx, panel) in panels.iter().enumerate() {
            let (x, y, cw, ch) = panel.bbox;
            let out = out_dir.join(format!("{name}-{}.png", idx + 1));
            if bbox {
                // 矩形包围盒裁剪 (可按 edge_trim 内缩去亮晕边)
                let t = edge_trim;
                let nx = x + t;
                let ny = y + t;
                let nw = cw.saturating_sub(2 * t);
                let nh = ch.saturating_sub(2 * t);
                if nw == 0 || nh == 0 {
                    continue;
                }
                crop_rgb(&img, nx, ny, nw, nh)
                    .save(&out)
                    .with_context(|| format!("保存失败: {}", out.display()))?;
            } else {
                // alpha 贴纸裁切: 卡片形不透明, 外部透明 (0 白边)
                let crop = crop_rgb(&img, x, y, cw, ch);
                let mut rgba = RgbaImage::new(cw, ch);
                for py in 0..ch {
                    for px in 0..cw {
                        let [r, g, b] = crop.get_pixel(px, py).0;
                        let a = panel.mask[(py as usize) * (cw as usize) + (px as usize)];
                        rgba.put_pixel(px, py, image::Rgba([r, g, b, a]));
                    }
                }
                rgba
                    .save(&out)
                    .with_context(|| format!("保存失败: {}", out.display()))?;
            }
            eprintln!(
                "        {}-{}  bbox=({}, {}, {}, {}) area={}",
                name,
                idx + 1,
                x,
                y,
                cw,
                ch,
                panel.area
            );
            total += 1;
        }
    }
    eprintln!("[done] 共 {total} 卡 → {}", out_dir.display());
    Ok(())
}

/// 1D content (旧 --content): 行投影 + 四边收缩 + sprite/free/auto 统一。
fn run_content(
    inputs: &[PathBuf],
    out_dir: &Path,
    threshold: u8,
    min_gap: u32,
    mode: Mode,
    edge_trim: u32,
) -> anyhow::Result<()> {
    let mut results: Vec<(String, u32, u32, Vec<(u32, u32, u32, u32)>)> = Vec::new();
    for src in inputs {
        let name = stem(src);
        let img = image::open(src)
            .with_context(|| format!("读取图片失败: {}", src.display()))?
            .to_rgb8();
        let mut rects = split_panels(&img, threshold, min_gap);
        if edge_trim > 0 {
            let t = edge_trim;
            rects = rects
                .into_iter()
                .map(|(x, y, w, h)| (x + t, y + t, w.saturating_sub(2 * t), h.saturating_sub(2 * t)))
                .filter(|(_, _, w, h)| *w > 0 && *h > 0)
                .collect();
        }
        eprintln!(
            "[split] 1d thr={threshold} trim={edge_trim} {} {}x{} → {} 块",
            src.display(),
            img.width(),
            img.height(),
            rects.len()
        );
        results.push((name, img.width(), img.height(), rects));
    }

    let resolved = unify_sizes(&mut results, mode);

    let mut total = 0usize;
    for (name, w, _h, rects) in &results {
        let src = inputs
            .iter()
            .find(|s| stem(s) == *name)
            .context("源图丢失")?;
        let img = image::open(src)
            .with_context(|| format!("读取图片失败: {}", src.display()))?
            .to_rgb8();

        let eff_mode = resolved.get(w).copied().unwrap_or(Mode::Free);
        let widgets = ocr_layout::panels::rects_to_widgets(rects, img.width() * img.height());
        let preview = out_dir.join(format!("{name}_preview_{}.png", eff_mode.as_str()));
        annotate(&img, &widgets)
            .save(&preview)
            .with_context(|| format!("保存失败: {}", preview.display()))?;
        eprintln!("        预览 → {}", preview.display());

        for (idx, (x, y, cw, ch)) in rects.iter().enumerate() {
            let out = out_dir.join(format!("{name}-{}.png", idx + 1));
            crop_rgb(&img, *x, *y, *cw, *ch)
                .save(&out)
                .with_context(|| format!("保存失败: {}", out.display()))?;
            eprintln!("        {name}-{}  rect=({x},{y},{cw},{ch})", idx + 1);
            total += 1;
        }
    }
    eprintln!("[done] 共 {total} 块 → {}", out_dir.display());
    Ok(())
}

/// 文件名 (无扩展名)。
fn stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "img".into())
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
