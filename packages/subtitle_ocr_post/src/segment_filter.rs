//! 字幕段置信度过滤。

use crate::segment_adjust::OcrSegmentWithAdjust;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "specta-types", derive(specta::Type))]
pub struct OcrSegmentFilterResult {
    pub meta: OcrSegmentFilterMeta,
    pub result: OcrSegmentFilterData,
}

#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "specta-types", derive(specta::Type))]
pub struct OcrSegmentFilterMeta {
    pub segment_count: u32,
    pub text_confidence_threshold: f32,
    pub dropped: u32,
}

#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "specta-types", derive(specta::Type))]
pub struct OcrSegmentFilterData {
    pub text: String,
    pub segments: Vec<OcrSegmentWithAdjust>,
}

/// 按置信度过滤字幕段（≤0 视为不过滤）。
pub fn ocr_segment_filter(
    segments: &[OcrSegmentWithAdjust], text_confidence_threshold: f32,
) -> Vec<OcrSegmentWithAdjust> {
    if text_confidence_threshold <= 0.0 { return segments.to_vec(); }
    segments.iter().filter(|s| {
        let conf = s.adjusted_confidence.unwrap_or(s.base.text_confidence);
        conf >= text_confidence_threshold
    }).cloned().collect()
}

/// 按置信度过滤并返回带统计的结果。
pub fn ocr_segment_filter_with_meta(
    segments: &[OcrSegmentWithAdjust], text_confidence_threshold: f32,
) -> OcrSegmentFilterResult {
    let filtered = ocr_segment_filter(segments, text_confidence_threshold);
    let dropped = segments.len() - filtered.len();
    let text = filtered.iter().map(|s| s.base.base.text.as_str()).collect::<Vec<_>>().join(" ");
    OcrSegmentFilterResult {
        meta: OcrSegmentFilterMeta {
            segment_count: filtered.len() as u32,
            text_confidence_threshold,
            dropped: dropped as u32,
        },
        result: OcrSegmentFilterData { text, segments: filtered },
    }
}
