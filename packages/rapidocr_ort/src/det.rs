//! DB (Differentiable Binarization) post-processing.
//!
//! Pipeline (bit-for-bit mirrors `subtitle_ocr_cpp/ocr_pipeline.cpp` + `geometry.h`):
//!   1. Apply sigmoid (if values are logits) → probability map.
//!   2. `prob[i] > thr` → 8-bit bitmap.
//!   3. 2×2 dilate to reconnect thin regions (matches Python `use_dilation`).
//!   4. Connected components via cv::findContours.
//!   5. For each contour: `minAreaRect` → `boxPoints` (tl-tr-br-bl) → side<3 早退
//!      → `box_score_fast`（minAreaRect 4 角点掩码内对 prob 取均值）→ unclip
//!      （顶点法线外扩 `offsetPolygon`）→ 再 `minAreaRect` → side<5 早退 → 缩放。
//!   6. 把热力图坐标按 scale 缩回 ROI 坐标（调用方再加 yOffset 回原图）。
//!   7. 丢弃 score < box_threshold 的框，按 score 降序截断。
//!
//! 几何用 `packages/geometry`（glam），其 `min_area_rect` / `box_points` 已验证与
//! 真实 cv::minAreaRect + RotatedRect::points 逐位一致（见 geometry 的对照测试）。
//!
//! ⚠️ 注意：det 几何对齐 minAreaRect 会让 det 框变旋转；此时 rec 裁剪**必须**同时
//! 用 `crop_for_rec_warp`（透视矫正），否则轴对齐裁剪会带进背景导致 rec 严重退化
//! （之前实测 CER(paired) 0.18%→0.36%、CER(norm)→5.72%）。两者耦合，需一起用。

use std::ffi::c_void;

use geometry::{box_points, min_area_rect, offset_polygon, polygon_area, polygon_length};
use glam::Vec2;
use opencv::core::{Mat, Point as CvPoint, Scalar, Size, Vector};
use opencv::imgproc;

const DET_THRESH: f32 = 0.3;
const UNCLIP_RATIO: f32 = 1.6;
const MAX_CANDIDATES: usize = 1000;

/// 单个检测结果：四点多边形（窗口/文本框的四个顶点）与检测得分。
///
/// 与 `subtitle-rust` 的 `DetBox { polygon: [Point; 4], score }` 对齐，
/// 区别仅在于这里用 `glam::Vec2` 表示顶点（geometry 的同一类型，避免在
/// opencv `Point2f` 与几何层之间反复转换）。
pub struct DetBox {
    /// 四个顶点（顺时针：左上、右上、右下、左下），原图像素坐标。
    pub polygon: [Vec2; 4],
    /// 检测得分：框内平均概率（DB 后处理里对框内像素的 prob 取均值）。
    pub score: f32,
}

pub fn db_postprocess(
    heatmap: &[f32],
    hm_w: usize,
    hm_h: usize,
    orig_w: usize,
    orig_h: usize,
    box_thresh: f32,
) -> Vec<DetBox> {
    let sigmoid = heatmap.iter().any(|&v| v > 1.0);
    let prob: Vec<f32> = if sigmoid {
        heatmap.iter().map(|&v| 1.0 / (1.0 + (-v).exp())).collect()
    } else {
        heatmap.to_vec()
    };

    let mut bitmap_data = vec![0u8; hm_w * hm_h];
    for i in 0..hm_w * hm_h {
        if prob[i] > DET_THRESH {
            bitmap_data[i] = 255;
        }
    }

    let bitmap_mat = unsafe {
        Mat::new_rows_cols_with_data_unsafe_def(
            hm_h as i32,
            hm_w as i32,
            opencv::core::CV_8U,
            bitmap_data.as_mut_ptr() as *mut c_void,
        )
        .expect("cv::Mat")
    };

    let mut dilated = Mat::default();
    let kernel = imgproc::get_structuring_element_def(imgproc::MORPH_RECT, Size::new(2, 2))
        .expect("cv::kernel");
    imgproc::dilate_def(&bitmap_mat, &mut dilated, &kernel).expect("cv::dilate");

    let mut contours: Vector<Vector<CvPoint>> = Vector::new();
    imgproc::find_contours_def(
        &dilated,
        &mut contours,
        imgproc::RETR_LIST,
        imgproc::CHAIN_APPROX_SIMPLE,
    )
    .expect("cv::findContours");

    let mut out: Vec<DetBox> = Vec::new();
    let n_contours = contours.len();
    tracing::trace!("[db] contours={} bitmap_px>0.3={}", n_contours, bitmap_data.iter().filter(|&&v| v != 0).count());
    for ci in 0..n_contours {
        let pts_vec = contours.get(ci).expect("contour");
        if pts_vec.len() < 3 {
            continue;
        }
        // 轮廓点（热力图坐标）转 glam::Vec2。
        let contour: Vec<Vec2> = pts_vec.iter().map(|p| Vec2::new(p.x as f32, p.y as f32)).collect();

        // ---- get_mini_boxes(contour) ----
        // minAreaRect → box_points（内部已排成 tl-tr-br-bl）→ 边长 < 3 早退。
        let rect = min_area_rect(&contour);
        let ordered = box_points(&rect);
        let side_a = ordered[0].distance(ordered[1]);
        let side_b = ordered[1].distance(ordered[2]);
        tracing::trace!(
            "[db] contour#{} npts={} minSide={:.1} center=({:.0},{:.0})",
            ci,
            pts_vec.len(),
            side_a.min(side_b),
            contour.iter().map(|p| p.x).sum::<f32>() / contour.len() as f32,
            contour.iter().map(|p| p.y).sum::<f32>() / contour.len() as f32,
        );
        if side_a.min(side_b) < 3.0 {
            continue;
        }

        // ---- box_score_fast(prob, ordered) ----
        // 用 minAreaRect 的 4 点框（非原始轮廓）在掩码内对 prob 取均值，对齐 cpp。
        let score = box_score_fast(&prob, hm_w, hm_h, &ordered);
        tracing::trace!("[db]   contour#{} score={:.4} boxThresh={}", ci, score, box_thresh);
        if score < box_thresh {
            continue;
        }

        // ---- unclip(ordered, distance) ----
        // distance = polygon_area * unclip_ratio / perimeter，min 3.0，顶点法线外扩。
        let area = polygon_area(&ordered);
        let len = polygon_length(&ordered);
        let dist = if len > 0.0 { area * UNCLIP_RATIO / len } else { 0.0 };
        let dist = dist.max(3.0);
        let expanded = offset_polygon(&ordered, dist);
        if expanded.len() < 4 {
            continue;
        }

        // ---- get_mini_boxes(expanded) ----
        // 再 minAreaRect → box_points → 边长 < 5 早退。
        let final_rect = min_area_rect(&expanded);
        let final4 = box_points(&final_rect);
        let fside1 = final4[0].distance(final4[1]);
        let fside2 = final4[1].distance(final4[2]);
        if fside1.min(fside2) < 5.0 {
            continue;
        }

        // ---- scale to ROI size ----
        let scale_w = orig_w as f32 / hm_w as f32;
        let scale_h = orig_h as f32 / hm_h as f32;
        let mut pts = [Vec2::ZERO; 4];
        for (i, p) in final4.iter().enumerate() {
            pts[i] = Vec2::new(
                (p.x * scale_w).round().clamp(0.0, (orig_w - 1) as f32),
                (p.y * scale_h).round().clamp(0.0, (orig_h - 1) as f32),
            );
        }
        out.push(DetBox { polygon: pts, score });
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(MAX_CANDIDATES);
    out
}

/// 标准 `box_score_fast`（对齐 rapidocr_onnxruntime / PaddleOCR / cpp 的
/// `DBPostProcess.box_score_fast`）：取多边形的轴对齐外接框，在该框内用
/// `fillPoly(多边形)` 生成掩码，对掩码覆盖区域的 prob 求均值作为框得分。
/// `pts` 是 minAreaRect 的 4 点框（tl-tr-br-bl），对齐 cpp。
fn box_score_fast(prob: &[f32], hm_w: usize, hm_h: usize, pts: &[Vec2; 4]) -> f32 {
    let (mut xmin, mut ymin, mut xmax, mut ymax) =
        (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in pts.iter() {
        xmin = xmin.min(p.x.floor() as i32);
        xmax = xmax.max(p.x.ceil() as i32);
        ymin = ymin.min(p.y.floor() as i32);
        ymax = ymax.max(p.y.ceil() as i32);
    }
    xmin = xmin.max(0).min(hm_w as i32 - 1);
    xmax = xmax.max(0).min(hm_w as i32 - 1);
    ymin = ymin.max(0).min(hm_h as i32 - 1);
    ymax = ymax.max(0).min(hm_h as i32 - 1);
    if xmax < xmin || ymax < ymin {
        return 0.0;
    }
    let bw = (xmax - xmin + 1) as usize;
    let bh = (ymax - ymin + 1) as usize;

    // 掩码：在 (xmin,ymin) 平移后的坐标系里 fillPoly 四点框（cpp 用 round 到 int）。
    let mut mask = vec![0u8; bw * bh];
    let mut shifted: Vector<CvPoint> = Vector::new();
    for p in pts.iter() {
        shifted.push(CvPoint::new(
            (p.x - xmin as f32).round() as i32,
            (p.y - ymin as f32).round() as i32,
        ));
    }
    let mut mask_mat = unsafe {
        Mat::new_rows_cols_with_data_unsafe_def(
            bh as i32,
            bw as i32,
            opencv::core::CV_8U,
            mask.as_mut_ptr() as *mut c_void,
        )
        .expect("cv::Mat mask")
    };
    let mut contours: Vector<Vector<CvPoint>> = Vector::new();
    contours.push(shifted);
    imgproc::fill_poly(
        &mut mask_mat,
        &contours,
        Scalar::new(1.0, 0.0, 0.0, 0.0),
        imgproc::LINE_8,
        0,
        opencv::core::Point::new(0, 0),
    )
    .expect("cv::fillPoly");

    let mut sum = 0.0f64;
    let mut count: u64 = 0;
    for yy in 0..bh {
        for xx in 0..bw {
            if mask[yy * bw + xx] != 0 {
                let xi = (xx as i32 + xmin) as usize;
                let yi = (yy as i32 + ymin) as usize;
                sum += prob[yi * hm_w + xi] as f64;
                count += 1;
            }
        }
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64) as f32
    }
}
