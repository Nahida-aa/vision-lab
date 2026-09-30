//! 识别流水线里的「裁剪 → 分类(可选) → 识别」步骤。
//!
//! 与 subtitle-rust 的 `pipeline.rs::run_ocr_core` 对应。`crop_for_rec` 提供
//! 包围盒裁剪；`crop_for_rec_warp` 提供 cpp 同款的透视矫正裁剪（用 faer 求解
//! getPerspectiveTransform），用于让 rust 与 cpp 的 rec 输入逐位一致。

use glam::Vec2;
use ndarray::Array3;
use opencv::imgproc;
use opencv::prelude::*;
use ort::session::Session;

use crate::cls;
use crate::preprocess::preprocess_rec;
use crate::rec::ctc_greedy_decode;

/// 用 faer 解线性系统求透视变换矩阵（对应 OpenCV `getPerspectiveTransform`）。
///
/// 4 组对应点 (src→dst) 求解 3×3 单应矩阵 H（h22=1，8 个未知数），用 DLT 构造
/// 8×8 线性系统，faer 的 `partial_piv_lu().solve()` 求解。
///
/// 返回 `[[f64;3];3]`（行优先的 3×3 H）。
fn get_perspective_transform(src: &[Vec2; 4], dst: &[Vec2; 4]) -> [[f64; 3]; 3] {
    use faer::prelude::*;

    // DLT: 对每对点 (x,y)->(u,v) 贡献两行。
    let mut a = faer::Mat::<f64>::zeros(8, 8);
    let mut b = faer::Mat::<f64>::zeros(8, 1);
    for (i, (s, d)) in src.iter().zip(dst.iter()).enumerate() {
        let (x, y) = (s.x as f64, s.y as f64);
        let (u, v) = (d.x as f64, d.y as f64);
        // row 2i
        a[(2 * i, 0)] = x;
        a[(2 * i, 1)] = y;
        a[(2 * i, 2)] = 1.0;
        a[(2 * i, 6)] = -u * x;
        a[(2 * i, 7)] = -u * y;
        b[(2 * i, 0)] = u;
        // row 2i+1
        a[(2 * i + 1, 3)] = x;
        a[(2 * i + 1, 4)] = y;
        a[(2 * i + 1, 5)] = 1.0;
        a[(2 * i + 1, 6)] = -v * x;
        a[(2 * i + 1, 7)] = -v * y;
        b[(2 * i + 1, 0)] = v;
    }

    let lu = a.partial_piv_lu();
    let x = lu.solve(&b);
    [
        [x[(0, 0)], x[(1, 0)], x[(2, 0)]],
        [x[(3, 0)], x[(4, 0)], x[(5, 0)]],
        [x[(6, 0)], x[(7, 0)], 1.0],
    ]
}

/// 透视变换裁剪（对应 cpp 的 `warpPerspective(INTER_CUBIC, BORDER_REPLICATE)`）。
///
/// 把四边形 `polygon`（tl-tr-br-bl）矫正成 `dst_w×dst_h` 的水平矩形。直接用
/// OpenCV 的 `cv::warpPerspective`（`INTER_CUBIC` + `BORDER_REPLICATE`），与 cpp
/// 的 rec 输入**逐位一致**（OpenCV 内部 invert M 后做定点插值，rust 手写无法逐位
/// 复刻，故这里直接调 OpenCV 绑定）。
fn warp_perspective(img: &Array3<u8>, polygon: &[Vec2; 4], dst_w: usize, dst_h: usize) -> Array3<u8> {
    let (h, w, c) = img.dim();
    let src = [
        Vec2::new(polygon[0].x, polygon[0].y),
        Vec2::new(polygon[1].x, polygon[1].y),
        Vec2::new(polygon[2].x, polygon[2].y),
        Vec2::new(polygon[3].x, polygon[3].y),
    ];
    let dst = [
        Vec2::new(0.0, 0.0),
        Vec2::new((dst_w - 1) as f32, 0.0),
        Vec2::new((dst_w - 1) as f32, (dst_h - 1) as f32),
        Vec2::new(0.0, (dst_h - 1) as f32),
    ];
    // 正向 H（src→dst），OpenCV warpPerspective 内部会 invert（非 WARP_INVERSE_MAP）。
    let fwd = get_perspective_transform(&src, &dst);
    let m_data: Vec<f64> = (0..3)
        .flat_map(|r| (0..3).map(move |col| fwd[r][col]))
        .collect();
    let m_1d = opencv::core::Mat::from_slice(&m_data).expect("M mat");
    let m = m_1d.reshape(1, 3).expect("M reshape");

    // 源图 → OpenCV Mat（CV_8UC3, 行优先 RGB/BGR 由调用方保证）。
    let mut src_data = Vec::with_capacity(h * w * c);
    for y in 0..h {
        for x in 0..w {
            for k in 0..c {
                src_data.push(img[[y, x, k]]);
            }
        }
    }
    let src_1d = opencv::core::Mat::from_slice(&src_data).expect("src mat");
    let src_mat = src_1d.reshape(3, h as i32).expect("src reshape");

    // 显式 flags：INTER_CUBIC + BORDER_REPLICATE，borderValue 0（对齐 cpp）。
    // 默认提示 ALGO_HINT_ACCURATE：Windows/Linux 均用 OpenCV 5 头（vcpkg 无 opencv5，
    // Windows 使用官方 prebuilt 5.0.0），仅 8 参重载，无需按平台分流。
    let mut dst_mat = opencv::core::Mat::default();
    imgproc::warp_perspective(
        &src_mat,
        &mut dst_mat,
        &m,
        opencv::core::Size::new(dst_w as i32, dst_h as i32),
        opencv::imgproc::INTER_CUBIC,
        opencv::core::BORDER_REPLICATE,
        opencv::core::Scalar::new(0.0, 0.0, 0.0, 0.0),
        opencv::core::AlgorithmHint::ALGO_HINT_ACCURATE,
    )
    .expect("warpPerspective cubic");

    // 读回 Array3。
    let mut out = Array3::<u8>::zeros((dst_h, dst_w, c));
    let data = dst_mat.data_bytes().expect("dst data");
    let stride = dst_mat.step1(0).expect("dst step") as usize;
    for y in 0..dst_h {
        for x in 0..dst_w {
            for k in 0..c {
                out[[y, x, k]] = data[y * stride + x * c + k];
            }
        }
    }
    let _ = (dst_mat, src_mat);
    out
}

/// OpenCV `interpolateCubic`（A = -0.75）的 4 个三次卷积系数，`x∈[0,1)` 为分数位。
#[cfg(test)]
fn cubic_coeffs(x: f64) -> [f64; 4] {
    const A: f64 = -0.75;
    let x1 = x + 1.0;
    let c0 = ((A * x1 - 5.0 * A) * x1 + 8.0 * A) * x1 - 4.0 * A;
    let c1 = ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0;
    let u = 1.0 - x;
    let c2 = ((A + 2.0) * u - (A + 3.0)) * u * u + 1.0;
    let c3 = 1.0 - c0 - c1 - c2;
    [c0, c1, c2, c3]
}

/// 按四点 bbox 从原图裁剪（包围盒 crop，不做透视变换；对近水平文本足够）。
/// `polygon` 为四个顶点（顺时针），这里取其 x/y 的极值作为包围盒，向四周外扩
/// 5% 避免裁掉字形上下缘。对任意四边形（含旋转框）都取正确包围盒。
pub fn crop_for_rec(img: &Array3<u8>, polygon: &[Vec2; 4]) -> Array3<u8> {
    let (h, w, c) = img.dim();
    let minx_f = polygon.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
    let maxx_f = polygon
        .iter()
        .map(|p| p.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let miny_f = polygon.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
    let maxy_f = polygon
        .iter()
        .map(|p| p.y)
        .fold(f32::NEG_INFINITY, f32::max);
    // 对齐 Python RapidOCR 行为：不加 pad，直接按 polygon 的轴对齐外接矩形裁剪。
    // 旧 5% pad 会让 unclip 后的框四周多出背景，rec 对"字未填满"输入识别失败
    // （如 51×54 含两字的 crop 被判为全 blank）。Python det 框本就紧密外接，无需额外 pad。
    let minx = minx_f.max(0.0).floor() as usize;
    let miny = miny_f.max(0.0).floor() as usize;
    let maxx = (maxx_f + 1.0).min(w as f32).ceil() as usize;
    let maxy = (maxy_f + 1.0).min(h as f32).ceil() as usize;
    if maxx <= minx || maxy <= miny {
        return Array3::<u8>::zeros((1, 1, c));
    }
    let mut out = Array3::<u8>::zeros((maxy - miny, maxx - minx, c));
    for y in miny..maxy {
        for x in minx..maxx {
            for k in 0..c {
                out[[y - miny, x - minx, k]] = img[[y, x, k]];
            }
        }
    }
    out
}

/// cpp 同款的透视矫正裁剪：把四边形 `polygon`（tl-tr-br-bl）矫正成水平矩形。
///
/// 完全复刻 cpp `ocr_pipeline.cpp` 的 crop 流程：
///   dstW/H 由四边长度取 max 后 round 得到（min 4）；若 dstH/dstW ≥ 1.5 则转置
///   90°（高瘦框）；getPerspectiveTransform(polygon, 矩形角) → warpPerspective。
/// 用于与 cpp 的 rec 输入逐位对齐（配合 det 几何 minAreaRect 一起用）。
pub fn crop_for_rec_warp(img: &Array3<u8>, polygon: &[Vec2; 4]) -> Array3<u8> {
    let (x0, y0) = (polygon[0].x, polygon[0].y);
    let (x1, y1) = (polygon[1].x, polygon[1].y);
    let (x2, y2) = (polygon[2].x, polygon[2].y);
    let (x3, y3) = (polygon[3].x, polygon[3].y);
    let w1 = ((x1 - x0) * (x1 - x0) + (y1 - y0) * (y1 - y0)).sqrt();
    let w2 = ((x2 - x3) * (x2 - x3) + (y2 - y3) * (y2 - y3)).sqrt();
    let h1 = ((x3 - x0) * (x3 - x0) + (y3 - y0) * (y3 - y0)).sqrt();
    let h2 = ((x2 - x1) * (x2 - x1) + (y2 - y1) * (y2 - y1)).sqrt();

    let dst_w = 4usize.max(w1.max(w2).round() as usize);
    let dst_h = 4usize.max(h1.max(h2).round() as usize);

    let rotate90 = (dst_h as f32) / (dst_w as f32) >= 1.5;
    let (out_w, out_h) = if rotate90 { (dst_h, dst_w) } else { (dst_w, dst_h) };

    let mut warped = warp_perspective(img, polygon, dst_w, dst_h);

    // 高瘦框转置 90°（顺时针），使文本水平。
    if rotate90 {
        let (oh, ow, c) = warped.dim();
        let mut rot = Array3::<u8>::zeros((ow, oh, c));
        for y in 0..oh {
            for x in 0..ow {
                for k in 0..c {
                    rot[[x, oh - 1 - y, k]] = warped[[y, x, k]];
                }
            }
        }
        warped = rot;
    }
    // out_w/out_h 可能与 warped 尺寸差 1（round 差异），这里直接按 warped 返回。
    let _ = (out_w, out_h);
    warped
}

/// 对一个裁剪块跑 rec，返回 `(文本, 置信度)`。
///
/// `rec` 为 rec Session；`rec_out_name` 为其输出节点名；`vocab` 为字典（索引 0
/// 为 blank）。`use_cls` 为真时先跑 cls（输出节点名 `cls_out_name`）判断是否需要旋转 180°。
pub fn recognize(
    img: &Array3<u8>,
    rec: &mut Session,
    rec_out_name: &str,
    cls: &mut Session,
    cls_out_name: &str,
    use_cls: bool,
    vocab: &[String],
) -> (String, f32) {
    // 可选方向分类
    let rec_crop = if use_cls {
        let cls_in = crate::preprocess::preprocess_cls(img);
        let cls_tensor = ort::value::Tensor::from_array(cls_in).expect("构造 cls 输入张量失败");
        let cls_out = cls
            .run(ort::inputs!["x" => cls_tensor])
            .expect("cls 推理失败");
        let cls_arr = cls_out[cls_out_name]
            .try_extract_array::<f32>()
            .unwrap()
            .to_owned();
        let cls_slice: Vec<f32> = cls_arr.as_slice().unwrap().to_vec();
        if cls::need_rotate_180(&cls_slice) {
            cls::rotate_180(img)
        } else {
            img.clone()
        }
    } else {
        img.clone()
    };

    let (rec_in, rec_w) = preprocess_rec(&rec_crop);
    tracing::trace!("[rec_in] first24={:?} shape={:?}", rec_in.as_slice().unwrap().iter().take(24).copied().collect::<Vec<f32>>(), rec_in.shape());
    let rec_tensor = ort::value::Tensor::from_array(rec_in).expect("构造 rec 输入张量失败");
    let rec_out = rec
        .run(ort::inputs!["x" => rec_tensor])
        .expect("rec 推理失败");
    let logits = rec_out[rec_out_name]
        .try_extract_array::<f32>()
        .unwrap()
        .to_owned();
    // rec 输出形状 [1, T, C]（C=字符类数，T=时间步），直接取最后两维。
    let shape = logits.shape().to_vec();
    let flat = logits.as_slice().unwrap().to_vec();
    let (text, conf) = ctc_greedy_decode(&flat, &shape, vocab);
    tracing::trace!(
        "[rec] crop_in={:?} rec_crop={:?} rec_input_w={} -> text={:?} conf={:.4} logits_shape={:?}",
        (img.dim().0, img.dim().1),
        (rec_crop.dim().0, rec_crop.dim().1),
        rec_w,
        text,
        conf,
        shape
    );
    (text, conf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f32, y: f32) -> Vec2 {
        Vec2::new(x, y)
    }

    #[test]
    fn perspective_identity() {
        // 矩形映射到自身 → H ≈ 恒等（仿射）。
        let src = [pt(0.0, 0.0), pt(10.0, 0.0), pt(10.0, 5.0), pt(0.0, 5.0)];
        let dst = [pt(0.0, 0.0), pt(10.0, 0.0), pt(10.0, 5.0), pt(0.0, 5.0)];
        let h = get_perspective_transform(&src, &dst);
        // 恒等：h00≈1, h11≈1, 其余≈0。
        assert!((h[0][0] - 1.0).abs() < 1e-6, "h00={}", h[0][0]);
        assert!((h[1][1] - 1.0).abs() < 1e-6, "h11={}", h[1][1]);
        assert!(h[2][0].abs() < 1e-6 && h[2][1].abs() < 1e-6);
    }

    #[test]
    fn perspective_maps_corners() {
        // 把一个 45° 旋转/平移的四边形矫正回 [0,W]x[0,H] 矩形。
        let src = [pt(0.0, 0.0), pt(10.0, 0.0), pt(10.0, 5.0), pt(0.0, 5.0)];
        let dst = [pt(0.0, 0.0), pt(20.0, 0.0), pt(20.0, 10.0), pt(0.0, 10.0)];
        let h = get_perspective_transform(&src, &dst);
        // 验证 H 把 src 角点映射到 dst：src[0]=(0,0) → dst[0]=(0,0)。
        let apply = |p: Vec2| -> (f64, f64) {
            let (x, y) = (p.x as f64, p.y as f64);
            let denom = h[2][0] * x + h[2][1] * y + h[2][2];
            let u = (h[0][0] * x + h[0][1] * y + h[0][2]) / denom;
            let v = (h[1][0] * x + h[1][1] * y + h[1][2]) / denom;
            (u, v)
        };
        let (u0, v0) = apply(src[0]);
        assert!((u0 - 0.0).abs() < 1e-6 && (v0 - 0.0).abs() < 1e-6, "corner0=({u0},{v0})");
        let (u1, v1) = apply(src[1]);
        assert!((u1 - 20.0).abs() < 1e-6 && (v1 - 0.0).abs() < 1e-6, "corner1=({u1},{v1})");
        let (u3, v3) = apply(src[3]);
        assert!((u3 - 0.0).abs() < 1e-6 && (v3 - 10.0).abs() < 1e-6, "corner3=({u3},{v3})");
    }

    #[test]
    fn warp_perspective_axis_aligned_box() {
        // 对一张 20x10 的图，裁剪轴对齐框 (0,0)-(10,5) → 应得到原图左上 10x5 区域。
        let (w, h) = (20usize, 10usize);
        let mut img = Array3::<u8>::zeros((h, w, 3));
        for y in 0..h {
            for x in 0..w {
                img[[y, x, 0]] = (x + y) as u8;
                img[[y, x, 1]] = x as u8;
                img[[y, x, 2]] = y as u8;
            }
        }
        let poly = [pt(0.0, 0.0), pt(10.0, 0.0), pt(10.0, 5.0), pt(0.0, 5.0)];
        let crop = crop_for_rec_warp(&img, &poly);
        let (ch, cw, _) = crop.dim();
        assert_eq!(cw, 10);
        assert_eq!(ch, 5);
        // 左上角像素应与原图一致（src(0,0)→dst(0,0)，整数坐标处三次插值精确通过）。
        assert_eq!(crop[[0, 0, 0]], img[[0, 0, 0]]);
        // 底部区域近似原图（bicubic 有轻微振铃，允许偏差）。
        let br = crop[[4, 9, 0]] as i32;
        let ref_val = 15i32;
        assert!((br - ref_val).abs() <= 4, "bottom-right={br}, ref={ref_val}");
    }

    #[test]
    fn cubic_coeffs_match_opencv() {
        // OpenCV interpolateCubic(A=-0.75) 在 x=0.5 的已知系数。
        let c = cubic_coeffs(0.5);
        // x=0.5: c0=(A*1.5-5A)*1.5+8A)*1.5-4A；逐项验证总和≈1。
        let sum: f64 = c.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "sum={sum}");
        // x=0 时：c1=1，其余 0（插值精确经过采样点）。
        let c0 = cubic_coeffs(0.0);
        assert!((c0[1] - 1.0).abs() < 1e-9, "c0[1]={}", c0[1]);
        assert!(c0[0].abs() < 1e-9 && c0[2].abs() < 1e-9 && c0[3].abs() < 1e-9);
    }
}
