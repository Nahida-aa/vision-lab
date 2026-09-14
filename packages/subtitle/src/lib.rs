//! 纯字幕领域类型（与时间轴相关的通用字幕对象，不含 OCR 内部细节）。
//!
//! 本 crate 是 OCR 管线的下游领域底座：只描述「一段带时间跨度的文本」这种
//! 与识别方式无关的概念。OCR 派生字段（置信度 / 框几何 / 源帧）由 `ocr-types`
//! 的 [`ocr_types::OcrSegment`] 通过 `#[serde(flatten)]` 内嵌扩展。
//!
//! 分层：`subtitle`（纯字幕领域）→ `ocr-types`（OCR 管线 IO）→ `subtitle-ocr-post`（算法）。

use serde::{Deserialize, Serialize};

/// 一段字幕：文本与时间跨度。
///
/// 与识别来源无关（`OCR` / `ASR` / 手工校对产出的字幕段都是它）。毫秒时间戳。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "specta-types", derive(specta::Type))]
pub struct SubtitleSegment {
    /// 字幕文本。
    pub text: String,
    /// 起始时间（毫秒）。
    pub start_ms: u32,
    /// 结束时间（毫秒）。
    pub end_ms: u32,
}
