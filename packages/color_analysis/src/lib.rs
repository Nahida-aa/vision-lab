//! 图像颜色分析原语（与文字、控件、业务无关）。
//!
//! 这一层只回答「图里有哪些颜色连贯的区域」「主色是什么」「某区域和周围对比
//! 度如何」这类**纯视觉统计**问题。它不认识「按钮」「控件」「文字」，那些语义
//! 由上层（`ocr_layout` 等）在消费本层输出后赋予。
//!
//! 设计原则：
//! - 纯函数式 API：输入图像，输出颜色区域，不持状态、不依赖 OpenCV。
//! - 可配置：量化粒度、相似度阈值、最小面积等都由调用方传入。
//! - 无业务语义：公开类型只谈 `color` / `region` / `segment` / `contrast`。

use image::RgbImage;
use std::collections::HashMap;

/// 一个颜色连贯区域（连通域）的统计结果。
///
/// 不含任何「控件 / 按钮」语义 —— 只是「这块区域颜色差不多，边界在此」。
#[derive(Clone, Debug)]
pub struct ColorRegion {
    /// 该区域的主导色（区域内原始像素的平均色）。
    pub color: [u8; 3],
    /// 包围盒 `(x, y, w, h)`（像素，原点左上）。
    pub rect: (u32, u32, u32, u32),
    /// 区域像素数。
    pub pixel_count: usize,
}

/// `segment_by_color` 的可配置参数。
#[derive(Clone, Copy, Debug)]
pub struct SegmentOpts {
    /// 量化步长：每个通道按 `1 << (8 - bits)` 合并。bits 越大越细（默认 4 →
    /// 每通道 16 级）。值越小区域越合并、越大越碎。
    pub quant_bits: u8,
    /// 连通判定用的颜色距离阈值（0–441，三维欧氏距离上限）。相邻像素量化后仍
    /// 在此距离内视为同色。默认 6 —— 这是为「找 UI 元素」调过的预设：真实
    /// 界面里按钮 / 文字 / 底色往往落在同一个窄色带（色距普遍 < 12），阈值
    /// 若 ≥12 会把整窗误并成一个区域；压到 6 才能把它们拆开。真·高对比色块
    /// （色距 >200）依旧会被正确分成多块，不会过度合并。
    pub merge_distance: u32,
    /// 最小区域面积占全图比例（0–1）。小于此的区域视为噪声丢弃。默认 0.0005
    /// （约 493×378 图上 93 像素），可滤掉抗锯齿 / 文字笔画碎片，保留「大块」
    /// 元素。高对比场景可降到 0.0001 看全部分量。
    pub min_area_ratio: f32,
}

impl Default for SegmentOpts {
    fn default() -> Self {
        Self {
            quant_bits: 4,
            merge_distance: 6,
            min_area_ratio: 0.0005,
        }
    }
}

/// 把单个像素按 `quant_bits` 量化到粗色空间（降维，便于连通判定）。
fn quantize(p: [u8; 3], bits: u8) -> [u8; 3] {
    let shift = 8u32.saturating_sub(bits as u32) as u8;
    [
        p[0] >> shift << shift,
        p[1] >> shift << shift,
        p[2] >> shift << shift,
    ]
}

/// 三维颜色欧氏距离的平方（避免开方，比较时用平方）。
fn color_dist2(a: [u8; 3], b: [u8; 3]) -> u32 {
    let dr = a[0] as i32 - b[0] as i32;
    let dg = a[1] as i32 - b[1] as i32;
    let db = a[2] as i32 - b[2] as i32;
    (dr * dr + dg * dg + db * db) as u32
}

/// 按颜色连通性把图像分割成若干区域。
///
/// 流程：逐像素量化 → 对量化色做「近色合并」形成合并标签 → 在合并标签图上做
/// 4-邻域连通分量 → 过滤过小区域 → 输出每个区域的包围盒与平均原色。
///
/// 这是「靠颜色区分元素」的基础原语：UI 中功能不同的相邻元素通常颜色不同，
/// 因此一个颜色连通域往往对应一个视觉上独立的 UI 元素边界（但本函数不对此
/// 下结论，只给区域）。
pub fn segment_by_color(img: &RgbImage, opts: SegmentOpts) -> Vec<ColorRegion> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let n = (w * h) as usize;
    let min_pixels = (n as f32 * opts.min_area_ratio).max(1.0) as usize;

    // 1. 量化每个像素。
    let quantized: Vec<[u8; 3]> = img
        .pixels()
        .map(|p| quantize([p.0[0], p.0[1], p.0[2]], opts.quant_bits))
        .collect();

    // 2. 对量化色做近色合并：把两两距离 <= merge_distance 的量化色归并到同一
    //    代表色（并查集）。先收集出现过的量化色。
    let mut present: Vec<[u8; 3]> = quantized.iter().copied().collect();
    present.sort_unstable();
    present.dedup();
    let merge_thresh2 = opts.merge_distance * opts.merge_distance;
    let mut parent: Vec<usize> = (0..present.len()).collect();
    let find = |parent: &mut Vec<usize>, mut x: usize| -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    };
    let union = |parent: &mut Vec<usize>, a: usize, b: usize| {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[ra] = rb;
        }
    };
    for i in 0..present.len() {
        for j in (i + 1)..present.len() {
            if color_dist2(present[i], present[j]) <= merge_thresh2 {
                union(&mut parent, i, j);
            }
        }
    }
    // 量化色 → 合并代表索引。
    let label_of: HashMap<[u8; 3], usize> = present
        .iter()
        .enumerate()
        .map(|(i, c)| (*c, find(&mut parent, i)))
        .collect();

    // 3. 合并标签图上做 4-邻域连通分量。
    let mut merged_label = vec![usize::MAX; n];
    let mut comp_id = 0usize;
    let mut comps: Vec<(u32, u32, u32, u32, u64, u64, u64)> = Vec::new();
    // 累加器：minx, miny, maxx, maxy, sum_r, sum_g, sum_b
    let mut visited = vec![false; n];
    for y0 in 0..h {
        for x0 in 0..w {
            let idx0 = (y0 * w + x0) as usize;
            if visited[idx0] {
                continue;
            }
            let lab = *label_of.get(&quantized[idx0]).unwrap();
            // BFS
            let mut stack = vec![(y0, x0)];
            visited[idx0] = true;
            let (mut minx, mut miny, mut maxx, mut maxy) = (x0, y0, x0, y0);
            let (mut sr, mut sg, mut sb) = (0u64, 0u64, 0u64);
            while let Some((y, x)) = stack.pop() {
                let idx = (y * w + x) as usize;
                merged_label[idx] = comp_id;
                minx = minx.min(x);
                miny = miny.min(y);
                maxx = maxx.max(x);
                maxy = maxy.max(y);
                let p = img.get_pixel(x, y).0;
                sr += p[0] as u64;
                sg += p[1] as u64;
                sb += p[2] as u64;
                for (dy, dx) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let ny = y as i32 + dy;
                    let nx = x as i32 + dx;
                    if ny < 0 || nx < 0 || ny >= h as i32 || nx >= w as i32 {
                        continue;
                    }
                    let nidx = (ny as u32 * w + nx as u32) as usize;
                    if visited[nidx] {
                        continue;
                    }
                    if *label_of.get(&quantized[nidx]).unwrap() == lab {
                        visited[nidx] = true;
                        stack.push((ny as u32, nx as u32));
                    }
                }
            }
            comps.push((minx, miny, maxx, maxy, sr, sg, sb));
            comp_id += 1;
        }
    }

    // 4. 过滤小区域 + 组装结果。
    let mut out = Vec::new();
    for (minx, miny, maxx, maxy, sr, sg, sb) in comps {
        let bw = (maxx - minx + 1) as usize;
        let bh = (maxy - miny + 1) as usize;
        let area = bw * bh;
        if area < min_pixels {
            continue;
        }
        let count = area as u64;
        out.push(ColorRegion {
            color: [(sr / count) as u8, (sg / count) as u8, (sb / count) as u8],
            rect: (minx, miny, bw as u32, bh as u32),
            pixel_count: area,
        });
    }
    out
}

/// 提取图像主色板（按像素数排序的前 `k` 个颜色连通区域的代表色）。
///
/// 是 `segment_by_color` 的便捷视图：用默认参数分割后，按面积取前 `k` 个代表色。
pub fn dominant_colors(img: &RgbImage, k: usize) -> Vec<[u8; 3]> {
    let mut regions = segment_by_color(img, SegmentOpts::default());
    regions.sort_by(|a, b| b.pixel_count.cmp(&a.pixel_count));
    regions.into_iter().take(k).map(|r| r.color).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn solid(w: u32, h: u32, c: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb(c))
    }

    #[test]
    fn single_color_is_one_region() {
        let img = solid(20, 10, [100, 150, 200]);
        let regs = segment_by_color(&img, SegmentOpts::default());
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].color, [100, 150, 200]);
        assert_eq!(regs[0].rect, (0, 0, 20, 10));
    }

    #[test]
    fn two_distinct_blocks_two_regions() {
        // 左半红、右半蓝 → 两个区域。
        let mut img = solid(20, 10, [0, 0, 0]);
        for y in 0..10u32 {
            for x in 0..10u32 {
                img.put_pixel(x, y, Rgb([255, 0, 0]));
            }
            for x in 10..20u32 {
                img.put_pixel(x, y, Rgb([0, 0, 255]));
            }
        }
        let regs = segment_by_color(&img, SegmentOpts::default());
        assert_eq!(regs.len(), 2, "应分割成左右两个颜色区域");
        let colors: Vec<[u8; 3]> = regs.iter().map(|r| r.color).collect();
        assert!(colors.contains(&[255, 0, 0]));
        assert!(colors.contains(&[0, 0, 255]));
    }

    #[test]
    fn small_noise_filtered() {
        // 大片底色 + 一个 2x2 的噪点（远小于 min_area_ratio）。
        let mut img = solid(100, 100, [200, 200, 200]);
        img.put_pixel(50, 50, Rgb([10, 10, 10]));
        img.put_pixel(51, 50, Rgb([10, 10, 10]));
        img.put_pixel(50, 51, Rgb([10, 10, 10]));
        img.put_pixel(51, 51, Rgb([10, 10, 10]));
        let regs = segment_by_color(&img, SegmentOpts::default());
        // 噪点 4 像素 < min_pixels（10000*0.002=20），应被过滤，只剩底色。
        assert_eq!(regs.len(), 1, "噪点区域应被过滤，只剩一个底色区域");
        // 底色区域的平均色仍应约等于 [200,200,200]（噪点占比极小，允许 ±2 容差）。
        let c = regs[0].color;
        assert!(
            (c[0] as i32 - 200).abs() <= 2
                && (c[1] as i32 - 200).abs() <= 2
                && (c[2] as i32 - 200).abs() <= 2,
            "底色平均色应接近 [200,200,200]，实际 {c:?}"
        );
    }

    #[test]
    fn dominant_colors_orders_by_area() {
        let mut img = solid(20, 10, [0, 0, 0]);
        for y in 0..10u32 {
            for x in 0..4u32 {
                img.put_pixel(x, y, Rgb([255, 255, 255])); // 小色块
            }
        }
        let top = dominant_colors(&img, 1);
        assert_eq!(top, vec![[0, 0, 0]]); // 大面积底色排第一
    }
}
