//! 检测 / 识别 / 方向分类 三分支的输入预处理。
//!
//! 对齐 subtitle-rust（即 Python rapidocr）的做法：
//! - Det: 把**短边**缩放到 736，向下取整到 32 网格（同 Python/CPP）。
//! - Cls: resize 到 48×192，归一化 `(x/255 - 0.5)/0.5`。
//! - Rec: 固定高度 48，保持纵横比，`img_w = int(48 * ratio)`（**不封顶**，
//!   这样宽行不会被压扁、不会掉中段字符）。归一化同样 `(x/255 - 0.5)/0.5`。
//!
//! 与 subtitle-rust 的区别：我们不做 `--subtitle-only` 的底部裁剪（那是字幕
//! 特化的），但 det 的短边缩放逻辑一致。

use ndarray::{Array3, Array4};
use std::path::Path;

/// Det 输入短边目标尺寸（对齐 Python rapidocr 的 `det_limit_side_len=736`）。
pub const DET_LIMIT_SIDE: usize = 736;
/// Cls 输入尺寸。
pub const CLS_W: usize = 192;
pub const CLS_H: usize = 48;
/// Rec 输入高度（PP-OCR v3 / v6 的 rec 均吃 48，v3 之前误用 32 是掉字根因）。
pub const REC_H: usize = 48;
/// Rec 最大宽高比（对齐 rapidocr config rec_img_shape `[3,48,320]` → 320/48）。
/// rec 输入宽 = int(48 × max(该值, 实际宽高比))，对单框恒取 320，右侧补零。
pub const REC_MAX_WH_RATIO: f32 = 320.0 / 48.0;

/// 识别分支归一化参数 `(mean, std)`。
/// PP-OCR 训练时用的就是 `(x/255 - 0.5)/0.5`，与 ImageNet 统计不同；v3 之前
/// 误用了 ImageNet 归一化，模型其实是同一套权重，故统一改回 (0.5,0.5)。
pub const REC_NORM: ([f32; 3], [f32; 3]) = ([0.5, 0.5, 0.5], [0.5, 0.5, 0.5]);

/// 检测预处理：保持长宽比缩放到短边=736，对齐到 32 倍数，归一化 (x/255-mean)/std。
///
/// 返回 `(chw_tensor, resized_h, resized_w)`，其中 tensor 为 `[1,3,H,W]`。
pub fn preprocess_det(img: &Array3<u8>) -> (Array4<f32>, usize, usize) {
    let (h, w, c) = img.dim();
    let (nh, nw) = det_target_size(h, w);
    // det 用 geometry::imgproc 的纯 Rust SIMD 双线性缩放（half-pixel，对齐 cpp 的
    // cv::resize INTER_LINEAR）。曾用 image crate 的 Triangle 核；现改为 bilinear 对齐
    // cpp，需重测基准（见 bench README）。
    let flat: &[u8] = img.as_slice().unwrap();
    let resized = geometry::imgproc::resize_bilinear_hwc(flat, w, h, c, nw, nh);
    // det 归一化必须与 PP-OCR / rapidocr_onnxruntime 一致：mean/std = 0.5/0.5
    // （`(x/255 - 0.5)/0.5`），而非 ImageNet 的 0.485/0.229。曾误用 ImageNet 参数，
    // 导致 det 输入偏置/方差错乱、检测框漏检（如「嗯」字单字漏检）。rec/cls 已用 0.5/0.5。
    let mean = [0.5_f32, 0.5, 0.5];
    let std = [0.5_f32, 0.5, 0.5];
    let chw_flat = geometry::imgproc::normalize_chw(&resized, nh, nw, c, &mean, &std);
    let chw = Array4::from_shape_vec((1, c, nh, nw), chw_flat).expect("CHW 张量形状");
    (chw, nh, nw)
}

/// 计算 det 目标尺寸：对齐 cpp / PP-OCR 官方 Python 的缩放约定。
///
/// cpp（`preprocessDet`，`limit_type='min'`）：
///   if min(h, w) < DET_LIMIT_SIDE: ratio = DET_LIMIT_SIDE / min(h, w)
///   else:                           ratio = 1.0  // 短边已 ≥736 则不缩放
///   newH/newW = round(dim * ratio / 32) * 32，且 ≥ 32。
///
/// 注意：ratio 基于**短边**算、对整个图统一缩放（不是只把短边固定为 736
/// 再单独算长边）。这与旧实现（永远 736*ratio + 向下取整）不同。两版 det 输入
/// 尺寸现在一致，使差异纯粹来自实现本身（缩放插值核、后处理几何等）。
pub fn det_target_size(h: usize, w: usize) -> (usize, usize) {
    let short = h.min(w);
    let ratio = if short < DET_LIMIT_SIDE {
        DET_LIMIT_SIDE as f32 / short as f32
    } else {
        1.0_f32
    };
    // 对齐 cpp 顺序：先按 ratio 截断缩放（int 截断），再 round(dim/32)*32。
    let nh = ((h as f32 * ratio) as usize / 32) as f32;
    let nw = ((w as f32 * ratio) as usize / 32) as f32;
    let nh = (nh.round() * 32.0) as usize;
    let nw = (nw.round() * 32.0) as usize;
    (nh.max(32), nw.max(32))
}

/// 识别预处理：对齐 Python rapidocr_onnxruntime 的 `resize_norm_img`。
///
/// 用 `max_wh_ratio = max(REC_MAX_WH_RATIO, 当前框 wh_ratio)` 决定输入宽
/// `img_w = int(48 * max_wh_ratio)`（对单框，REC_MAX_WH_RATIO 即默认 320/48=6.667
/// 恒为最大，故 img_w 恒 = 320），crop 缩放到 `REC_H × resized_w`（resized_w 按实际
/// 宽高比 ceil），右侧 pad 补零到 img_w。
///
/// ⚠️ 关键：rec 模型对**近方形单字**（如「嗯」，wh_ratio≈1）在窄输入（如 48×45）下
/// 输出全 blank，pad 到 320 宽后能正确识别（实测 argmax 命中「嗯」）。旧实现
/// img_w=int(48*ratio) 不封顶，导致近方形框输入过窄而漏识。
///
/// 返回 `(chw_tensor, rec_w)`。tensor 形状 `[1,3,REC_H,img_w]`。
pub fn preprocess_rec(img: &Array3<u8>) -> (Array4<f32>, usize) {
    let (h, w, _) = img.dim();
    let ratio = w as f32 / h as f32;
    // 默认最大宽高比（对齐 rapidocr config rec_img_shape 320/48），单框时恒最大。
    let max_wh_ratio = (REC_MAX_WH_RATIO).max(ratio);
    let img_w = (REC_H as f32 * max_wh_ratio).round() as usize;
    // resized_w = ceil(48*ratio)，但不超过 img_w（对齐 Python resize_norm_img）。
    let resized_w = if (REC_H as f32 * ratio).ceil() > img_w as f32 {
        img_w
    } else {
        (REC_H as f32 * ratio).ceil() as usize
    };
    let resized_w = resized_w.max(1);
    let resized = resize_bilinear(img, REC_H, resized_w);
    let (mean, std) = REC_NORM;
    let chw = normalize_chw(&resized, &mean, &std);
    // 对齐 cpp：tensor 宽 = imgW（可能 > resized_w，右侧补零）。
    if img_w > resized_w {
        let mut padded = Array4::<f32>::zeros((1, 3, REC_H, img_w));
        for ci in 0..3 {
            for y in 0..REC_H {
                for x in 0..resized_w {
                    padded[[0, ci, y, x]] = chw[[0, ci, y, x]];
                }
            }
        }
        return (padded, img_w);
    }
    (chw, img_w)
}

/// 方向分类预处理：resize 到 `[CLS_H, CLS_W]`，归一化 (x/255-0.5)/0.5。
pub fn preprocess_cls(img: &Array3<u8>) -> Array4<f32> {
    let resized = resize_bilinear(img, CLS_H, CLS_W);
    normalize_chw(&resized, &[0.5, 0.5, 0.5], &[0.5, 0.5, 0.5])
}

/// HWC u8 -> CHW f32 并归一化 `(v/255 - mean)/std`。
fn normalize_chw(img: &Array3<u8>, mean: &[f32; 3], std: &[f32; 3]) -> Array4<f32> {
    let (h, w, c) = img.dim();
    let mut chw = Array4::<f32>::zeros((1, c, h, w));
    for ci in 0..c {
        for y in 0..h {
            for x in 0..w {
                let v = img[[y, x, ci]] as f32 / 255.0;
                chw[[0, ci, y, x]] = (v - mean[ci]) / std[ci];
            }
        }
    }
    chw
}

/// 双线性缩放（HWC u8），基于 `image` crate 的 `imageops::resize`（Triangle）。
/// 比最近邻保留更多笔画细节，识别更稳。
pub fn resize_bilinear(img: &Array3<u8>, nh: usize, nw: usize) -> Array3<u8> {
    let (h, w, c) = img.dim();
    let mut buf: Vec<u8> = Vec::with_capacity(h * w * c);
    for y in 0..h {
        for x in 0..w {
            for k in 0..c {
                buf.push(img[[y, x, k]]);
            }
        }
    }
    let src = image::RgbImage::from_raw(w as u32, h as u32, buf).expect("构造源图失败");
    let dst = image::imageops::resize(&src, nw as u32, nh as u32, image::imageops::FilterType::Triangle);
    let mut out = Array3::<u8>::zeros((nh, nw, c));
    for y in 0..nh {
        for x in 0..nw {
            let p = dst.get_pixel(x as u32, y as u32);
            for k in 0..c {
                out[[y, x, k]] = p.0[k];
            }
        }
    }
    out
}

/// 最近邻缩放（HWC u8），用于 det 这种只需粗略对齐的场合（速度优先）。
#[allow(dead_code)]
pub fn resize_nearest(img: &Array3<u8>, nh: usize, nw: usize) -> Array3<u8> {
    let (h, w, c) = img.dim();
    let mut out = Array3::<u8>::zeros((nh, nw, c));
    for y in 0..nh {
        let sy = ((y as f32 + 0.5) / nh as f32 * h as f32 - 0.5).max(0.0) as usize;
        let sy = sy.min(h - 1);
        for x in 0..nw {
            let sx = ((x as f32 + 0.5) / nw as f32 * w as f32 - 0.5).max(0.0) as usize;
            let sx = sx.min(w - 1);
            for k in 0..c {
                out[[y, x, k]] = img[[sy, sx, k]];
            }
        }
    }
    out
}

/// 调试用：把某张图存盘（需显式调用，main/test 里按需启用）。
#[allow(dead_code)]
pub fn dump_png(path: &Path, img: &Array3<u8>) {
    let (h, w, _c) = img.dim();
    let buf = img.as_slice().unwrap().to_vec();
    if let Some(rgb) = image::RgbImage::from_raw(w as u32, h as u32, buf) {
        let _ = rgb.save(path);
    }
}
