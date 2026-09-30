//! OCR 纯函数工具集（无模型依赖，可单测）。
//!
//! 存放不依赖引擎、可独立测试的感知后处理函数。目前包含：
//! - [`nms`]：重叠框去重（复刻 cpp runOcr 的 IoU 过滤）。

use ocr_types::OcrBoxResult;

/// 按面积降序，剔除被已保留大框覆盖超过 70% 的小框（IoU 口径）。
pub fn nms(boxes: Vec<OcrBoxResult>) -> Vec<OcrBoxResult> {
    // 计算外接框 rect
    struct B {
        idx: usize,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        area: f32,
    }
    let mut rects: Vec<B> = boxes
        .iter()
        .enumerate()
        .map(|(idx, l)| {
            let (mut x0, mut y0, mut x1, mut y1) = (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            );
            for p in &l.bbox {
                x0 = x0.min(p[0]);
                x1 = x1.max(p[0]);
                y0 = y0.min(p[1]);
                y1 = y1.max(p[1]);
            }
            let area = (x1 - x0).max(1.0) * (y1 - y0).max(1.0);
            B {
                idx,
                x0,
                y0,
                x1,
                y1,
                area,
            }
        })
        .collect();
    // 面积大的优先保留。
    rects.sort_by(|a, b| {
        b.area
            .partial_cmp(&a.area)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut keep = vec![true; boxes.len()];
    for i in 0..rects.len() {
        if !keep[rects[i].idx] {
            continue;
        }
        let a = &rects[i];
        for j in (i + 1)..rects.len() {
            if !keep[rects[j].idx] {
                continue;
            }
            let b = &rects[j];
            let ix0 = a.x0.max(b.x0);
            let iy0 = a.y0.max(b.y0);
            let ix1 = a.x1.min(b.x1);
            let iy1 = a.y1.min(b.y1);
            if ix1 <= ix0 || iy1 <= iy0 {
                continue;
            }
            let inter = (ix1 - ix0) * (iy1 - iy0);
            // 小框 b 被大框 a 覆盖比例。
            if inter / b.area > 0.7 {
                keep[rects[j].idx] = false;
            }
        }
    }
    let mut out: Vec<OcrBoxResult> = keep
        .iter()
        .zip(boxes)
        .filter(|(k, _)| **k)
        .map(|(_, l)| l)
        .collect();
    // 维持原顺序（按 y 排序在 ocr_image 末尾统一做；此处仅 NMS 过滤，先按输入序）。
    out.sort_by_key(|l| (l.center[1] * 1000.0) as i64);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nms_removes_contained_small_box() {
        // 大框包含小框（覆盖 >70%）→ 小框被剔除。
        let big = OcrBoxResult {
            text: "A".into(),
            text_confidence: 0.9,
            bbox: [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
            box_confidence: 0.9,
            x_range: [0.0, 100.0],
            y_range: [0.0, 100.0],
            center: [50.0, 50.0],
        };
        let small = OcrBoxResult {
            text: "B".into(),
            text_confidence: 0.8,
            bbox: [[10.0, 10.0], [20.0, 10.0], [20.0, 20.0], [10.0, 20.0]],
            box_confidence: 0.8,
            x_range: [10.0, 20.0],
            y_range: [10.0, 20.0],
            center: [15.0, 15.0],
        };
        let out = nms(vec![big.clone(), small]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "A");
    }

    #[test]
    fn nms_keeps_disjoint_boxes() {
        let a = OcrBoxResult {
            text: "A".into(),
            text_confidence: 0.9,
            bbox: [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
            box_confidence: 0.9,
            x_range: [0.0, 10.0],
            y_range: [0.0, 10.0],
            center: [5.0, 5.0],
        };
        let b = OcrBoxResult {
            text: "B".into(),
            text_confidence: 0.9,
            bbox: [
                [100.0, 100.0],
                [110.0, 100.0],
                [110.0, 110.0],
                [100.0, 110.0],
            ],
            box_confidence: 0.9,
            x_range: [100.0, 110.0],
            y_range: [100.0, 110.0],
            center: [105.0, 105.0],
        };
        let out = nms(vec![a, b]);
        assert_eq!(out.len(), 2);
    }
}
