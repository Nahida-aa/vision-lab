//! OCR post-processing algorithms (merge, filter, adjust).
//!
//! This crate depends only on `ocr-types` + `serde` — **no ort, opencv, or ffmpeg**.
//! It provides the post-OCR pipeline functions consumed by both `subtitle-ocr` CLI
//! and downstream crates like LocalDub's `ld-core`.

pub mod box_adjust;
pub mod box_filter;
pub mod merge_frames;
pub mod segment_adjust;
pub mod segment_filter;

// Re-export key types for convenience.
pub use box_adjust::{
    BoxAdjustedArgs, FrameResultBoxWithAdjust, OcrBoxAdjustResult, OcrBoxAdjustResultMeta,
    OcrBoxResultWithAdjust, OcrFramesBoxFilteredResult, OcrFramesBoxFilteredResultMeta,
    ocr_frames_adjust_box,
};
pub use box_filter::ocr_frames_filter_box;
pub use merge_frames::{
    base_merge_frames, dedup_overlap, merge_adjacent_same_text, merge_frames,
    merge_substring_segments, remove_triplet_noise,
};
// 原子检测框 OcrBoxResult 仍定义在 ocr-types；其余字幕/OCR 产物与管线类型 +
// 工具函数（含 SubtitleSegment 透出）统一来自 subtitle-ocr-types。
pub use ocr_types::{OcrBoxResult};
pub use subtitle_ocr_types::{
    FrameResult, MergeFramesArgs, MergeFramesResult, OcrSegment, SegmentFrame, SubtitleSegment,
    XStats, YStats, aggregate_boxes, avg_confidence, compute_box_x_stats, compute_box_y_stats,
    edit_distance, is_substring_of, merge_confidence, normalize, overlap,
};
pub use segment_adjust::{
    OcrSegmentAdjustArgs, OcrSegmentWithAdjust, ocr_segment_adjust,
};
pub use segment_filter::{
    OcrSegmentFilterData, OcrSegmentFilterMeta, OcrSegmentFilterResult, ocr_segment_filter,
    ocr_segment_filter_with_meta,
};
