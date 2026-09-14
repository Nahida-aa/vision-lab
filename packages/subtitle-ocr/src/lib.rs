//! # subtitle-ocr
//!
//! 字幕 OCR 专用层，构建在 [`rapidocr_ort::OcrEngine`]（PP-OCR det/rec/cls）之上。
//!
//! 类型与算法已拆分为独立 crate（无 ort 依赖）：
//! - [`ocr_types`] — 原子检测框 `OcrBoxResult`
//! - [`subtitle_ocr_types`] — 字幕 OCR 产物/管线类型（FrameResult/OcrSegment/OcrFrames*/MergeFrames*）+ 工具函数
//! - [`subtitle_ocr_post`] — OCR 后处理算法（merge/filter/adjust）
//!
//! 本包仅保留引擎封装（`SubtitleOcr`）与批量入口（`ocr_entries`）。

use anyhow::Result;
use ndarray::{Array3, s};
use rapidocr_ort::{ModelProfile, OcrEngine};
use std::path::PathBuf;

// ── 内部模块 ──
pub(crate) mod geometry;
pub mod util;

// ── 委托到新 crate ──
/// 类型与纯工具函数（零重型依赖）。
pub use ocr_types;
/// OCR 后处理算法（无 ort/opencv）。
pub use subtitle_ocr_post;
/// 向后兼容：CLI 二进制用 `crate::ocr_post::*` 路径。
pub use subtitle_ocr_post as ocr_post;

// ── 批量导出（对齐旧 API 路径） ──
// 字幕 OCR 产物/管线类型（含 SubtitleSegment 透出）+ 工具函数来自 subtitle-ocr-types；
// 原子检测框 OcrBoxResult 仍定义在 ocr-types。
pub use subtitle_ocr_types::*;
pub use ocr_types::{OcrBoxResult};
pub use subtitle_ocr_post::{
    BoxAdjustedArgs, FrameResultBoxWithAdjust, OcrBoxAdjustResult, OcrBoxAdjustResultMeta,
    OcrBoxResultWithAdjust, OcrFramesBoxFilteredResult, OcrFramesBoxFilteredResultMeta,
    OcrSegmentAdjustArgs, OcrSegmentFilterData, OcrSegmentFilterMeta, OcrSegmentFilterResult,
    OcrSegmentWithAdjust, base_merge_frames, dedup_overlap, merge_adjacent_same_text, merge_frames,
    merge_substring_segments, ocr_frames_adjust_box, ocr_frames_filter_box, ocr_segment_adjust,
    ocr_segment_filter, ocr_segment_filter_with_meta, remove_triplet_noise,
};
pub use geometry::nms;

// ==========================================================
// 引擎封装（本包特有，依赖 rapidocr-ort）
// ==========================================================

/// 字幕 OCR 的行为开关（对齐 cpp 的 CLI 参数）。
#[derive(Clone, Debug)]
pub struct OcrOptions {
    pub bottom_only: bool,
    pub subtitle_only: bool,
    pub use_nms: bool,
    pub text_confidence_threshold: f32,
    pub use_warp_crop: bool,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self {
            bottom_only: true,
            subtitle_only: false,
            use_nms: true,
            text_confidence_threshold: 0.5,
            use_warp_crop: false,
        }
    }
}

/// 字幕 OCR 引擎：持有 [`OcrEngine`] 与行为选项。
pub struct SubtitleOcr {
    engine: OcrEngine,
    opts: OcrOptions,
}

fn offset_box_y(b: &mut ocr_types::OcrBoxResult, dy: f32) {
    for p in &mut b.bbox {
        p[1] += dy;
    }
    b.center[1] += dy;
    b.y_range[0] += dy;
    b.y_range[1] += dy;
}

impl SubtitleOcr {
    pub fn from_profile(
        profile: ModelProfile,
        model_dir: &std::path::Path,
        opts: OcrOptions,
    ) -> Result<Self> {
        let engine =
            OcrEngine::from_profile(profile, model_dir)?.with_warp_crop(opts.use_warp_crop);
        Ok(Self { engine, opts })
    }

    pub fn ocr_image(&mut self, rgb: &Array3<u8>) -> Result<Vec<ocr_types::OcrBoxResult>> {
        let (h, _, _) = rgb.dim();
        let h = h as i64;
        let y_offset = if self.opts.bottom_only {
            ((h as f32) * 0.6) as i64
        } else {
            0
        };
        let roi: Array3<u8> = if y_offset > 0 {
            rgb.slice(s![y_offset as usize.., .., ..]).to_owned()
        } else {
            rgb.clone()
        };
        let results: Vec<ocr_types::OcrBoxResult> = self.engine.detect(&roi)?;
        let mut boxes: Vec<ocr_types::OcrBoxResult> = results
            .into_iter()
            .map(|mut r| {
                if y_offset > 0 {
                    offset_box_y(&mut r, y_offset as f32);
                }
                r.text = r.text.trim().to_string();
                r
            })
            .filter(|r| {
                if self.opts.subtitle_only {
                    let ratio = r.center[1] / (h as f32);
                    if !(0.85..=0.99).contains(&ratio) {
                        return false;
                    }
                }
                !r.text.is_empty() && r.text_confidence >= self.opts.text_confidence_threshold
            })
            .collect();
        if self.opts.use_nms && boxes.len() > 1 {
            boxes = geometry::nms(boxes);
        }
        boxes.sort_by(|a, b| {
            let ya = a.center[1];
            let yb = b.center[1];
            if (ya - yb).abs() > 20.0 {
                ya.partial_cmp(&yb).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                let xa = a.bbox[0][0];
                let xb = b.bbox[0][0];
                xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal)
            }
        });
        Ok(boxes)
    }
}

// ===========================================================================
// 批量入口
// ===========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameTimes {
    None,
    Single(u64),
    Range(u64, u64),
}

impl FrameTimes {
    pub fn sort_key(&self) -> u64 {
        match self {
            FrameTimes::None => 0,
            FrameTimes::Single(t) => *t,
            FrameTimes::Range(s, _) => *s,
        }
    }
}

pub struct OcrEntry {
    pub path: PathBuf,
    pub times: FrameTimes,
}

pub fn ocr_entry(ocr: &mut SubtitleOcr, entry: &OcrEntry) -> Result<Vec<subtitle_ocr_types::FrameResult>> {
    let rgb = rapidocr_ort::load_image(&entry.path)?;
    let boxes = ocr.ocr_image(&rgb)?;
    let aggregated = subtitle_ocr_types::aggregate_boxes(&boxes);
    let out = match entry.times {
        FrameTimes::None => vec![aggregated],
        FrameTimes::Single(t) => vec![subtitle_ocr_types::FrameResult {
            timestamp: t,
            ..aggregated
        }],
        FrameTimes::Range(s, end) => vec![
            subtitle_ocr_types::FrameResult { timestamp: s, ..aggregated.clone() },
            subtitle_ocr_types::FrameResult { timestamp: end, ..aggregated },
        ],
    };
    Ok(out)
}

pub fn ocr_entries(ocr: &mut SubtitleOcr, entries: &[OcrEntry]) -> Result<Vec<subtitle_ocr_types::FrameResult>> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        out.extend(ocr_entry(ocr, e)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_with_ys(corners_y: [f32; 4]) -> ocr_types::OcrBoxResult {
        ocr_types::OcrBoxResult {
            text: "a".into(),
            text_confidence: 0.9,
            box_confidence: 0.9,
            bbox: [
                [0.0, corners_y[0]],
                [10.0, corners_y[1]],
                [10.0, corners_y[2]],
                [0.0, corners_y[3]],
            ],
            x_range: [0.0, 10.0],
            y_range: [corners_y[0], corners_y[2]],
            center: [5.0, (corners_y[0] + corners_y[2]) / 2.0],
        }
    }

    #[test]
    fn nms_deduplicates_heavily_overlapping_boxes() {
        let boxes = vec![
            box_with_ys([100.0, 100.0, 130.0, 130.0]),
            box_with_ys([101.0, 101.0, 131.0, 131.0]),
            box_with_ys([200.0, 200.0, 230.0, 230.0]),
        ];
        let result = geometry::nms(boxes);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn frame_times_sort_key() {
        assert_eq!(FrameTimes::None.sort_key(), 0);
        assert_eq!(FrameTimes::Single(500).sort_key(), 500);
        assert_eq!(FrameTimes::Range(100, 200).sort_key(), 100);
    }
}
