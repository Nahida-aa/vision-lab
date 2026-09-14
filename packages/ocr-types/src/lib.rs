//! `subtitle-ocr` 引擎的原子检测框类型（纯数据，无 ort/opencv 依赖）。
//!
//! 本 crate 只保留最底层的 OCR 检测产物 [`OcrBoxResult`]，并为兼容保留
//! [`SubtitleSegment`] 透出（其规范定义在 `subtitle` crate）。
//!
//! 更高层的字幕 / OCR 产物与管线类型（`FrameResult` / `OcrSegment` /
//! `OcrFramesResult` / `MergeFrames*` / `XStats` / `YStats` 及工具函数）已迁到
//! `subtitle-ocr-types`。
//!
//! 分层：`subtitle`（纯字幕领域）← `ocr-types`（原子检测框）← `subtitle-ocr-types`。

use serde::{Deserialize, Serialize};

// 纯字幕领域类型本定义在 `subtitle` crate；此处透出仅为兼容既有 `ocr_types::*`
// 引用（LocalDub fnrpc 等），规范来源仍是 `subtitle`。
pub use subtitle::SubtitleSegment;

/// 单个文字识别区域（detected text box）。
///
/// Copied from `rapidocr_ort::OcrBoxResult` to avoid pulling in ort dependency.
/// Fields are identical — a plain data struct with no heavy deps.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrBoxResult {
    /// 识别出的文字。
    pub text: String,
    /// 文字置信度（rec 分支平均字符概率），反映「字认得准不准」。
    pub text_confidence: f32,
    /// 框置信度（det 后处理里框内平均概率），反映「框定位得准不准」。
    pub box_confidence: f32,
    /// 四个顶点（顺时针：左上、右上、右下、左下），原图像素坐标。
    pub bbox: [[f32; 2]; 4],
    /// 横向值域 `[min_x, max_x]`（像素坐标），便于按列/区域过滤。
    pub x_range: [f32; 2],
    /// 纵向值域 `[min_y, max_y]`（像素坐标），便于按行/区域过滤。
    pub y_range: [f32; 2],
    /// 几何中心（四点平均），便于操作回灌（点击中心点）。
    pub center: [f32; 2],
}
