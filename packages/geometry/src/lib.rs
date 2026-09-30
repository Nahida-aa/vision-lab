//! 纯 Rust 的二维多边形几何库，用 `glam::Vec2` 表示点，**不依赖 OpenCV 绑定**。
//!
//! 目标：逐步替代 `rapidocr_ort` det 后处理里对 opencv 的几何调用，降低对
//! opencv 绑定（本机 OpenCV 5 下 `min_area_rect` 缺失等）的依赖。所有原语都是
//! 无状态的纯函数，语义对齐 PP-OCR / PaddleOCR 官方 det 后处理：
//!
//! - `convex_hull` —— Andrew 单调链凸包
//! - `min_area_rect` —— 旋转卡壳求最小面积外接矩形（对齐 cv::minAreaRect 的
//!   width≥height 归一化约定）
//! - `box_points` —— 由矩形生成 4 个角点，顺序 tl-tr-br-bl（对齐 PaddleOCR
//!   `get_mini_boxes`）
//! - `polygon_area` / `polygon_length` —— 面积 / 周长
//! - `offset_polygon` —— 顶点法线外扩（近似 pyclipper JT_ROUND）
//!
//! 说明：这些原语在 `min_area_rect` / `box_points` 内部自洽（同一套
//! width/height/angle 约定），可直接作为 det 几何层的基础。若要替换 det.rs
//! 现有轴对齐包围盒，需同步改写 `db_postprocess` 并验证基准指标（之前的尝试
//! 因几何约定混用而回归，见 `rapidocr_ort/src/det.rs` 顶部注释）。
//!
//! 另含 `imgproc` 模块：纯 Rust SIMD 图像像素算子（双线性缩放 / 归一化），
//! 替代 OpenCV 绑定的像素级处理，进一步摆脱 opencv 依赖。

pub mod imgproc;

use glam::Vec2;

/// 二维多边形：一组按顺序排列的点。
pub type Polygon = Vec<Vec2>;

/// 最小外接矩形。`angle` 为弧度，`width` 恒为较长边（对齐 OpenCV
/// `cv::minAreaRect` 的归一化约定），保证 `box_points` 输出稳定。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MinAreaRect {
    pub center: Vec2,
    /// 较长边。
    pub width: f32,
    /// 较短边。
    pub height: f32,
    /// 弧度；`width` 轴相对 x 轴的角度。
    pub angle: f32,
}

/// 凸包（Andrew 单调链，去除共线冗余点，对齐 OpenCV `cv::convexHull` 的
/// 默认行为——返回最小顶点集）。
///
/// 输入点会被排序，重复/共线的内部点被剔除。返回按逆时针顺序的凸包顶点。
pub fn convex_hull(mut pts: Polygon) -> Polygon {
    if pts.len() <= 3 {
        return pts;
    }
    pts.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap().then(a.y.partial_cmp(&b.y).unwrap()));
    let cross = |o: Vec2, a: Vec2, b: Vec2| (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x);

    let mut lower: Polygon = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Polygon = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// 旋转卡壳求最小面积外接矩形（对齐 cpp `geometry.h::minAreaRect`，并做
/// width≥height 归一化以匹配 `cv::minAreaRect` 的输出约定）。
///
/// 对凸包每条边作候选轴，把所有点投影到该边方向与法向，取投影范围作矩形，
/// 记录面积最小者。`width`/`height` 取两次投影范围，随后交换使 `width` 为较长边。
pub fn min_area_rect(pts: &[Vec2]) -> MinAreaRect {
    let hull = convex_hull(pts.to_vec());
    let n = hull.len();
    if n <= 2 {
        return MinAreaRect {
            center: Vec2::ZERO,
            width: 0.0,
            height: 0.0,
            angle: 0.0,
        };
    }
    let edge_angle = |a: Vec2, b: Vec2| (b.y - a.y).atan2(b.x - a.x);

    let mut min_area = f32::MAX;
    let mut best = MinAreaRect {
        center: Vec2::ZERO,
        width: 0.0,
        height: 0.0,
        angle: 0.0,
    };
    for i in 0..n {
        let j = (i + 1) % n;
        let angle = edge_angle(hull[i], hull[j]);
        let (cos_a, sin_a) = (angle.cos(), angle.sin());

        let mut min_proj = f32::MAX;
        let mut max_proj = f32::NEG_INFINITY;
        let mut min_perp = f32::MAX;
        let mut max_perp = f32::NEG_INFINITY;
        for &p in &hull {
            let proj = p.x * cos_a + p.y * sin_a;
            let perp = -p.x * sin_a + p.y * cos_a;
            min_proj = min_proj.min(proj);
            max_proj = max_proj.max(proj);
            min_perp = min_perp.min(perp);
            max_perp = max_perp.max(perp);
        }
        let area = (max_proj - min_proj) * (max_perp - min_perp);
        if area < min_area {
            min_area = area;
            let cx = (min_proj + max_proj) * 0.5 * cos_a - (min_perp + max_perp) * 0.5 * sin_a;
            let cy = (min_proj + max_proj) * 0.5 * sin_a + (min_perp + max_perp) * 0.5 * cos_a;
            best.center = Vec2::new(cx, cy);
            best.width = max_proj - min_proj;
            best.height = max_perp - min_perp;
            best.angle = angle;
        }
    }
    // 归一化：让 width 恒为较长边，angle 相应加 90°（保持同一几何矩形）。
    if best.height > best.width {
        std::mem::swap(&mut best.width, &mut best.height);
        best.angle += std::f32::consts::FRAC_PI_2;
    }
    best
}

/// 由最小外接矩形生成四个角点，顺序 tl-tr-br-bl（顺时针，对齐 PaddleOCR
/// `get_mini_boxes` / cpp `geometry.h::boxPoints`）。
pub fn box_points(r: &MinAreaRect) -> [Vec2; 4] {
    let (cos_a, sin_a) = (r.angle.cos(), r.angle.sin());
    let hw = r.width * 0.5;
    let hh = r.height * 0.5;
    let c = r.center;
    let pts = [
        Vec2::new(c.x + (-hw * cos_a - (-hh) * sin_a), c.y + (-hw * sin_a + (-hh) * cos_a)),
        Vec2::new(c.x + (hw * cos_a - (-hh) * sin_a), c.y + (hw * sin_a + (-hh) * cos_a)),
        Vec2::new(c.x + (hw * cos_a - hh * sin_a), c.y + (hw * sin_a + hh * cos_a)),
        Vec2::new(c.x + (-hw * cos_a - hh * sin_a), c.y + (-hw * sin_a + hh * cos_a)),
    ];
    // x 升序拆左右，各按 y 升序 → tl, tr, br, bl。
    let mut idx = [0usize, 1, 2, 3];
    idx.sort_by(|&a, &b| pts[a].x.partial_cmp(&pts[b].x).unwrap());
    let (l, r) = (&idx[0..2], &idx[2..4]);
    let mut li = l.to_vec();
    let mut ri = r.to_vec();
    li.sort_by(|&a, &b| pts[a].y.partial_cmp(&pts[b].y).unwrap());
    ri.sort_by(|&a, &b| pts[a].y.partial_cmp(&pts[b].y).unwrap());
    let tl = pts[li[0]];
    let bl = pts[li[1]];
    let tr = pts[ri[0]];
    let br = pts[ri[1]];
    [tl, tr, br, bl]
}

/// 多边形面积（鞋带公式，取绝对值）。
pub fn polygon_area(poly: &[Vec2]) -> f32 {
    let mut area = 0.0;
    let n = poly.len();
    for i in 0..n {
        let j = (i + 1) % n;
        area += poly[i].x * poly[j].y - poly[j].x * poly[i].y;
    }
    area.abs() * 0.5
}

/// 多边形周长。
pub fn polygon_length(poly: &[Vec2]) -> f32 {
    let mut len = 0.0;
    let n = poly.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let d = poly[j] - poly[i];
        len += d.length();
    }
    len
}

/// 顶点法线外扩多边形（对齐 cpp `geometry.h::offsetPolygon`）：把每个顶点沿两条
/// 相邻边外法线之和的方向推出 `distance`，近似 pyclipper 的 JT_ROUND 矩形外扩。
///
/// 注意：外扩方向是**角平分线**，故直角角点的位移是 `distance / √2` 的斜向，
/// 而非 pyclipper 的正交位移。这对 det 后处理足够（后续会再取一次最小外接矩形）。
pub fn offset_polygon(poly: &[Vec2], distance: f32) -> Polygon {
    if poly.is_empty() {
        return Polygon::new();
    }
    let n = poly.len();
    let mut result = Polygon::new();
    let mut signed_area = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        signed_area += poly[i].x * poly[j].y - poly[j].x * poly[i].y;
    }
    let sign = if signed_area >= 0.0 { 1.0 } else { -1.0 };

    for i in 0..n {
        let prev = (i + n - 1) % n;
        let next = (i + 1) % n;
        let e1 = poly[i] - poly[prev];
        let e2 = poly[next] - poly[i];
        let len1 = e1.length();
        let len2 = e2.length();
        if len1 < 1e-6 || len2 < 1e-6 {
            continue;
        }
        let n1 = Vec2::new(sign * e1.y / len1, -sign * e1.x / len1);
        let n2 = Vec2::new(sign * e2.y / len2, -sign * e2.x / len2);
        let n = n1 + n2;
        let mut nlen = n.length();
        let mut dir = n;
        if nlen < 1e-6 {
            dir = n1;
            nlen = 1.0;
        }
        let scale = distance / nlen;
        result.push(poly[i] + dir * scale);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) {
        assert!((a - b).abs() <= eps, "{} vs {} (eps {})", a, b, eps);
    }

    fn v2_close(a: Vec2, b: Vec2, eps: f32) {
        close(a.x, b.x, eps);
        close(a.y, b.y, eps);
    }

    #[test]
    fn convex_hull_square() {
        let pts = vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(10.0, 10.0),
            Vec2::new(0.0, 10.0),
        ];
        assert_eq!(convex_hull(pts).len(), 4);
    }

    #[test]
    fn convex_hull_removes_interior_and_dupes() {
        let pts = vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(10.0, 10.0),
            Vec2::new(0.0, 10.0),
            Vec2::new(5.0, 5.0), // 内部
            Vec2::new(0.0, 10.0), // 重复
        ];
        assert_eq!(convex_hull(pts).len(), 4);
    }

    #[test]
    fn convex_hull_collinear_single_line() {
        // 全部共线：凸包应保留两个端点。
        let pts = vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0, 1.0),
            Vec2::new(2.0, 2.0),
            Vec2::new(3.0, 3.0),
        ];
        let hull = convex_hull(pts);
        assert_eq!(hull.len(), 2);
    }

    #[test]
    fn min_area_rect_axis_aligned() {
        // 水平矩形：中心 (5,20)，宽 10 高 8。
        let pts = vec![
            Vec2::new(0.0, 16.0),
            Vec2::new(10.0, 16.0),
            Vec2::new(10.0, 24.0),
            Vec2::new(0.0, 24.0),
        ];
        let r = min_area_rect(&pts);
        v2_close(r.center, Vec2::new(5.0, 20.0), 1e-3);
        close(r.width, 10.0, 1e-3);
        close(r.height, 8.0, 1e-3);
        // width 恒 ≥ height。
        assert!(r.width >= r.height);
        let b = box_points(&r);
        v2_close(b[0], Vec2::new(0.0, 16.0), 1e-3); // tl
        v2_close(b[1], Vec2::new(10.0, 16.0), 1e-3); // tr
        v2_close(b[2], Vec2::new(10.0, 24.0), 1e-3); // br
        v2_close(b[3], Vec2::new(0.0, 24.0), 1e-3); // bl
    }

    #[test]
    fn min_area_rect_rotated_keeps_extent() {
        // 45° 倾斜的 20×6 矩形，中心在原点。
        let angle = 45f32.to_radians();
        let (c, s) = (angle.cos(), angle.sin());
        let (w, h) = (20.0, 6.0);
        let local = [
            Vec2::new(-w / 2.0, -h / 2.0),
            Vec2::new(w / 2.0, -h / 2.0),
            Vec2::new(w / 2.0, h / 2.0),
            Vec2::new(-w / 2.0, h / 2.0),
        ];
        let pts: Vec<Vec2> = local
            .iter()
            .map(|p| Vec2::new(p.x * c - p.y * s, p.x * s + p.y * c))
            .collect();
        let r = min_area_rect(&pts);
        v2_close(r.center, Vec2::ZERO, 1e-2);
        // width ≥ height，且恢复出 20 × 6（允许离散误差）。
        assert!(r.width >= r.height);
        close(r.width, w, 0.5);
        close(r.height, h, 0.5);
    }

    #[test]
    fn min_area_rect_width_is_longer_side() {
        // 竖长矩形：保证 width 被归一化为较长边。
        let pts = vec![
            Vec2::new(4.0, 0.0),
            Vec2::new(4.0, 20.0),
            Vec2::new(6.0, 20.0),
            Vec2::new(6.0, 0.0),
        ];
        let r = min_area_rect(&pts);
        assert!(r.width >= r.height);
        close(r.width, 20.0, 1e-2);
        close(r.height, 2.0, 1e-2);
    }

    #[test]
    fn box_points_roundtrip_matches_extent() {
        let r = MinAreaRect {
            center: Vec2::new(3.0, 4.0),
            width: 12.0,
            height: 5.0,
            angle: 0.3,
        };
        let b = box_points(&r);
        // 每个角点逆旋转到局部系后，坐标应为 (±6, ±2.5) 的某个排列。
        let (cos_a, sin_a) = (r.angle.cos(), r.angle.sin());
        let near = |v: f32, target: f32| (v - target).abs() < 1e-3;
        for p in b {
            let d = p - r.center;
            let lx = d.x * cos_a + d.y * sin_a;
            let ly = -d.x * sin_a + d.y * cos_a;
            let (lx, ly) = (lx.abs(), ly.abs());
            assert!(
                (near(lx, 6.0) && near(ly, 2.5)) || (near(lx, 2.5) && near(ly, 6.0)),
                "corner local ({lx},{ly}) not on 12x5 rect"
            );
        }
    }

    #[test]
    fn polygon_area_rectangle() {
        let poly = [
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(10.0, 4.0),
            Vec2::new(0.0, 4.0),
        ];
        close(polygon_area(&poly), 40.0, 1e-3);
    }

    #[test]
    fn polygon_length_rectangle() {
        let poly = [
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(10.0, 4.0),
            Vec2::new(0.0, 4.0),
        ];
        close(polygon_length(&poly), 28.0, 1e-3);
    }

    #[test]
    fn offset_polygon_expands_along_bisector() {
        // 单位矩形 [0,10]x[0,4]，外扩 2。cpp 的 offsetPolygon 是沿角平分线推出：
        // 直角角点上两条邻边法线 (0,1)+(1,0)=(1,1)，归一化后位移 = distance/√2。
        let poly = [
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(10.0, 4.0),
            Vec2::new(0.0, 4.0),
        ];
        let exp = offset_polygon(&poly, 2.0);
        let d = 2.0 * std::f32::consts::FRAC_1_SQRT_2; // ≈1.414
        v2_close(exp[0], Vec2::new(-d, -d), 1e-3);
        v2_close(exp[1], Vec2::new(10.0 + d, -d), 1e-3);
        v2_close(exp[2], Vec2::new(10.0 + d, 4.0 + d), 1e-3);
        v2_close(exp[3], Vec2::new(-d, 4.0 + d), 1e-3);
    }

    #[test]
    fn offset_polygon_empty_input() {
        assert!(offset_polygon(&[], 2.0).is_empty());
    }

    // =====================================================================
    // 与真实 OpenCV `cv::minAreaRect` + `cv::RotatedRect::points` 的逐位对齐测试。
    // ground truth 来自用本机 OpenCV 5（/usr/include/opencv5）编译的 C++ 探针
    // （tests/bench/subtitle_ocr/tmp/cv_probe.cpp，链接 libopencv_geometry 的
    // minAreaRect）跑出的真实角点。这里硬编码为期望值，验证 geometry 的输出
    // 与 OpenCV 是**同一组 4 角点**（顺序可能不同，按集合比较）。
    // =====================================================================

    /// 断言 geometry 的 box_points 角点集合与 OpenCV 给出的角点集合一致。
    fn assert_corners_match(mine: [Vec2; 4], cv: [Vec2; 4], label: &str, eps: f32) {
        let mut used = [false; 4];
        for &m in &mine {
            let mut best_j = 0;
            let mut best_d = f32::MAX;
            for (j, &c) in cv.iter().enumerate() {
                if !used[j] {
                    let d = m.distance(c);
                    if d < best_d {
                        best_d = d;
                        best_j = j;
                    }
                }
            }
            used[best_j] = true;
            assert!(
                best_d <= eps,
                "[{label}] corner {m:?} not in OpenCV corners {cv:?} (nearest d={best_d} > {eps})"
            );
        }
    }

    #[test]
    fn matches_opencv_axis_aligned() {
        let pts = [
            Vec2::new(0.0, 16.0),
            Vec2::new(10.0, 16.0),
            Vec2::new(10.0, 24.0),
            Vec2::new(0.0, 24.0),
        ];
        let mine = box_points(&min_area_rect(&pts));
        // OpenCV 探针：corner (10,24) (0,24) (0,16) (10,16)
        let cv = [
            Vec2::new(10.0, 24.0),
            Vec2::new(0.0, 24.0),
            Vec2::new(0.0, 16.0),
            Vec2::new(10.0, 16.0),
        ];
        assert_corners_match(mine, cv, "axis", 1e-3);
    }

    #[test]
    fn matches_opencv_rotated_30() {
        let angle = 30f32.to_radians();
        let (c, s) = (angle.cos(), angle.sin());
        let local = [
            Vec2::new(-20.0, -6.0),
            Vec2::new(20.0, -6.0),
            Vec2::new(20.0, 6.0),
            Vec2::new(-20.0, 6.0),
        ];
        let pts: Vec<Vec2> = local
            .iter()
            .map(|p| Vec2::new(p.x * c - p.y * s, p.x * s + p.y * c))
            .collect();
        let mine = box_points(&min_area_rect(&pts));
        // OpenCV 探针：corner (14.32,15.20) (-20.32,-4.80) (-14.32,-15.20) (20.32,4.80)
        let cv = [
            Vec2::new(14.32050705, 15.19615078),
            Vec2::new(-20.32050705, -4.80384731),
            Vec2::new(-14.32050705, -15.19615078),
            Vec2::new(20.32050705, 4.80384731),
        ];
        assert_corners_match(mine, cv, "rot30", 1e-2);
    }

    #[test]
    fn matches_opencv_irregular_pentagon() {
        let pts = [
            Vec2::new(5.0, 0.0),
            Vec2::new(40.0, 4.0),
            Vec2::new(55.0, 30.0),
            Vec2::new(25.0, 45.0),
            Vec2::new(-5.0, 20.0),
            Vec2::new(20.0, 20.0), // 内部点
        ];
        let mine = box_points(&min_area_rect(&pts));
        // OpenCV 探针：corner (35.33,53.61) (-8.93,16.72) (17.38,-14.85) (61.64,22.03)
        let cv = [
            Vec2::new(35.32786560, 53.60655594),
            Vec2::new(-8.93442631, 16.72131157),
            Vec2::new(17.37704849, -14.85245705),
            Vec2::new(61.63934326, 22.03278732),
        ];
        assert_corners_match(mine, cv, "pentagon", 1e-2);
    }

    #[test]
    fn matches_opencv_tall_narrow() {
        let pts = [
            Vec2::new(30.0, 0.0),
            Vec2::new(31.0, 0.0),
            Vec2::new(31.0, 80.0),
            Vec2::new(30.0, 80.0),
        ];
        let mine = box_points(&min_area_rect(&pts));
        // OpenCV 探针：corner (31,80) (30,80) (30,0) (31,0)
        let cv = [
            Vec2::new(31.0, 80.0),
            Vec2::new(30.0, 80.0),
            Vec2::new(30.0, 0.0),
            Vec2::new(31.0, 0.0),
        ];
        assert_corners_match(mine, cv, "tall", 1e-2);
    }

    #[test]
    fn matches_opencv_rotated_45() {
        let angle = 45f32.to_radians();
        let (c, s) = (angle.cos(), angle.sin());
        let local = [
            Vec2::new(-10.0, -3.0),
            Vec2::new(10.0, -3.0),
            Vec2::new(10.0, 3.0),
            Vec2::new(-10.0, 3.0),
        ];
        let pts: Vec<Vec2> = local
            .iter()
            .map(|p| Vec2::new(p.x * c - p.y * s, p.x * s + p.y * c))
            .collect();
        let mine = box_points(&min_area_rect(&pts));
        // OpenCV 探针：corner (4.95,9.19) (-9.19,-4.95) (-4.95,-9.19) (9.19,4.95)
        let cv = [
            Vec2::new(4.94974709, 9.19238663),
            Vec2::new(-9.19238853, -4.94974852),
            Vec2::new(-4.94974804, -9.19238853),
            Vec2::new(9.19238758, 4.94974661),
        ];
        assert_corners_match(mine, cv, "rot45", 1e-2);
    }
}
