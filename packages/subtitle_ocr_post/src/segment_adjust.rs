//! 字幕段置信度调整（Y 偏移惩罚 + 孤立惩罚）。

use subtitle_ocr_types::{FrameResult, OcrSegment, YStats};
use serde::{Deserialize, Serialize};

/// 字幕段置信度调整参数。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OcrSegmentAdjustArgs {
    #[serde(default = "default_iso_threshold_ms")]
    pub iso_threshold_ms: Option<u64>,
    #[serde(default = "default_adjust_y_weight")]
    pub adjust_y_weight: Option<f32>,
    #[serde(default = "default_adjust_iso_weight")]
    pub adjust_iso_weight: Option<f32>,
    #[serde(default = "default_adjust_y_factor")]
    pub adjust_y_factor: Option<f32>,
}

fn default_iso_threshold_ms() -> Option<u64> { Some(1500) }
fn default_adjust_y_weight() -> Option<f32> { Some(0.8) }
fn default_adjust_iso_weight() -> Option<f32> { Some(0.2) }
fn default_adjust_y_factor() -> Option<f32> { Some(0.08) }

impl OcrSegmentAdjustArgs {
    pub fn iso_threshold_ms(&self) -> u64 { self.iso_threshold_ms.unwrap_or(1500) }
    pub fn adjust_y_weight(&self) -> f32 { self.adjust_y_weight.unwrap_or(0.8) }
    pub fn adjust_iso_weight(&self) -> f32 { self.adjust_iso_weight.unwrap_or(0.2) }
    pub fn adjust_y_factor(&self) -> f32 { self.adjust_y_factor.unwrap_or(0.08) }
}

/// 应用置信度调整后的字幕段。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "specta-types", derive(specta::Type))]
pub struct OcrSegmentWithAdjust {
    #[serde(flatten)]
    pub base: OcrSegment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adjusted_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso_penalty: Option<f32>,
}

fn compute_y_penalty(seg: &OcrSegment, avg_centroid: f32, video_height: f32, adjust_y_factor: f32) -> f32 {
    let y_range = match seg.y_range { Some(y) => y, None => return 0.0 };
    let centroid = (y_range[0] + y_range[1]) / 2.0;
    let offset = (centroid - avg_centroid).abs();
    let denom = video_height * adjust_y_factor;
    if denom > 0.0 { (offset / denom).min(1.0) } else { 0.0 }
}

fn compute_iso_penalty(seg: &OcrSegment, non_empty_ts: &[u64], iso_threshold_ms: u64) -> f32 {
    if seg.frame_count != Some(1) { return 0.0; }
    let mid = (seg.base.start_ms as u64 + seg.base.end_ms as u64) / 2;
    let before = non_empty_ts.iter().rev().find(|&&t| t < mid).copied();
    let after = non_empty_ts.iter().find(|&&t| t > mid).copied();
    let nearest_gap: f64 = match (before, after) {
        (Some(b), Some(a)) => (mid - b).min(a - mid) as f64,
        (Some(b), None) => (mid - b) as f64,
        (None, Some(a)) => (a - mid) as f64,
        (None, None) => f64::INFINITY,
    };
    ((nearest_gap / iso_threshold_ms as f64).min(1.0)) as f32
}

/// 把逐段 OcrSegment 调整出最终置信度。
pub fn ocr_segment_adjust(
    segments: &[OcrSegment], frame_results: &[FrameResult], y_stats: &YStats,
    video_height: f32, args: &OcrSegmentAdjustArgs,
) -> Vec<OcrSegmentWithAdjust> {
    if segments.is_empty() || (y_stats.avg[0] == 0.0 && y_stats.avg[1] == 0.0) {
        return segments.iter().map(|s| OcrSegmentWithAdjust {
            base: s.clone(), adjusted_confidence: None, y_penalty: None, iso_penalty: None,
        }).collect();
    }
    let avg_centroid = (y_stats.avg[0] + y_stats.avg[1]) / 2.0;
    let mut non_empty_ts: Vec<u64> = frame_results.iter()
        .filter(|f| !f.text.is_empty() && f.x_range != [0.0, 0.0] && f.y_range != [0.0, 0.0])
        .map(|f| f.timestamp).collect();
    non_empty_ts.sort_unstable();

    segments.iter().map(|seg| {
        if seg.frame_count.is_none() {
            return OcrSegmentWithAdjust { base: seg.clone(), adjusted_confidence: None, y_penalty: None, iso_penalty: None };
        }
        let y_penalty = compute_y_penalty(seg, avg_centroid, video_height, args.adjust_y_factor());
        let iso_penalty = compute_iso_penalty(seg, &non_empty_ts, args.iso_threshold_ms());
        let total_penalty = args.adjust_y_weight() as f64 * y_penalty as f64 + args.adjust_iso_weight() as f64 * iso_penalty as f64;
        let adjusted_confidence = seg.text_confidence as f64 * (1.0 - total_penalty).max(0.0);
        OcrSegmentWithAdjust {
            base: seg.clone(),
            adjusted_confidence: Some(adjusted_confidence as f32),
            y_penalty: Some(y_penalty),
            iso_penalty: Some(iso_penalty),
        }
    }).collect()
}
