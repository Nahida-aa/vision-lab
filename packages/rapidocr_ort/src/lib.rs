//! # rapidocr_ort
//!
//! 基于 ONNX Runtime 的 PP-OCR 文字识别（检测 + 识别）Rust 实现。
//!
//! 设计要点：
//! - **模型运行时可选**：通过 [`ModelProfile`]（`--model` 参数）选择 v3 / v6-tiny /
//!   v6-medium 套件，权重从 `data/models/rapidocr/` 加载，不条件编译。三件套（det/rec/cls）
//!   与字典随 profile 走。
//! - **既是库也是二进制**：库暴露 [`OcrEngine`] 给上层（如 ui_probe）复用；二进制
//!   `rapidocr_ort` 直接对一张图片出 JSON（文字 + 坐标）。
//! - 推理代码模型无关，同一套 DB 检测后处理 + CRNN/CTC 识别可跑任意 PP-OCR 权重。
//! - 预处理/后处理已按模块拆分（preprocess / det / rec / cls / pipeline），对齐
//!   subtitle-rust（Python rapidocr）的做法；v3 的 rec 输入高度 48、宽度不封顶、
//!   归一化 `(x/255-0.5)/0.5`，与 v6 一致。
//!
//! ## 坐标系
//! 每个检测结果的 `box` 为四个顶点（顺时针：左上、右上、右下、左下）的
//! `[[x, y]; 4]`，与图片像素坐标一致；`center` 为四点平均得到的几何中心，
//! `x_range`/`y_range` 为横/纵值域 `[min, max]`，便于按区域过滤。
//! `score` 为检测框得分，`confidence` 为识别置信度。

pub mod cls;
pub mod det;
pub mod geometry_util;
pub mod pipeline;
pub mod preprocess;
pub mod rec;
pub mod util;

pub use geometry_util::points_range;
pub use util::load_image;

use crate::geometry_util::polygon_metrics;
use anyhow::{Context, Result};
use ndarray::Array3;
use ort::session::Session;
use std::path::Path;
use tracing::debug;

/// 默认模型目录的单一源头：各 CLI 的 `--model-dir` 默认值、示例、测试统一引用
/// 该常量（相对仓库根解析），避免路径字符串散落多处。
pub const DEFAULT_MODEL_DIR: &str = "data/models/rapidocr";

/// 检测框过滤阈值：DB 后处理里框内平均概率低于它的框直接丢弃（PP-OCR 默认 0.6）。
const BOX_THRESH: f32 = 0.6;

/// 模型套件预设。枚举值即 `--model` 的取值。
///
/// 每套对应 `data/models/rapidocr/` 下的一组权重 + 一个字典文件。新增套件只改这里，
/// 不碰推理代码。
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ModelProfile {
    /// PP-OCRv3（成熟、参考多），det/rec/cls 均 v3，字典 ppocr_keys.json。
    V3,
    /// PP-OCRv4（官方升级，det/rec 更准，形近字识别更好），字典 ppocr_keys.json。
    V4,
    /// PP-OCRv6 tiny：快、体积小，字典 ppocrv6_tiny_dict.txt。
    V6Tiny,
    /// PP-OCRv6 medium：更准，模型较大，字典 ppocrv6_dict.txt。
    V6Medium,
}

impl ModelProfile {
    /// 返回 (det, rec, cls, dict) 四个文件的相对 `data/models/rapidocr/` 的路径。
    fn paths(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            ModelProfile::V3 => (
                "ch_PP-OCRv3_det_infer.onnx",
                "ch_PP-OCRv3_rec_infer.onnx",
                "ch_ppocr_mobile_v2.0_cls_infer.onnx",
                "ppocr_keys.json",
            ),
            ModelProfile::V4 => (
                "ch_PP-OCRv4_det_infer.onnx",
                "ch_PP-OCRv4_rec_infer.onnx",
                "ch_ppocr_mobile_v2.0_cls_infer.onnx",
                "ppocrv4_keys.json",
            ),
            ModelProfile::V6Tiny => (
                "pp-ocrv6_tiny_det.onnx",
                "pp-ocrv6_tiny_rec.onnx",
                "ch_ppocr_mobile_v2.0_cls_infer.onnx",
                "ppocrv6_tiny_dict.txt",
            ),
            ModelProfile::V6Medium => (
                "pp-ocrv6_medium_det.onnx",
                "pp-ocrv6_medium_rec.onnx",
                "ch_ppocr_mobile_v2.0_cls_infer.onnx",
                "ppocrv6_dict.txt",
            ),
        }
    }
}

/// 单个文字识别结果（一个检测出的文本框区域 + 识别文本）。
///
/// 唯一定义源在 `ocr_types`（无重型依赖），本 crate re-export 保持旧路径
/// `rapidocr_ort::OcrBoxResult` 兼容。
pub use ocr_types::OcrBoxResult;

/// OCR 引擎：持有 det / cls / rec 三个 Session 与字典。
pub struct OcrEngine {
    profile: ModelProfile,
    det: Session,
    rec: Session,
    cls: Session,
    /// 字典：index -> 字符。第 0 位通常是空白符（CTC blank）。
    vocab: Vec<String>,
    /// det / rec / cls 的输出节点名（PP-OCR v3 为 sigmoid_0.tmp_0 / softmax_5.tmp_0
    /// 等，v6 不同；构建时按索引缓存，避免硬编码）。
    det_out_name: String,
    rec_out_name: String,
    cls_out_name: String,
    /// 是否用 cpp 同款的透视矫正裁剪（warpPerspective）替代轴对齐包围盒。
    /// 默认 false（轴对齐），对近水平文本足够；true 时与 cpp 的 rec 输入一致。
    use_warp_crop: bool,
}

impl OcrEngine {
    /// 按预设套件构建引擎。
    ///
    /// `model_dir` 为 `data/models/rapidocr` 所在目录（默认仓库根的 `data/models/rapidocr`）。
    pub fn from_profile(profile: ModelProfile, model_dir: &Path) -> Result<Self> {
        let (det, rec, cls, dict) = profile.paths();
        let dir = model_dir.to_path_buf();
        let det = build_session(&dir.join(det))?;
        let rec = build_session(&dir.join(rec))?;
        let cls = build_session(&dir.join(cls))?;
        let vocab = load_vocab(&dir.join(dict))
            .with_context(|| format!("加载字典失败: {}", dir.join(dict).display()))?;

        let det_out_name = det.outputs()[0].name().to_string();
        let rec_out_name = rec.outputs()[0].name().to_string();
        let cls_out_name = cls.outputs()[0].name().to_string();

        Ok(Self {
            profile,
            det,
            rec,
            cls,
            vocab,
            det_out_name,
            rec_out_name,
            cls_out_name,
            use_warp_crop: false,
        })
    }

    /// 切换是否用透视矫正裁剪（对齐 cpp 的 warpPerspective）。返回 self 便于链式调用。
    pub fn with_warp_crop(mut self, on: bool) -> Self {
        self.use_warp_crop = on;
        self
    }

    pub fn profile(&self) -> ModelProfile {
        self.profile
    }

    /// 临时诊断：dump det heatmap（sigmoid 后）的 prob 直方图，排查检测漏检。
    pub fn dump_det_heatmap(&mut self, img: &Array3<u8>) {
        let (h, w, _) = img.dim();
        let (det_input, _nh, _nw) = preprocess::preprocess_det(img);
        let det_tensor = ort::value::Tensor::from_array(det_input).expect("tensor");
        let det_out = self
            .det
            .run(ort::inputs!["x" => det_tensor])
            .expect("det 推理失败");
        let det_map = det_out[self.det_out_name.as_str()]
            .try_extract_array::<f32>()
            .unwrap()
            .to_owned();
        let hm_shape = det_map.shape();
        debug!("[det_dump] full det output shape: {:?}", hm_shape);
        let (hm_h, hm_w) = (hm_shape[2], hm_shape[3]);
        let heatmap: Vec<f32> = det_map.into_raw_vec_and_offset().0;
        let sigmoid = heatmap.iter().any(|&v| v > 1.0);
        let prob: Vec<f32> = if sigmoid {
            heatmap.iter().map(|&v| 1.0 / (1.0 + (-v).exp())).collect()
        } else {
            heatmap.to_vec()
        };
        // 统计 prob 分布
        let mut bins = [0usize; 11];
        for &p in &prob {
            let b = ((p * 10.0).floor() as usize).min(10);
            bins[b] += 1;
        }
        println!(
            "det heatmap {}x{} sigmoid={} input_norm=0.5/0.5",
            hm_w, hm_h, sigmoid
        );
        for (i, c) in bins.iter().enumerate() {
            let lo = i as f32 / 10.0;
            let hi = (i + 1) as f32 / 10.0;
            println!("  prob[{:.1},{:.1}) : {} px", lo, hi, c);
        }
        // 找 y 在底部 40% 区域(原图 y>=0.6h)的最大 prob 位置
        // 热力图 y 缩放：scale_h = h / hm_h
        let scale_h = h as f32 / hm_h as f32;
        let scale_w = w as f32 / hm_w as f32;
        let y0 = (0.6 * h as f32 / scale_h) as usize;
        let x0 = 0;
        let x1 = hm_w;
        let mut maxp = 0.0f32;
        let mut maxxy = (0usize, 0usize);
        for yy in y0..hm_h {
            for xx in x0..x1 {
                let p = prob[yy * hm_w + xx];
                if p > maxp {
                    maxp = p;
                    maxxy = (xx, yy);
                }
            }
        }
        println!(
            "bottom40% max prob={:.4} at heatmap({},{}) -> 原图({:.0},{:.0})",
            maxp,
            maxxy.0,
            maxxy.1,
            maxxy.0 as f32 * scale_w,
            maxxy.1 as f32 * scale_h
        );
    }

    /// 临时诊断：仅跑 det 返回原始检测框（不 rec），用于排查检测/识别差异。
    pub fn detect_raw_boxes(&mut self, img: &Array3<u8>) -> Vec<det::DetBox> {
        let (h, w, _) = img.dim();
        let (det_input, _, _) = preprocess::preprocess_det(img);
        let det_tensor = ort::value::Tensor::from_array(det_input).expect("tensor");
        let det_out = self
            .det
            .run(ort::inputs!["x" => det_tensor])
            .expect("det 推理失败");
        let det_map = det_out[self.det_out_name.as_str()]
            .try_extract_array::<f32>()
            .unwrap()
            .to_owned();
        let hm_shape = det_map.shape();
        let (hm_h, hm_w) = (hm_shape[2], hm_shape[3]);
        let heatmap: Vec<f32> = det_map.into_raw_vec_and_offset().0;
        det::db_postprocess(&heatmap, hm_w, hm_h, w, h, BOX_THRESH)
    }

    /// 对一张 BGR 图像（height×width×3，0-255 u8，见 [`load_image`]）做检测 + 识别。
    pub fn detect(&mut self, img: &Array3<u8>) -> Result<Vec<OcrBoxResult>> {
        let (h, w, _) = img.dim();

        // ---- 1. 检测：原图缩放到输入尺寸，归一化后跑 det ----
        let (det_input, det_nh, det_nw) = preprocess::preprocess_det(img);
        debug!("[det] input {}x{} (img {}x{})", det_nh, det_nw, h, w);
        let det_tensor =
            ort::value::Tensor::from_array(det_input).context("构造 det 输入张量失败")?;
        let det_out = self
            .det
            .run(ort::inputs!["x" => det_tensor])
            .context("det 推理失败")?;
        let det_map = det_out[self.det_out_name.as_str()]
            .try_extract_array::<f32>()?
            .to_owned();
        // DB 后处理：sigmoid + 二值化 + 连通域 + minAreaRect + unclip + score。
        // 返回原图坐标系下的四点框（[Vec2;4]）。
        let hm_shape = det_map.shape();
        let (hm_h, hm_w) = (hm_shape[2], hm_shape[3]);
        debug!(
            "[det] det_out_shape={:?} hm={}x{} 但 det_input宽={} 原图/ROI={}x{}",
            hm_shape, hm_w, hm_h, det_nw, w, h
        );
        let heatmap: Vec<f32> = det_map.into_raw_vec_and_offset().0;
        let boxes = det::db_postprocess(&heatmap, hm_w, hm_h, w, h, BOX_THRESH);

        // ---- 2. 逐个文本框裁剪 + 识别 ----
        let mut results = Vec::with_capacity(boxes.len());
        for b in boxes {
            let crop = if self.use_warp_crop {
                pipeline::crop_for_rec_warp(img, &b.polygon)
            } else {
                pipeline::crop_for_rec(img, &b.polygon)
            };
            let (text, score) = pipeline::recognize(
                &crop,
                &mut self.rec,
                &self.rec_out_name,
                &mut self.cls,
                &self.cls_out_name,
                true,
                &self.vocab,
            );
            if text.is_empty() {
                continue;
            }
            // 由四点算 x/y 值域与几何中心（对齐 cpp/ts 的 polygonToXyRange 口径，
            // 这里额外返回几何中心用于点击回灌）。
            let (x_range, y_range, center) = polygon_metrics(&b.polygon);
            results.push(OcrBoxResult {
                text,
                // 文字置信度（rec 分支平均字符概率）。
                text_confidence: score,
                // 框置信度（框内平均概率，来自 det 后处理）。
                box_confidence: b.score,
                // 四个顶点转 [[x,y];4]。
                bbox: b.polygon.map(|p| [p.x, p.y]),
                // 几何中心（四点平均），便于点击回灌。
                center,
                // 横/纵值域，便于按区域过滤。
                x_range,
                y_range,
            });
        }
        Ok(results)
    }
}

// ===========================================================================
// 引擎构建辅助
// ===========================================================================

/// 构建 ONNX Session（文件存在性已校验）。
fn build_session(path: &Path) -> Result<Session> {
    Session::builder()?
        .commit_from_file(path)
        .with_context(|| format!("加载 ONNX 模型失败: {}", path.display()))
}

/// 读取字典文件。
/// - PP-OCRv3 的 `ppocr_keys.json` 是 JSON 字符串数组（每个元素是一个字符/词，
///   索引 0 为空白符），用 serde_json 解析。
/// - PP-OCRv6 的 `.txt` 每行一个字符，按行读取。
fn load_vocab(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)?;
    match serde_json::from_str::<Vec<String>>(&content) {
        Ok(arr) => return Ok(arr),
        Err(_) => {}
    }
    Ok(content.lines().map(|l| l.trim_end().to_string()).collect())
}

// 避免未使用告警：Axis / Array4 在模块间用到，这里仅保有关联导入的文档性引用。

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 仓库根：crate 在 crates/rapidocr_ort，往上两级即仓库根。
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn load_image(path: &Path) -> Array3<u8> {
        super::load_image(path).unwrap()
    }

    #[test]
    fn v3_detects_text_on_fixtures() {
        let root = repo_root();
        let model_dir = root.join(DEFAULT_MODEL_DIR);
        let fixtures = root.join("tests/fixtures");

        let mut engine = OcrEngine::from_profile(ModelProfile::V4, &model_dir)
            .expect("加载 v4 引擎失败（确认 data/models/rapidocr 权重已就绪）");

        let cases = [
            ("ui_stable1.png", "Count"),
            ("ui_big1.png", "OCR"),
            ("ui_nat1.png", "World"),
        ];
        for (name, expect) in cases {
            let img = load_image(&fixtures.join(name));
            let results = engine.detect(&img).expect("OCR 推理失败");
            assert!(!results.is_empty(), "{} 应检测到文字", name);
            let joined: String = results.iter().map(|r| r.text.clone()).collect();
            let joined_lower = joined.to_lowercase();
            let expect_lower = expect.to_lowercase();
            assert!(
                joined_lower.contains(&expect_lower),
                "{} 识别结果 {} 应包含 '{}'（大小写不敏感）",
                name,
                joined,
                expect
            );
        }
    }
}
