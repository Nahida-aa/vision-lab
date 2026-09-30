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
/// `frame` 为单通道灰度；返回是否检测到文字。
pub fn analyse_image(frame: &Array2<u8>, params: &Params) -> bool {
    let (h, w) = frame.dim();
    if h == 0 || w == 0 {
        return false;
    }
    let segh = params.segh;
    let tp = params.tp;
    let mtl = (params.mtpl * w as f32) as usize;

    let n = h / segh; // 水平条带数
    if n == 0 {
        return false;
    }
    let da = w * segh;

    // 每条带的白色段列表 (lb, le)，以及最大密度带。
    let mut best_pl = 0usize;
    let mut best_k = 0usize;
    // 存每条带的段起止（每带最多 w/2 段）。
    let mut segs_per_band: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    let mut pl_per_band = vec![0usize; n];

    for k in 0..n {
        let ia = k * da;
        let mut segs: Vec<(usize, usize)> = Vec::new();
        let mut pl = 0usize;
        let mut in_seg = false;
        let mut seg_start = 0usize;
        let mut seg_end = 0usize;
        for x in 0..w {
            // 该列在条带内是否有白像素（原代码：`bln` 段内 `g_le=x`；pl 统计白像素个数）。
            let mut any_white = false;
            for y in 0..segh {
                if frame[[ia / w + y, x]] == 255 {
                    // 注意：ImRes 是行优先，i = ia + x + y*w，对应 (ia/w + y, x)。
                    // 原代码 pl++ 在 y 循环内，统计的是白**像素**数（用于 mpl 排序），这里对齐。
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
        // 原代码逻辑：段结束条件是 `g_le[k][l] != x` 且 bln==1；这里以「白列结束」为段结束。
        // 但原代码在 `g_le == x`（列 x 无新白）时才结束，语义接近。为对齐，段按白列连续分组即可。
        // 注意原代码 `if (bln==1) if (g_le[k][l] != x) { bln=0; l++; }` —— 段在「本列无白但上一列有」时闭合。
        segs_per_band[k] = segs;
        pl_per_band[k] = pl;
        if pl > best_pl {
            best_pl = pl;
            best_k = k;
        }
    }

    if best_pl == 0 {
        return false;
    }

    // 用密度最大带判断。
    let k = best_k;
    // 对齐裁剪会原地改段列表，故取可变的拷贝（原代码对 g_lb/g_le 做 shift）。
    let mut segs: Vec<(usize, usize)> = segs_per_band[k].clone();
    if segs.is_empty() {
        return false;
    }
    let mut len: usize = segs.iter().map(|&(lb, le)| le - lb + 1).sum();
    let mut l = segs.len() - 1;

    // 对齐裁剪（C++ `g_text_alignment` 默认 **Center**，非 Any！）。
    // ⚠️ 之前硬编码 Any 跳过 Center 的段裁剪 → 保留偏离中心噪声段 → has_text 误判。
    let align = TextAlignment::Center;
    if align != TextAlignment::Any {
        while l > 0 {
            if len < mtl {
                return false;
            }
            let len2 = segs[l].1 - segs[0].0 + 1;
            if (len as f32) / (len2 as f32) >= tp {
                return true;
            }
            match align {
                TextAlignment::Center => {
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
                        // 移除第一段（对齐原代码对 g_lb/g_le 的 shift）。
                        segs.remove(0);
                    }
                }
                TextAlignment::Left => {
                    len -= segs[l].1 - segs[l].0 + 1;
                }
                _ => {
                    len -= segs[0].1 - segs[0].0 + 1;
                    segs.remove(0);
                }
            }
            l -= 1;
        }
    }

    if len > mtl {
        if (segs[0].0 * 2 < w) || align != TextAlignment::Center {
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
    fn analyse_detects_wide_text_row() {
        // 一条横跨大部分宽度的白行（segh=3，取一整个条带为白），宽度远超 mtl。
        let (h, w) = (9, 200);
        let mut frame = Array2::<u8>::zeros((h, w));
        // 第 3-5 行（一个条带）大部分白，模拟一行字幕。
        for y in 3..6 {
            for x in 4..196 {
                frame[[y, x]] = 255;
            }
        }
        assert!(analyse_image(&frame, &params()), "应有文字");
    }

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
