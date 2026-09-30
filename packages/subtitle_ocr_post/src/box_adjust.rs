//! 字幕框调整（行对齐后的离群剔除 / 置信度调整）。

use ocr_types::OcrBoxResult;
use subtitle_ocr_types::{FrameResult, XStats, YStats};
use serde::{Deserialize, Serialize};

/// box 调整的置信度阈值参数。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BoxAdjustedArgs {
    #[serde(default = "default_box_adjusted_threshold", rename = "boxAdjustedThreshold")]
    pub box_adjusted_threshold: Option<f32>,
}

fn default_box_adjusted_threshold() -> Option<f32> { Some(0.5) }

impl BoxAdjustedArgs {
    pub fn threshold(&self) -> f32 { self.box_adjusted_threshold.unwrap_or(0.5) }
}

/// 调整后（行对齐）的单个字幕框：原 OcrBoxResult + 调整附加字段。
#[derive(Clone, Debug, Serialize)]
pub struct OcrBoxResultWithAdjust {
    #[serde(flatten)]
    pub base: OcrBoxResult,
    pub y_center_offset_ratio: f32,
    pub x_center_offset_ratio: f32,
    pub height: f32,
    pub height_ratio: f32,
    pub y_penalty: f32,
    pub x_penalty: f32,
    pub height_penalty: f32,
    pub total_penalty: f32,
    pub is_outlier: bool,
    pub adjusted_confidence: f32,
}

/// 调整后的一帧（不含原 boxes，含调整后的 boxes）。
#[derive(Clone, Debug, Serialize)]
pub struct FrameResultBoxWithAdjust {
    pub text: String,
    pub text_confidence: f64,
    pub x_range: [f32; 2],
    pub y_range: [f32; 2],
    pub timestamp: u64,
    pub boxes: Vec<OcrBoxResultWithAdjust>,
}

impl From<FrameResultBoxWithAdjust> for FrameResult {
    fn from(f: FrameResultBoxWithAdjust) -> FrameResult {
        FrameResult {
            text: f.text, text_confidence: f.text_confidence,
            x_range: f.x_range, y_range: f.y_range, timestamp: f.timestamp,
            boxes: f.boxes.into_iter().map(|b| b.base).collect(),
        }
    }
}

/// `ocr_frames_adjust_box` 返回结构。
#[derive(Clone, Debug, Serialize)]
pub struct OcrBoxAdjustResult {
    pub frames: Vec<FrameResultBoxWithAdjust>,
    pub meta: OcrBoxAdjustResultMeta,
}

/// `OcrBoxAdjustResult` 的 meta。
#[derive(Clone, Debug, Serialize)]
pub struct OcrBoxAdjustResultMeta {
    pub y_stats: YStats,
    pub x_stats: XStats,
    pub frame_count: usize,
    pub args: BoxAdjustedArgs,
}

/// `ocr_frames_filter_box` 返回结构。
#[derive(Clone, Debug, Serialize)]
pub struct OcrFramesBoxFilteredResult {
    pub frames: Vec<FrameResult>,
    pub meta: OcrFramesBoxFilteredResultMeta,
}

#[derive(Clone, Debug, Serialize)]
pub struct OcrFramesBoxFilteredResultMeta {
    pub y_stats: YStats,
    pub frame_count: usize,
}

/// 单个框的调整。
fn adjust_box(box_r: &OcrBoxResult, y_stats: &YStats, x_stats: &XStats, threshold: f32) -> OcrBoxResultWithAdjust {
    if box_r.text.trim().is_empty() {
        return OcrBoxResultWithAdjust {
            base: box_r.clone(), y_center_offset_ratio: 0.0, x_center_offset_ratio: 0.0,
            height: 0.0, height_ratio: 0.0, y_penalty: 0.0, x_penalty: 0.0,
            height_penalty: 0.0, total_penalty: 0.0, is_outlier: false, adjusted_confidence: box_r.box_confidence,
        };
    }
    let top = box_r.y_range[0];
    let bottom = box_r.y_range[1];
    let height = bottom - top;
    let height_ratio = if y_stats.median_height > 0.0 { height / y_stats.median_height } else { 0.0 };
    let y_center_offset_ratio = if y_stats.median_height > 0.0 {
        let mode_center = (y_stats.mode[0] + y_stats.mode[1]) / 2.0;
        (box_r.center[1] - mode_center) / y_stats.median_height
    } else { 0.0 };
    let x_center_offset_ratio = if y_stats.median_height > 0.0 {
        (box_r.center[0] - x_stats.mode) / y_stats.median_height
    } else { 0.0 };

    const BAND_THRESHOLD: f32 = 0.05;
    const BAND_WEIGHT: f32 = 0.8;
    const HEIGHT_LOG_WEIGHT: f32 = 0.3;
    const SAT_C: f32 = 1.0;
    fn saturate(raw: f32) -> f32 { let r = raw.max(0.0); r / (r + SAT_C) }

    let y_raw = ((y_center_offset_ratio.abs() - BAND_THRESHOLD).max(0.0)) * BAND_WEIGHT;
    let x_raw = ((x_center_offset_ratio.abs() - BAND_THRESHOLD).max(0.0)) * BAND_WEIGHT;
    let h_raw = if height_ratio > 0.0 { height_ratio.log2().abs() * HEIGHT_LOG_WEIGHT } else { 1.0 };
    let y_penalty = saturate(y_raw);
    let x_penalty = saturate(x_raw);
    let height_penalty = saturate(h_raw);
    let total_penalty = saturate(y_raw + x_raw + h_raw);
    const TEXT_W: f32 = 0.3;
    const BOX_W: f32 = 0.7;
    let weighted_conf = box_r.text_confidence * TEXT_W + box_r.box_confidence * BOX_W;
    let adjusted = weighted_conf * (1.0 - total_penalty);
    let is_outlier = adjusted < threshold;

    OcrBoxResultWithAdjust {
        base: box_r.clone(), y_center_offset_ratio, x_center_offset_ratio,
        height, height_ratio, y_penalty, x_penalty, height_penalty,
        total_penalty, is_outlier, adjusted_confidence: adjusted,
    }
}

/// 对一组帧做字幕框调整。
pub fn ocr_frames_adjust_box(
    ocr_frames: &[FrameResult], y_stats: &YStats, x_stats: &XStats, args: &BoxAdjustedArgs,
) -> OcrBoxAdjustResult {
    let threshold = args.threshold();
    let frames: Vec<FrameResultBoxWithAdjust> = ocr_frames
        .iter()
        .map(|f| FrameResultBoxWithAdjust {
            text: f.text.clone(), text_confidence: f.text_confidence,
            x_range: f.x_range, y_range: f.y_range, timestamp: f.timestamp,
            boxes: f.boxes.iter().map(|b| adjust_box(b, y_stats, x_stats, threshold)).collect(),
        })
        .collect();
    OcrBoxAdjustResult {
        meta: OcrBoxAdjustResultMeta { y_stats: *y_stats, x_stats: *x_stats, frame_count: frames.len(), args: *args },
        frames,
    }
}
