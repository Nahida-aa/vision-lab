//! 字幕 OCR 领域的「产物 / 管线」类型与纯函数（与识别引擎强相关，但无 ort/opencv 重依赖）。
//!
//! 本 crate 承载 `subtitle-ocr` 引擎产出、以及 `subtitle-ocr-post` 后处理消费的
//! 类型与工具：逐帧结果 [`FrameResult`]、字幕段 [`OcrSegment`]、引擎输出
//! [`OcrFramesResult`] / [`OcrFramesMeta`]、合并参数/结果 [`MergeFramesArgs`] /
//! [`MergeFramesResult`]、框/帧统计 [`YStats`] / [`XStats`]，以及归一化 / 编辑距离
//! 等纯函数。
//!
//! 依赖关系：`subtitle`（纯字幕领域 [`SubtitleSegment`]）← `ocr-types`（原子检测框
//! [`OcrBoxResult`]）← 本 crate。[`OcrBoxResult`] 仍定义在 `ocr-types`，本 crate 通过
//! `#[serde(flatten)]` 内嵌 [`SubtitleSegment`] 构成 [`OcrSegment`]。
//!
//! 分层：`subtitle` → `ocr-types` → `subtitle-ocr-types` → `subtitle-ocr-post` / `subtitle-ocr`。

use serde::{Deserialize, Serialize};

// 纯字幕领域类型（文本 + 时间跨度）由 `subtitle` crate 提供，本 crate 透出以便
// 下游统一从 `subtitle_ocr_types` 取全部字幕 / OCR 类型。
pub use subtitle::SubtitleSegment;
// 原子检测框定义在 ocr-types，本 crate 内部（FrameResult / 统计 / aggregate_boxes）用到。
use ocr_types::OcrBoxResult;

// ==========================================================
// Frame / segment types
// ==========================================================

/// 单图聚合结果（可携带单时刻时间戳）。
///
/// 把一张图里识别出的多框聚合成一条文本 + 值域 + 明细。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameResult {
    /// 该图识别文本（多行按出现顺序拼接，用空格分隔）。
    pub text: String,
    /// 该图聚合置信度：各框 `text_confidence` 的均值。
    pub text_confidence: f64,
    /// 该图所有识别区域明细（每行文本/框/score，含坐标还原）。
    pub boxes: Vec<OcrBoxResult>,
    /// 横向值域 `[min_x, max_x]`（像素坐标），无字幕时为 `[0,0]`。
    pub x_range: [f32; 2],
    /// 纵向值域 `[min_y, max_y]`（像素坐标），无字幕时为 `[0,0]`。
    pub y_range: [f32; 2],
    /// 该图对应时刻（毫秒）。`0` 表示无时间。
    pub timestamp: u64,
}

/// 一条字幕段（extends SubtitleSegment with OCR-specific fields）。
///
/// TS 用 `extends SubtitleSegment`；Rust 用 `#[serde(flatten)]` 内嵌。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "specta-types", derive(specta::Type))]
pub struct OcrSegment {
    #[serde(flatten)]
    pub base: SubtitleSegment,
    /// 字幕带纵向值域 `[min_y, max_y]`（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_range: Option<[f32; 2]>,
    /// 字幕文本置信度（必填）。
    pub text_confidence: f32,
    /// 该段聚合的帧数（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_count: Option<u32>,
    /// 组成该段的各帧明细（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<Vec<SegmentFrame>>,
}

/// 组成字幕段的单个帧明细。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "specta-types", derive(specta::Type))]
pub struct SegmentFrame {
    /// 帧文本。
    pub text: String,
    /// 帧时刻（毫秒）。
    pub timestamp: u32,
    /// 文本置信度。
    pub text_confidence: f32,
}

// ==========================================================
// Merge / pipeline args + results
// ==========================================================

/// 多帧合并参数。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MergeFramesArgs {
    /// 是否合并互为子串的相邻文本。默认 `false`。
    #[serde(default = "default_is_merge_substring")]
    pub is_merge_substring: Option<bool>,
    /// dedupOverlap 的编辑距离阈值：edit_distance ≤ 此值则合并。默认 `1`。
    #[serde(default = "default_dedup_edit_distance")]
    pub dedup_edit_distance: Option<u32>,
}

fn default_is_merge_substring() -> Option<bool> {
    Some(false)
}
fn default_dedup_edit_distance() -> Option<u32> {
    Some(1)
}

impl MergeFramesArgs {
    pub fn is_merge_substring(&self) -> bool {
        self.is_merge_substring.unwrap_or(false)
    }
    pub fn dedup_edit_distance(&self) -> u32 {
        self.dedup_edit_distance.unwrap_or(1)
    }
}

/// 多帧合并输出。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeFramesResult {
    /// 全文：各段 `text` 以空格拼接。
    pub text: String,
    /// 合并后的字幕段列表。
    pub segments: Vec<OcrSegment>,
}

// ==========================================================
// Pipeline output types
// ==========================================================

/// OCR 运行设备。
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrDevice {
    Cpu,
    Cuda,
    Directml,
    Coreml,
    Rocm,
    Mps,
}

/// `ocr_frames.json` 的元数据（溯源 / 生成参数）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrFramesMeta {
    /// OCR 引擎名称，如 `ort-cpp` / `ort-rust`。
    pub engine: String,
    /// OCR 运行设备。
    pub device: OcrDevice,
    /// 画面像素高度（`y_range` 所在的图像坐标系高度，通常即视频帧高）。
    ///
    /// 下游 `ocr-segment-adjust` 的 Y 偏移惩罚用它当归一化分母
    /// （见 `compute_y_penalty`）。由识别侧（读图时）填入，本 crate 不读视频。
    ///
    /// 命名沿用 `video_height`：分母的本意是「画面高度」，而抽帧通常为原始尺寸，
    /// 二者相等；若抽帧有缩放，这里存图像高度才是正确的（与 `y_range` 同坐标系）。
    /// 老 JSON（含 cpp 侧产出）没有此字段 → `None`，调用方需回退到显式传值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_height: Option<u32>,
}

/// 一次 stage 的原始 OCR 帧输出（`asr_ocr_frames.json | sf_ocr_frames.json` 等）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrFramesResult {
    pub frames: Vec<FrameResult>,
    pub meta: OcrFramesMeta,
}

// ==========================================================
// Stats types + compute functions
// ==========================================================

/// 字幕框纵向统计结果。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct YStats {
    pub avg: [f32; 2],
    pub mode: [f32; 2],
    pub median: [f32; 2],
    pub avg_height: f32,
    pub median_height: f32,
    pub mode_height: f32,
}

/// 字幕框横向（x 中心）统计结果。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct XStats {
    pub avg: f32,
    pub mode: f32,
    pub median: f32,
}

/// 对升序切片取中位数（偶数长取中间两数均值）。
pub fn median_of(arr: &[f32]) -> f32 {
    let m = arr.len() / 2;
    if arr.len() % 2 == 0 {
        (arr[m - 1] + arr[m]) / 2.0
    } else {
        arr[m]
    }
}

/// 对一组帧统计字幕框的纵向分布（位置 + 高度）。
pub fn compute_box_y_stats(frames: &[FrameResult]) -> YStats {
    let boxes: Vec<&OcrBoxResult> = frames
        .iter()
        .flat_map(|f| f.boxes.iter())
        .filter(|l| !l.text.trim().is_empty())
        .collect();
    if boxes.is_empty() {
        return YStats::default();
    }
    let n = boxes.len();
    let box_ys: Vec<[f32; 2]> = boxes.iter().map(|l| l.y_range).collect();
    let sum_top: f32 = box_ys.iter().map(|[t, _]| *t).sum();
    let sum_btm: f32 = box_ys.iter().map(|[_, b]| *b).sum();
    let avg = [sum_top / n as f32, sum_btm / n as f32];
    let sum_h: f32 = boxes.iter().map(|l| l.y_range[1] - l.y_range[0]).sum();
    let avg_height = sum_h / n as f32;

    let mut heights: Vec<f32> = boxes.iter().map(|l| l.y_range[1] - l.y_range[0]).collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_height = median_of(&heights);

    let mut tops: Vec<f32> = box_ys.iter().map(|[t, _]| *t).collect();
    let mut btms: Vec<f32> = box_ys.iter().map(|[_, b]| *b).collect();
    tops.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    btms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = [median_of(&tops), median_of(&btms)];

    let mut height_counts: std::collections::HashMap<i32, u32> = std::collections::HashMap::new();
    let mut mode_height_count = 0u32;
    let mut mode_height = heights[0];
    for &h in &heights {
        let key = h.round() as i32;
        let c = height_counts.entry(key).or_insert(0);
        *c += 1;
        if *c > mode_height_count {
            mode_height_count = *c;
            mode_height = h;
        }
    }

    let mut counts: std::collections::HashMap<(i32, i32), u32> = std::collections::HashMap::new();
    let mut max_count = 0u32;
    let mut mode = box_ys[0];
    for &[t, b] in &box_ys {
        let key = (t.round() as i32, b.round() as i32);
        let c = counts.entry(key).or_insert(0);
        *c += 1;
        if *c > max_count {
            max_count = *c;
            mode = [t, b];
        }
    }

    YStats { avg, mode, median, avg_height, median_height, mode_height }
}

/// 对一组帧统计字幕框的横向（x 中心）分布。
pub fn compute_box_x_stats(frames: &[FrameResult]) -> XStats {
    let centers: Vec<f32> = frames
        .iter()
        .flat_map(|f| f.boxes.iter())
        .filter(|l| !l.text.trim().is_empty())
        .map(|l| l.center[0])
        .collect();
    if centers.is_empty() {
        return XStats::default();
    }
    let n = centers.len() as f32;
    let avg = centers.iter().sum::<f32>() / n;
    let mut sorted = centers.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = median_of(&sorted);
    let mut counts: std::collections::HashMap<i32, u32> = std::collections::HashMap::new();
    let mut max_count = 0u32;
    let mut mode = sorted[0];
    for &c in &centers {
        let key = c.round() as i32;
        let cnt = counts.entry(key).or_insert(0);
        *cnt += 1;
        if *cnt > max_count {
            max_count = *cnt;
            mode = c;
        }
    }
    XStats { avg, mode, median }
}

// ==========================================================
// Pure utility functions
// ==========================================================

/// 归一化文本：去掉所有空白字符。
pub fn normalize(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 平均置信度：输入为空时返回 `0.0`。
pub fn avg_confidence(confidences: &[f32]) -> f32 {
    if confidences.is_empty() {
        0.0
    } else {
        confidences.iter().sum::<f32>() / confidences.len() as f32
    }
}

/// 判断 `a` 是否为 `b` 的子串（双向，较短被较长包含即算；等长不算）。
pub fn is_substring_of(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() || a.len() == b.len() {
        return false;
    }
    if a.len() < b.len() {
        b.contains(a)
    } else {
        a.contains(b)
    }
}

/// 合并两个（可选）置信度为均值；任一为 `None` 时返回另一个。
pub fn merge_confidence(a: Option<f32>, b: Option<f32>) -> f32 {
    match (a, b) {
        (Some(x), Some(y)) => (x + y) / 2.0,
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => 0.0,
    }
}

/// 编辑距离（Levenshtein）：按**字符**遍历（对齐 TS 的 `a.length`）。
pub fn edit_distance(a: &str, b: &str) -> u32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    let mut prev: Vec<u32> = (0..=n as u32).collect();
    let mut cur: Vec<u32> = vec![0; n + 1];
    for i in 1..=m {
        cur[0] = i as u32;
        for j in 1..=n {
            cur[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1]
            } else {
                prev[j].min(cur[j - 1]).min(prev[j - 1]) + 1
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

/// 两个值域 `[min, max]` 是否重叠。
pub fn overlap(a: Option<[f32; 2]>, b: Option<[f32; 2]>) -> bool {
    match (a, b) {
        (Some([a0, a1]), Some([b0, b1])) => a0 < b1 && b0 < a1,
        _ => false,
    }
}

/// 聚合多框为单个 [`FrameResult`]：拼接文本、均值置信度、合并值域。
pub fn aggregate_boxes(boxes: &[OcrBoxResult]) -> FrameResult {
    let text = boxes
        .iter()
        .filter(|b| !b.text.trim().is_empty())
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let confidences: Vec<f32> = boxes.iter().map(|b| b.text_confidence).collect();
    let text_confidence = avg_confidence(&confidences) as f64;
    let x_range = if boxes.is_empty() {
        [0.0, 0.0]
    } else {
        let min_x = boxes.iter().map(|b| b.x_range[0]).fold(f32::INFINITY, f32::min);
        let max_x = boxes.iter().map(|b| b.x_range[1]).fold(f32::NEG_INFINITY, f32::max);
        [min_x, max_x]
    };
    let y_range = if boxes.is_empty() {
        [0.0, 0.0]
    } else {
        let min_y = boxes.iter().map(|b| b.y_range[0]).fold(f32::INFINITY, f32::min);
        let max_y = boxes.iter().map(|b| b.y_range[1]).fold(f32::NEG_INFINITY, f32::max);
        [min_y, max_y]
    };
    FrameResult { text, text_confidence, boxes: boxes.to_vec(), x_range, y_range, timestamp: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_basic() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "ab"), 1);
        assert_eq!(edit_distance("陆", "陆执巡"), 2);
    }

    #[test]
    fn normalize_strips_whitespace() {
        assert_eq!(normalize("a b\nc"), "abc");
        assert_eq!(normalize("  "), "");
    }

    #[test]
    fn is_substring_of_works() {
        assert!(!is_substring_of("a", "a"));
        assert!(is_substring_of("ab", "abc"));
        assert!(is_substring_of("bc", "abc"));
        assert!(!is_substring_of("ac", "abc"));
    }

    #[test]
    fn merge_confidence_cases() {
        assert_eq!(merge_confidence(None, None), 0.0);
        assert_eq!(merge_confidence(Some(0.8), None), 0.8);
        assert!((merge_confidence(Some(0.6), Some(0.8)) - 0.7).abs() < 1e-6);
    }
}
