//! 多帧合并流水线（base_merge_frames → substring merge → triplet noise → dedup → adjacent same text）。
//!
//! All types/functions are imported from `subtitle-ocr-types`; this module contains only
//! algorithm functions and ocr_post-specific result types.

use subtitle::SubtitleSegment;
use subtitle_ocr_types::{
    avg_confidence, edit_distance, is_substring_of, merge_confidence, normalize, overlap,
    FrameResult, MergeFramesArgs, MergeFramesResult, OcrSegment, SegmentFrame,
};

/// 合并两个段（公共逻辑）：时间取 min/max，置信度取 merge_confidence，frame_count 相加，frames 拼接。
fn merge_two_segments(
    a: &OcrSegment,
    b: &OcrSegment,
    text: String,
    y_range: Option<[f32; 2]>,
) -> OcrSegment {
    let mut frames = a.frames.clone().unwrap_or_default();
    frames.extend(b.frames.clone().unwrap_or_default());
    OcrSegment {
        base: SubtitleSegment {
            text,
            start_ms: a.base.start_ms.min(b.base.start_ms),
            end_ms: a.base.end_ms.max(b.base.end_ms),
        },
        y_range,
        text_confidence: merge_confidence(Some(a.text_confidence), Some(b.text_confidence)),
        frame_count: Some(a.frame_count.unwrap_or(1) + b.frame_count.unwrap_or(1)),
        frames: Some(frames),
    }
}

/// 断段判据（`base_merge_frames`）：同文本 run 内，**帧数越少越宽松、越多越严格**。
///
/// - **两帧**（仅一个间距、无采样节奏基线）：用 `args.max_gap_two_frames`（默认 6000）
///   绝对上限，倾向保留稀疏采样的单句。
/// - **多帧**（已建立基线）：用相对簇断裂 `gap > split_ratio × 典型间距`（系数默认 4.0）
///   或绝对硬上限 `max_gap_multi_hard`（默认 5000）断段——帧越多节奏越稳，越小的跳跃
///   也能抓到，故比两帧更严格。
///
/// 背景：上游 `ocr_frames_filter_box` 丢掉无框空帧（`box_filter.rs`），帧流里没有空帧，
/// 原先靠「夹空帧 >1500ms」断段的信号丢失，纯靠文本变化会把相隔十几秒的两句相同文本
/// 粘成一长段。上述两模式在保留断段能力的同时，避免把稀疏单句误切。
///
/// 三个阈值都物化进 [`MergeFramesArgs`]（带默认值），可被 CLI / fnrpc 调用方覆盖。

/// 对一组 u32 间距取中位数（偶数长取中间两数均值，向下取整到 u32）。
fn median_u32(v: &[u32]) -> u32 {
    if v.is_empty() {
        return 0;
    }
    let mut s: Vec<u32> = v.to_vec();
    s.sort_unstable();
    let m = s.len() / 2;
    if s.len() % 2 == 0 {
        s[m - 1].saturating_add(s[m]) / 2
    } else {
        s[m]
    }
}

/// 把逐帧 `FrameResult` 合并成带时间轴的字幕段（`base_merge_frames`）。
pub fn base_merge_frames(frames: &[FrameResult], args: &MergeFramesArgs) -> Vec<OcrSegment> {
    let mut segments: Vec<OcrSegment> = Vec::new();
    let mut current_text = String::new();
    let mut current_start: u32 = 0;
    let mut current_end: u32 = 0;
    let mut current_box_y: Option<[f32; 2]> = None;
    let mut gap_start: u32 = 0;
    let mut current_confidences: Vec<f32> = Vec::new();
    let mut current_frames: Vec<SegmentFrame> = Vec::new();
    // 当前同文本 run 的累积帧数与相邻间距序列（供多帧相对簇断裂做基线）。
    let mut current_frame_count: u32 = 0;
    let mut current_gaps: Vec<u32> = Vec::new();

    let flush = |current_text: &str,
                     current_start: u32,
                     end_ms: u32,
                     current_box_y: Option<[f32; 2]>,
                     current_confidences: &[f32],
                     current_frames: Vec<SegmentFrame>,
                     segments: &mut Vec<OcrSegment>| {
        segments.push(OcrSegment {
            base: SubtitleSegment {
                text: current_text.to_string(),
                start_ms: current_start,
                end_ms,
            },
            y_range: current_box_y,
            text_confidence: avg_confidence(current_confidences),
            frame_count: Some(current_confidences.len() as u32),
            frames: Some(current_frames),
        });
    };

    for f in frames {
        if f.text.is_empty() {
            if !current_text.is_empty() && gap_start == 0 {
                gap_start = f.timestamp as u32;
            }
            continue;
        }
        if gap_start > 0 {
            let gap_ms = (f.timestamp as u32).saturating_sub(gap_start);
            if gap_ms <= 1500
                && (normalize(&f.text) == normalize(&current_text)
                    || is_substring_of(&f.text, &current_text)
                    || is_substring_of(&current_text, &f.text))
            {
                current_confidences.push(f.text_confidence as f32);
                current_end = f.timestamp as u32;
                gap_start = 0;
                continue;
            }
            flush(
                &current_text, current_start, gap_start, current_box_y,
                &current_confidences, std::mem::take(&mut current_frames), &mut segments,
            );
            current_text.clear();
            current_start = 0;
            current_end = 0;
            current_box_y = None;
            gap_start = 0;
            current_confidences.clear();
            current_frames = Vec::new();
            current_frame_count = 0;
            current_gaps = Vec::new();
        }
        // 相对簇断段：两帧宽松 / 多帧严格（系数 K）。
        let gap = (f.timestamp as u32).saturating_sub(current_end);
        let too_far = !current_text.is_empty() && {
            let n = current_frame_count;
            if n <= 1 {
                // 仅一个间距、无采样节奏基线：用宽松绝对上限。
                gap > args.max_gap_two_frames()
            } else {
                // 已建立基线：当前间距 > K×典型间距，或超过硬上限 → 断段。
                let typical = median_u32(&current_gaps);
                let rel = (args.split_ratio() * typical as f64) as u32;
                gap > rel || gap > args.max_gap_multi_hard()
            }
        };
        if current_text.is_empty() || too_far || normalize(&f.text) != normalize(&current_text) {
            if !current_text.is_empty() {
                flush(
                    &current_text, current_start, current_end, current_box_y,
                    &current_confidences, std::mem::take(&mut current_frames), &mut segments,
                );
            }
            current_text = f.text.clone();
            current_start = f.timestamp as u32;
            current_end = f.timestamp as u32;
            current_box_y = Some(f.y_range);
            current_confidences = vec![f.text_confidence as f32];
            current_frames = vec![SegmentFrame {
                timestamp: f.timestamp as u32,
                text: f.text.clone(),
                text_confidence: f.text_confidence as f32,
            }];
            current_frame_count = 1;
            current_gaps = Vec::new();
        } else {
            current_confidences.push(f.text_confidence as f32);
            current_end = f.timestamp as u32;
            // 记录本 run 内的相邻间距（供多帧典型间距基线使用）。
            current_gaps.push(gap);
            current_frame_count += 1;
            current_frames.push(SegmentFrame {
                timestamp: f.timestamp as u32,
                text: f.text.clone(),
                text_confidence: f.text_confidence as f32,
            });
        }
    }
    if !current_text.is_empty() {
        let last_ts = if gap_start > 0 { gap_start } else { current_end };
        flush(
            &current_text, current_start, last_ts, current_box_y,
            &current_confidences, std::mem::take(&mut current_frames), &mut segments,
        );
    }
    segments
}

/// 合并相邻且互为子串、y 重叠的段（OCR 单字幻觉修复）。
pub fn merge_substring_segments(segments: &[OcrSegment]) -> Vec<OcrSegment> {
    let mut out: Vec<OcrSegment> = Vec::new();
    for cur in segments {
        if let Some(prev) = out.last_mut() {
            if overlap(prev.y_range, cur.y_range)
                && (is_substring_of(&prev.base.text, &cur.base.text)
                    || is_substring_of(&cur.base.text, &prev.base.text))
            {
                if is_substring_of(&prev.base.text, &cur.base.text) {
                    prev.base.text = cur.base.text.clone();
                    prev.y_range = cur.y_range;
                }
                prev.base.end_ms = cur.base.end_ms;
                prev.text_confidence =
                    merge_confidence(Some(prev.text_confidence), Some(cur.text_confidence));
                prev.frame_count =
                    Some(prev.frame_count.unwrap_or(1) + cur.frame_count.unwrap_or(1));
                continue;
            }
        }
        out.push(cur.clone());
    }
    out
}

/// 消除夹在两段相同真实字幕之间的短噪声段（A-B-C → A+C）。
pub fn remove_triplet_noise(segments: &[OcrSegment]) -> Vec<OcrSegment> {
    let mut out: Vec<OcrSegment> = segments.to_vec();
    let mut i = 0;
    while i + 2 < out.len() {
        let a = out[i].clone();
        let b = out[i + 1].clone();
        let c = out[i + 2].clone();

        let a_conf = a.text_confidence.clamp(0.0, 1.0);
        let b_conf = b.text_confidence.clamp(0.0, 1.0);
        let max_triplet_edit = (1.0 - a_conf) * 2.0;
        let triplet_match = edit_distance(&a.base.text, &c.base.text) as f32 <= max_triplet_edit
            && overlap(a.y_range, b.y_range)
            && overlap(b.y_range, c.y_range);
        if !triplet_match {
            i += 1;
            continue;
        }
        const HIGH_CONF: f32 = 0.8;
        if b_conf >= HIGH_CONF {
            i += 1;
            continue;
        }
        let dur_b = b.base.end_ms.saturating_sub(b.base.start_ms);
        let max_short_ms = 500.0 + (1.0 - b_conf) * 1000.0;
        let is_short = dur_b as f32 <= max_short_ms;
        let max_edit = (1.0 - b_conf) * 3.0;
        let b_near_a = edit_distance(&b.base.text, &a.base.text) as f32 <= max_edit
            && (b.base.text.chars().count() as i32 - a.base.text.chars().count() as i32).abs() <= 2;
        let b_near_c = edit_distance(&b.base.text, &c.base.text) as f32 <= max_edit
            && (b.base.text.chars().count() as i32 - c.base.text.chars().count() as i32).abs() <= 2;
        let is_noise = is_short || b_near_a || b_near_c;
        if !is_noise {
            i += 1;
            continue;
        }
        let merged_conf = avg_confidence(&[a.text_confidence, b.text_confidence, c.text_confidence]);
        let fc = a.frame_count.unwrap_or(1) + b.frame_count.unwrap_or(1) + c.frame_count.unwrap_or(1);
        let mut frames = a.frames.unwrap_or_default();
        frames.extend(b.frames.unwrap_or_default());
        frames.extend(c.frames.unwrap_or_default());
        out[i] = OcrSegment {
            base: SubtitleSegment { text: a.base.text, start_ms: a.base.start_ms, end_ms: c.base.end_ms },
            y_range: a.y_range,
            text_confidence: merged_conf,
            frame_count: Some(fc),
            frames: Some(frames),
        };
        out.drain(i + 1..=i + 2);
        if i > 0 { i -= 1; }
    }
    out
}

/// 去重 / 重叠合并（时间重叠/相接、文本近邻的段合并）。
pub fn dedup_overlap(segments: &[OcrSegment], dedup_edit_distance: u32) -> Vec<OcrSegment> {
    const TOUCH_GAP_MS: u32 = 500;
    let mut out: Vec<OcrSegment> = Vec::new();
    for cur in segments {
        if let Some(prev) = out.last_mut() {
            let gap = prev.base.start_ms.max(cur.base.start_ms)
                .saturating_sub(prev.base.end_ms.min(cur.base.end_ms));
            let overlaps = prev.base.start_ms < cur.base.end_ms && cur.base.start_ms < prev.base.end_ms;
            let touching = gap <= TOUCH_GAP_MS;
            let both_short = prev.base.text.chars().count() <= 2 && cur.base.text.chars().count() <= 2;
            let not_same_word = prev.base.text != cur.base.text
                && !is_substring_of(&prev.base.text, &cur.base.text);
            if (overlaps || touching)
                && edit_distance(&prev.base.text, &cur.base.text) <= dedup_edit_distance
                && !(both_short && not_same_word)
            {
                let text = if prev.base.text.chars().count() >= cur.base.text.chars().count() {
                    prev.base.text.clone()
                } else {
                    cur.base.text.clone()
                };
                *prev = merge_two_segments(prev, cur, text, prev.y_range);
                continue;
            }
        }
        out.push(cur.clone());
    }
    out
}

/// 合并归一化后文本相同、间隔 ≤ 2s 且不重叠的相邻段。
pub fn merge_adjacent_same_text(segments: &[OcrSegment]) -> Vec<OcrSegment> {
    const MAX_GAP_MS: u32 = 2000;
    let mut out: Vec<OcrSegment> = segments.to_vec();
    for i in (1..out.len()).rev() {
        let prev_norm = normalize(&out[i - 1].base.text);
        let cur_norm = normalize(&out[i].base.text);
        if prev_norm != cur_norm { continue; }
        let gap = out[i].base.start_ms.saturating_sub(out[i - 1].base.end_ms);
        if gap > MAX_GAP_MS { continue; }
        out[i - 1].base.end_ms = out[i].base.end_ms;
        out[i - 1].text_confidence =
            avg_confidence(&[out[i - 1].text_confidence, out[i].text_confidence]);
        out[i - 1].frame_count =
            Some(out[i - 1].frame_count.unwrap_or(1) + out[i].frame_count.unwrap_or(1));
        out.remove(i);
    }
    out
}

/// 完整合并流水线：base → substring → triplet → dedup → adjacent same text。
pub fn merge_frames(frames: &[FrameResult], args: &MergeFramesArgs) -> MergeFramesResult {
    let mut segments = base_merge_frames(frames, args);
    if args.is_merge_substring() {
        segments = merge_substring_segments(&segments);
    }
    segments = remove_triplet_noise(&segments);
    segments = dedup_overlap(&segments, args.dedup_edit_distance());
    segments = merge_adjacent_same_text(&segments);
    let text = segments.iter().map(|s| s.base.text.as_str()).collect::<Vec<_>>().join(" ");
    MergeFramesResult { text, segments }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(text: &str, ts: u64) -> FrameResult {
        FrameResult {
            text: text.to_string(),
            text_confidence: 0.9,
            boxes: vec![],
            x_range: [0.0, 0.0],
            y_range: [0.0, 0.0],
            timestamp: ts,
        }
    }

    #[test]
    fn base_merge_groups_same_text() {
        let frames = vec![make_frame("hello", 100), make_frame("hello", 200), make_frame("world", 300)];
        let segs = base_merge_frames(&frames, &MergeFramesArgs::default());
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].base.text, "hello");
        assert_eq!(segs[0].base.start_ms, 100);
        assert_eq!(segs[0].base.end_ms, 200);
        assert_eq!(segs[1].base.text, "world");
    }

    #[test]
    fn base_merge_splits_same_text_far_apart() {
        // 上游 filter 丢掉了空帧，只剩 4 帧「啊」：前两句是一句，后两句是另一句（相隔 12.6s）。
        let frames = vec![
            make_frame("啊", 3200),
            make_frame("啊", 4366),
            make_frame("啊", 17033),
            make_frame("啊", 18266),
        ];
        let segs = base_merge_frames(&frames, &MergeFramesArgs::default());
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].base.start_ms, 3200);
        assert_eq!(segs[0].base.end_ms, 4366);
        assert_eq!(segs[0].frame_count, Some(2));
        assert_eq!(segs[1].base.start_ms, 17033);
        assert_eq!(segs[1].base.end_ms, 18266);
    }

    #[test]
    fn base_merge_keeps_same_text_within_gap() {
        // 同一句字幕因丢帧只留两帧，间隔 3266ms（实测最大值），不能被误切。
        let frames = vec![make_frame("我徒弟哈哈亲生的", 36100), make_frame("我徒弟哈哈亲生的", 39366)];
        let segs = base_merge_frames(&frames, &MergeFramesArgs::default());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].base.start_ms, 36100);
        assert_eq!(segs[0].base.end_ms, 39366);
    }

    #[test]
    fn base_merge_keeps_same_text_within_two_frame() {
        // 两帧宽松上限 6000：相隔 4500ms 的同文本两帧视为同一句（稀疏采样），不切。
        // 复现「握住它…龙巢中昏睡」9.3s 与 13.8s 的 case。
        let frames = vec![
            make_frame("握住它你我的躯体尚在龙巢中昏睡", 9366),
            make_frame("握住它你我的躯体尚在龙巢中昏睡", 13866),
        ];
        let segs = base_merge_frames(&frames, &MergeFramesArgs::default());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].base.start_ms, 9366);
        assert_eq!(segs[0].base.end_ms, 13866);
    }

    #[test]
    fn base_merge_splits_same_text_beyond_two_frame() {
        // 两帧模式：间距超过放宽上限 6000ms 才断。
        let frames = vec![make_frame("啊", 1000), make_frame("啊", 7500)];
        let segs = base_merge_frames(&frames, &MergeFramesArgs::default());
        assert_eq!(segs.len(), 2);
    }

    #[test]
    fn base_merge_splits_multi_frame_relative() {
        // 多帧模式：已建立节奏基线（典型间距 1000ms），当前间距 5000ms 远超
        // 系数 K=4 的阈值（4×1000=4000），即使未到硬上限 5000 也断成两段。
        let frames = vec![
            make_frame("啊", 1000),
            make_frame("啊", 2000),
            make_frame("啊", 7000),
        ];
        let segs = base_merge_frames(&frames, &MergeFramesArgs::default());
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].base.start_ms, 1000);
        assert_eq!(segs[0].base.end_ms, 2000);
        assert_eq!(segs[1].base.start_ms, 7000);
    }

    #[test]
    fn base_merge_args_override_two_frame() {
        // CLI/fnrpc 可覆盖：把两帧上限压到 4000，原本保留的 4500ms 间距被断开。
        let args = MergeFramesArgs {
            max_gap_two_frames: Some(4000),
            ..MergeFramesArgs::default()
        };
        let frames = vec![
            make_frame("握住它你我的躯体尚在龙巢中昏睡", 9366),
            make_frame("握住它你我的躯体尚在龙巢中昏睡", 13866),
        ];
        let segs = base_merge_frames(&frames, &args);
        assert_eq!(segs.len(), 2);
    }

    #[test]
    fn dedup_merge_overlapping_near_text() {
        let segs = vec![
            OcrSegment { base: SubtitleSegment { text: "hello".into(), start_ms: 0, end_ms: 1000 }, y_range: None, text_confidence: 0.9, frame_count: Some(1), frames: None },
            OcrSegment { base: SubtitleSegment { text: "helo".into(), start_ms: 900, end_ms: 1100 }, y_range: None, text_confidence: 0.8, frame_count: Some(1), frames: None },
        ];
        let out = dedup_overlap(&segs, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].base.text, "hello");
    }
}
