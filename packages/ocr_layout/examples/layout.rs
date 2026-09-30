//! 命令行：对一张图跑 `ocr_layout` 的布局分析，打印候选控件（JSON）。
//!
//!   cargo run -p ocr_layout --example layout -- <image.png> [model_dir] [quant_bits] [merge_distance] [min_area_ratio]
//!
//! 默认 model_dir 为仓库根 data/models/rapidocr；若只想要纯颜色分析（不加载 OCR），
//! 传 `--no-ocr`。输出每个控件的 id / label / rect / color / area_ratio / source。

use anyhow::Context as _;
use color_analysis::SegmentOpts;
use ocr_layout::{LayoutAnalyzer, Widget};
use rapidocr_ort::ModelProfile;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
struct Output {
    image: String,
    widgets: Vec<Widget>,
}

fn repo_root() -> PathBuf {
    // 本示例在 target/debug/examples，往上两级到 crate，再往上到仓库根。
    let exe = std::env::current_exe().expect("当前可执行文件路径");
    exe.parent()
        .unwrap()
        .join("..") // target/debug
        .join("..") // crates/ocr_layout
        .join("..") // 仓库根
        .canonicalize()
        .expect("解析仓库根失败")
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        anyhow::bail!(
            "用法: layout <image.png> [--no-ocr] [--annotate <out.png>] [model_dir] [quant_bits] [merge_distance] [min_area_ratio]"
        );
    }
    let image_path = args[1].clone();
    let no_ocr = args.iter().any(|a| a == "--no-ocr");
    // 解析 --annotate <path>。
    let mut annotate_path: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--annotate" {
            if let Some(p) = args.get(i + 1) {
                annotate_path = Some(p.clone());
                i += 2;
                continue;
            }
        }
        i += 1;
    }

    let mut positional = args
        .iter()
        .skip(2)
        .filter(|a| *a != "--no-ocr" && *a != "--annotate" && annotate_path.as_ref() != Some(*a));
    let model_dir = positional
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR));
    let quant_bits = positional.next().and_then(|s| s.parse().ok()).unwrap_or(4);
    let merge_distance = positional.next().and_then(|s| s.parse().ok()).unwrap_or(6);
    let min_area_ratio = positional
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0005);

    let img = image::open(&image_path)
        .with_context(|| format!("读取图片失败: {image_path}"))?
        .to_rgb8();

    let opts = SegmentOpts {
        quant_bits,
        merge_distance,
        min_area_ratio,
    };

    let mut analyzer = if no_ocr {
        LayoutAnalyzer::color_only(opts)
    } else {
        LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, opts)
            .context("构建 OCR 引擎失败（确认 data/models/rapidocr 权重就绪）")?
    };

    let widgets = analyzer.analyze(&img)?;

    let out = Output {
        image: image_path,
        widgets: widgets.clone(),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);

    // 可选：把控件画回原图，方便人工核对 UI 理解是否正确。
    if let Some(p) = annotate_path {
        let annotated = ocr_layout::annotate(&img, &widgets);
        annotated
            .save(&p)
            .with_context(|| format!("保存标注图失败: {p}"))?;
        eprintln!("已保存标注图: {}", p);
    }
    Ok(())
}
