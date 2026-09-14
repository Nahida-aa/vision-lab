//! 字幕框离群过滤：逐帧剔除离群框、重聚合得到干净帧。

use crate::box_adjust::{
    FrameResultBoxWithAdjust, OcrBoxResultWithAdjust, OcrFramesBoxFilteredResult,
    OcrFramesBoxFilteredResultMeta,
};
use subtitle_ocr_types::{aggregate_boxes, compute_box_y_stats, FrameResult};

/// 过滤离群框：逐帧剔除 `is_outlier` 的框后，重新聚合得到干净帧。
pub fn ocr_frames_filter_box(frames: &[FrameResultBoxWithAdjust]) -> OcrFramesBoxFilteredResult {
    let frames: Vec<FrameResult> = frames
        .iter()
        .flat_map(|f| {
            let clean_boxes: Vec<&OcrBoxResultWithAdjust> = f.boxes.iter().filter(|b| !b.is_outlier).collect();
            if clean_boxes.is_empty() { return Vec::new(); }
            if clean_boxes.len() == f.boxes.len() { return vec![f.clone().into()]; }
            let mut rebuilt_ocr = aggregate_boxes(
                &clean_boxes.iter().map(|b| b.base.clone()).collect::<Vec<_>>(),
            );
            rebuilt_ocr.timestamp = f.timestamp;
            vec![rebuilt_ocr]
        })
        .collect();
    OcrFramesBoxFilteredResult {
        meta: OcrFramesBoxFilteredResultMeta { y_stats: compute_box_y_stats(&frames), frame_count: frames.len() },
        frames,
    }
}
