//! 逐帧预处理：相邻帧交集（去噪）+ 水平投影检测文字（AnalyseImage）。
//!
//! 复刻 VideoSubFinder：
//! - `IntersectTwoImages`：对 `Im2` 为 0 的像素，把 `ImRes` 对应位置清 0（取公共非零区）。
//! - `AnalyseImage`：按 `segh` 高度水平分带，统计白色像素密度最大的带，用
//!   `tp`（文字占比）/`mtpl`（最小文字长度）判断该帧是否有文字。
//!
//! 输入用**单通道灰度 `Array2<u8>`**（0-255），白色为 255。

use ndarray::Array2;

use super::params::Params;

/// 对齐方式（VideoSubFinder `TextAlignment`）。默认 `Any`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextAlignment {
    Any,
    Center,
    Left,
    Right,
}

impl Default for TextAlignment {
    fn default() -> Self {
        TextAlignment::Any
    }
}

/// 相邻帧交集：对 `b` 为 0 的像素，把 `a` 对应位置清 0（取公共非零区，去单帧闪烁）。
/// `a` 与 `b` 均为单通道灰度，尺寸相同，返回新图。
pub fn intersect_two_images(a: &Array2<u8>, b: &Array2<u8>) -> Array2<u8> {
    let (h, w) = a.dim();
    debug_assert_eq!(b.dim(), (h, w));
    let mut out = a.clone();
    for y in 0..h {
        for x in 0..w {
            if b[[y, x]] == 0 {
                out[[y, x]] = 0;
            }
        }
    }
    out
}

/// 分析一帧灰度图是否有文字行（复刻 `AnalyseImage`）。
/// `frame` 为单通道灰度；返回是否检测到文字。非连续内存时退化为拷贝。
pub fn analyse_image(frame: &Array2<u8>, params: &Params) -> bool {
    let (h, w) = frame.dim();
    if h == 0 || w == 0 {
        return false;
    }
    if let Some(s) = frame.as_slice() {
        return analyse_image_flat(s, w, h, params);
    }
    let owned: Vec<u8> = frame.iter().copied().collect();
    analyse_image_flat(&owned, w, h, params)
}

/// `AnalyseImage` 的行优先实现（`analyse_image` 的连续内存版本）。
///
/// 与 C++ `SSAlgorithms::AnalyseImage` 语义逐位一致（`analyse_flat_matches_legacy`
/// 用随机帧与旧实现逐帧对拍），只改数据布局与扫描顺序：
/// 1. 白像素统计按**行**连续扫描（`chunks_exact`），不再列优先 + ndarray 二维索引
///    （`frame[[y, x]]` 每次 2 次乘加 + 边界检查）。1280×720 实测约 0.47 → 0.41 ms/帧。
/// 2. **只保留密度最大带**的「有白列」栅格，不再给每条带分配一个 `Vec<(usize, usize)>`
///    段表：h=720、segh=3 时每帧 240 条带、240 次小分配，而后续裁剪只用到密度最大那条带。
///
/// 判定逻辑（`len`/`len2`/`tp`/`len < mtl`、`lb[0] * 2 >= w` 短路、Center 的
/// `val1 <= val2` 收左边/右边）与旧实现逐字一致。
pub fn analyse_image_flat(frame: &[u8], w: usize, h: usize, params: &Params) -> bool {
    let segh = params.segh;
    if w == 0 || h == 0 || segh == 0 {
        return false;
    }
    let tp = params.tp;
    let mtl = (params.mtpl * w as f32) as usize;
    let n = h / segh; // 水平条带数
    if n == 0 {
        return false;
    }
    let da = w * segh;
    if frame.len() < n * da {
        return false;
    }

    // 每列的白色像素计数（当前条带局部）；!= 0 即「该列在带内有白」。
    let mut cnt = vec![0u16; w];
    // 最大密度带的“有白列”栅格（逐像素计数只留最大带，省掉每条带的段表分配）。
    let mut best_col = vec![0u16; w];
    let mut best_pl = 0usize;

    for k in 0..n {
        let band = &frame[k * da..k * da + da];
        cnt.fill(0);
        let mut pl = 0usize;
        for row in band.chunks_exact(w) {
            for (c, &v) in cnt.iter_mut().zip(row) {
                // v == 255 才计数；u16 累加，单带内最多 segh 次，不会溢出。
                if v == 255 {
                    *c += 1;
                    pl += 1;
                }
            }
        }
        if pl > best_pl {
            best_pl = pl;
            best_col.copy_from_slice(&cnt);
        }
    }

    if best_pl == 0 {
        return false;
    }

    // 段 = 连续「有白列」区间（左闭右闭），与旧实现逐列扫描得到的一致。
    let mut lb: Vec<usize> = Vec::new();
    let mut le: Vec<usize> = Vec::new();
    let mut x = 0usize;
    while x < w {
        if best_col[x] != 0 {
            let s = x;
            while x < w && best_col[x] != 0 {
                x += 1;
            }
            lb.push(s);
            le.push(x - 1);
        } else {
            x += 1;
        }
    }
    if lb.is_empty() {
        return false;
    }

    let mut len: usize = lb.iter().zip(le.iter()).map(|(&a, &b)| b - a + 1).sum();
    let mut l = lb.len() - 1;

    // 对齐裁剪（C++ `g_text_alignment` 默认 **Center**，非 Any！）。
    // ⚠️ 之前硬编码 Any 跳过 Center 的段裁剪 → 保留偏离中心噪声段 → has_text 误判。
    let align = TextAlignment::Center;
    if align != TextAlignment::Any {
        while l > 0 {
            if len < mtl {
                return false;
            }
            let len2 = le[l] - lb[0] + 1;
            if (len as f32) / (len2 as f32) >= tp {
                return true;
            }
            match align {
                TextAlignment::Center => {
                    if lb[0] * 2 >= w {
                        return false;
                    }
                    let val1 = {
                        let v = (le[l - 1] + lb[0] + 1) as i64 - w as i64;
                        v.abs() as usize
                    };
                    let val2 = {
                        let v = (le[l] + lb[1] + 1) as i64 - w as i64;
                        v.abs() as usize
                    };
                    if val1 <= val2 {
                        len -= le[l] - lb[l] + 1;
                    } else {
                        len -= le[0] - lb[0] + 1;
                        // 移除第一段（对齐原代码对 g_lb/g_le 的 shift）。
                        lb.remove(0);
                        le.remove(0);
                    }
                }
                TextAlignment::Left => {
                    len -= le[l] - lb[l] + 1;
                }
                _ => {
                    len -= le[0] - lb[0] + 1;
                    lb.remove(0);
                    le.remove(0);
                }
            }
            l -= 1;
        }
    }

    if len > mtl {
        if lb[0] * 2 < w || align != TextAlignment::Center {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params::default()
    }

    /// 旧版 `AnalyseImage`（列优先 + ndarray 二维索引 + 每条带一个段表），
    /// 作为语义基准与 [`analyse_image_flat`] 逐帧对拍。
    fn analyse_image_legacy(frame: &Array2<u8>, params: &Params) -> bool {
        let (h, w) = frame.dim();
        if h == 0 || w == 0 {
            return false;
        }
        let segh = params.segh;
        let tp = params.tp;
        let mtl = (params.mtpl * w as f32) as usize;
        let n = h / segh;
        if n == 0 {
            return false;
        }
        let da = w * segh;
        let mut best_pl = 0usize;
        let mut best_k = 0usize;
        let mut segs_per_band: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
        for k in 0..n {
            let ia = k * da;
            let mut segs: Vec<(usize, usize)> = Vec::new();
            let mut pl = 0usize;
            let mut in_seg = false;
            let mut seg_start = 0usize;
            let mut seg_end = 0usize;
            for x in 0..w {
                let mut any_white = false;
                for y in 0..segh {
                    if frame[[ia / w + y, x]] == 255 {
                        any_white = true;
                        pl += 1;
                    }
                }
                if any_white {
                    if !in_seg {
                        seg_start = x;
                        seg_end = x;
                        in_seg = true;
                    } else {
                        seg_end = x;
                    }
                } else if in_seg {
                    segs.push((seg_start, seg_end));
                    in_seg = false;
                }
            }
            if in_seg {
                segs.push((seg_start, seg_end));
            }
            segs_per_band[k] = segs;
            if pl > best_pl {
                best_pl = pl;
                best_k = k;
            }
        }
        if best_pl == 0 {
            return false;
        }
        let mut segs: Vec<(usize, usize)> = segs_per_band[best_k].clone();
        if segs.is_empty() {
            return false;
        }
        let mut len: usize = segs.iter().map(|&(lb, le)| le - lb + 1).sum();
        let mut l = segs.len() - 1;
        while l > 0 {
            if len < mtl {
                return false;
            }
            let len2 = segs[l].1 - segs[0].0 + 1;
            if (len as f32) / (len2 as f32) >= tp {
                return true;
            }
            if segs[0].0 * 2 >= w {
                return false;
            }
            let val1 = {
                let v = (segs[l - 1].1 + segs[0].0 + 1) as i64 - w as i64;
                v.abs() as usize
            };
            let val2 = {
                let v = (segs[l].1 + segs[1].0 + 1) as i64 - w as i64;
                v.abs() as usize
            };
            if val1 <= val2 {
                len -= segs[l].1 - segs[l].0 + 1;
            } else {
                len -= segs[0].1 - segs[0].0 + 1;
                segs.remove(0);
            }
            l -= 1;
        }
        if len > mtl {
            if (segs[0].0 * 2 < w) || TextAlignment::Center != TextAlignment::Any {
                return true;
            }
        }
        false
    }

    /// 确定性伪随机帧（无外部依赖），模拟字幕边缘噪声。
    fn noise_frame(w: usize, h: usize, seed: u64, density_pct: u32) -> Vec<u8> {
        let mut s = seed | 1;
        let mut f = vec![0u8; w * h];
        for p in f.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            if (s % 100) < density_pct as u64 {
                *p = 255;
            }
        }
        f
    }

    #[test]
    fn intersect_keeps_common_nonzero() {
        // a 全白，b 只有左半白 → 交集保留左半。
        let mut a = Array2::<u8>::zeros((2, 4));
        a.fill(255);
        let mut b = Array2::<u8>::zeros((2, 4));
        for y in 0..2 {
            for x in 0..2 {
                b[[y, x]] = 255;
            }
        }
        let out = intersect_two_images(&a, &b);
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(out[[y, x]], 255, "({},{})", y, x);
            }
            for x in 2..4 {
                assert_eq!(out[[y, x]], 0, "({},{})", y, x);
            }
        }
    }

    #[test]
    fn analyse_no_text_when_blank() {
        let frame = Array2::<u8>::zeros((9, 20));
        assert!(!analyse_image(&frame, &params()));
    }

    #[test]
    fn analyse_flat_matches_legacy() {
        let p = params();
        let cases: Vec<(usize, usize, Vec<u8>)> = vec![
            (720, 1280, noise_frame(1280, 720, 0x1234_5678, 3)),
            (720, 1280, noise_frame(1280, 720, 0xdead_beef, 12)),
            (720, 1280, noise_frame(1280, 720, 0x2026_1102, 40)),
            (360, 640, noise_frame(640, 360, 0xabc, 8)),
        ];
        for (h, w, f) in cases {
            let arr = Array2::<u8>::from_shape_vec((h, w), f).unwrap();
            assert_eq!(
                analyse_image(&arr, &p),
                analyse_image_legacy(&arr, &p),
                "{}x{}",
                h,
                w
            );
        }
    }

    /// 中途横插一条“最密带”的帧（考察最大带切换时统计是否跟着换）。
    #[test]
    fn analyse_flat_matches_legacy_dense_row() {
        let (h, w) = (9, 200);
        let mut frame = Array2::<u8>::zeros((h, w));
        // 一条横跨大部分宽度的白行（segh=3，取一整个条带为白），宽度远超 mtl。
        for y in 3..6 {
            for x in 4..196 {
                frame[[y, x]] = 255;
            }
        }
        assert!(analyse_image(&frame, &params()), "应有文字");
    }

    /// 只有零星几个白点，宽度远小于 mtl（mtl = 0.022*200 ≈ 4）。
    #[test]
    fn analyse_rejects_tiny_noise() {
        // 只有零星几个白点，宽度远小于 mtl（mtl = 0.022*200 ≈ 4）。
        let (h, w) = (9, 200);
        let mut frame = Array2::<u8>::zeros((h, w));
        // 单个白列（段宽 1），即使有 segh 个白像素，长度仍 < mtl。
        for y in 3..6 {
            frame[[y, 5]] = 255;
        }
        assert!(!analyse_image(&frame, &params()), "噪声不应判为文字");
    }
}
