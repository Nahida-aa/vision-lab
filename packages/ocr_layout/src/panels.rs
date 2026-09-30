//! 漫画长图 / 精灵图切分：白底长图 → 紧致内容块（纯图片构成的 UI）。
//!
//! 每一块对应一条**无文字信号的 [`Widget`]**（source=Color）。算法三层：
//!
//! 1. **行投影切行**（[`split_panels`]）：`row_dark[y]` = 该行最暗像素亮度；
//!    整行无内容的连续行（亮度 >= `threshold`）达到 `min_gap` 视为格间分隔带，
//!    带间即行块。不做网格整齐排列假设。
//!
//! 2. **四边收缩**：行块边缘常有振铃/浅灰渐变白边（min 198–253 不等）。从每
//!    条边向内收缩「整行/列 min >= threshold」的浅色边缘。阈值 195 对这类
//!    漫画恰好分离白边/画布 (min >= 195) 与卡片内容 (min <= 180，含灰蓝背景)。
//!    渐变过渡不需要「突变」：只要内容首行/列比白边深，收缩就会停在那里。
//!
//!    历史教训（都已实测）：梯度跳变检测在渐变边界上时灵时不灵（右侧白边
//!    整段漏裁）；逐像素占比判据同因（渐变中相邻像素差不足）；顶/底紧致会
//!    被画面内斜线（桌沿）拖尾误裁 37px——行块边界由分隔带给出后，这些
//!    花活全部不需要。
//!
//! 3. **组内一致性**（[`unify_sizes`]）：同一部漫画的卡片理论上等大，但各格
//!    收缩后的尺寸有 ±几 px 噪声（渐变带内边缘位置的抖动），会漂移出
//!    872/868/865 这种渐小尺寸。sprite 模式统一到组内**最紧**（left=max、
//!    x1=min、height=min）——零白边保证；多裁的 1-4px 是边缘渐变带，无
//!    视觉内容。free 模式逐格保留（自由拼图不等大）。
//!
//!    参与格 = 边界可信的格子：收缩后 `x0 > 0` 且 `x1 < w` 且 `y1 < h`。
//!    贴源图边的格子（卡片超出源图、全宽到底）边界不可信，不参与统计也
//!    保持原样——统一它们会裁掉真实内容。

use crate::Widget;
use image::RgbImage;

/// 拼接布局模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// 自动：按组内边界极差判定（≤ [`SPRITE_SPREAD_MAX`] 视为 [`Mode::Sprite`]）。
    #[default]
    Auto,
    /// 精灵图：卡片等大，组内统一到最紧共同边界（零白边）。
    Sprite,
    /// 自由拼图：逐格独立紧致，不等大。
    Free,
}

/// 自动判定的边界极差上限（px）：组内参与格的 left / x1 / 高度极差都不超过
/// 此值时视为「等大卡片」（[`Mode::Sprite`]），否则视为自由布局。
pub const SPRITE_SPREAD_MAX: u32 = 12;

/// 切分单张长图：白底上按行排布的卡片 → 各卡片的紧致包围盒 `(x, y, w, h)`。
///
/// `threshold` 是「浅色」判定线：整行/列的最暗像素亮度 >= 此值视为无内容。
/// 对 JPEG 脏白/浅灰渐变的漫画长图，195 左右是经验起点（见模块文档）。
pub fn split_panels(img: &RgbImage, threshold: u8, min_gap: u32) -> Vec<(u32, u32, u32, u32)> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    // 每像素最暗通道值：分隔带检测与边缘收缩的聚合信号。
    let darkness: Vec<u8> = img
        .pixels()
        .map(|p| *p.0.iter().min().unwrap())
        .collect();

    // 行投影 → 行块。
    let row_dark: Vec<u8> = (0..h)
        .map(|y| (0..w).map(|x| darkness[y * w + x]).min().unwrap())
        .collect();

    let mut rects = Vec::new();
    for (y0, y1) in bright_blocks(&row_dark, threshold, min_gap) {
        // 四边收缩：从每条边向内跳过「整行/列 min >= threshold」的浅色边缘，
        // 停在第一个内容行/列（阈值语义，渐变过渡也能正确定位）。
        let y0t = (y0..y1).find(|&y| row_dark[y] < threshold).unwrap_or(y0);
        let y1t = (y0..y1)
            .rev()
            .find(|&y| row_dark[y] < threshold)
            .map(|y| y + 1)
            .unwrap_or(y0t);
        if y1t <= y0t {
            continue;
        }

        let col_dark: Vec<u8> = (0..w)
            .map(|x| (y0t..y1t).map(|y| darkness[y * w + x]).min().unwrap())
            .collect();
        let x0t = (0..w).find(|&x| col_dark[x] < threshold).unwrap_or(0);
        let x1t = (0..w)
            .rev()
            .find(|&x| col_dark[x] < threshold)
            .map(|x| x + 1)
            .unwrap_or(x0t);
        if x1t <= x0t {
            continue;
        }

        rects.push((
            x0t as u32,
            y0t as u32,
            (x1t - x0t) as u32,
            (y1t - y0t) as u32,
        ));
    }
    rects
}

/// 从最暗值序列找内容区间：连续亮（>= 阈值）且长度 >= `min_gap` 的段为分隔，
/// 分隔之间的部分即内容块。
fn bright_blocks(dark: &[u8], threshold: u8, min_gap: u32) -> Vec<(usize, usize)> {
    let min_gap = min_gap as usize;
    let mut gaps: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &v) in dark.iter().enumerate() {
        if v >= threshold {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            if i - s >= min_gap {
                gaps.push((s, i));
            }
        }
    }
    if let Some(s) = start
        && dark.len() - s >= min_gap
    {
        gaps.push((s, dark.len()));
    }

    let mut blocks = Vec::new();
    let mut prev = 0usize;
    for (a, b) in gaps {
        if a > prev {
            blocks.push((prev, a));
        }
        prev = b;
    }
    if dark.len() > prev {
        blocks.push((prev, dark.len()));
    }
    blocks
}

/// 组内尺寸一致性：同一批卡片按源图宽度分组（同宽视为同部漫画/同裁切），
/// sprite 模式统一到组内最紧边界，free 模式跳过，auto 先判极差。
///
/// `results` 的元素为 `(来源标识, 源图宽, 源图高, 该图的卡片 rect 列表)`；
/// 来源标识只用于 auto 判定不达标时的日志。
pub fn unify_sizes(
    results: &mut [(String, u32, u32, Vec<(u32, u32, u32, u32)>)],
    mode: Mode,
) {
    use std::collections::HashMap;

    let mut groups: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, (_, w, _, _)) in results.iter().enumerate() {
        groups.entry(*w).or_default().push(i);
    }

    for (_source_w, idxs) in groups {
        let mut lefts: Vec<u32> = Vec::new();
        let mut x1s: Vec<u32> = Vec::new();
        let mut heights: Vec<u32> = Vec::new();
        for &i in &idxs {
            let (_, w, h, rects) = &results[i];
            for r in rects {
                let (x0, y0, rw, rh) = *r;
                if x0 == 0 || x0 + rw == *w || y0 + rh == *h {
                    continue; // 贴边格：卡片被源图裁断，边界不可信
                }
                lefts.push(x0);
                x1s.push(x0 + rw);
                heights.push(rh);
            }
        }
        if lefts.len() < 2 {
            continue; // 可信格不足 2 个，无一致性可言
        }

        // Auto 判据：三个维度的极差都在限内 → 等大卡片（精灵图）。
        if mode == Mode::Auto {
            let spread = |v: &[u32]| v.iter().max().unwrap() - v.iter().min().unwrap();
            let is_sprite = spread(&lefts) <= SPRITE_SPREAD_MAX
                && spread(&x1s) <= SPRITE_SPREAD_MAX
                && spread(&heights) <= SPRITE_SPREAD_MAX;
            if !is_sprite {
                // auto 判定不达标是用户需要知道的行为切换, 直接打到 stderr
                // (与本 example 其余 [split]/[mode] 日志同风格)
                eprintln!("[mode] auto → free (边界极差超 {SPRITE_SPREAD_MAX}px)");
                continue;
            }
        }
        if mode == Mode::Free {
            continue;
        }

        // Sprite：统一到组内最紧（零白边；多裁的是边缘渐变带）。
        let ul = *lefts.iter().max().unwrap();
        let ux1 = *x1s.iter().min().unwrap();
        let uh = *heights.iter().min().unwrap();
        for &i in &idxs {
            let (_, w, h, rects) = &mut results[i];
            for r in rects.iter_mut() {
                if r.0 == 0 || r.0 + r.2 == *w || r.1 + r.3 == *h {
                    continue; // 特殊格（贴源图边）保持原样
                }
                *r = (ul, r.1, ux1 - ul, uh);
            }
        }
    }
}

/// 把切分结果包装成无文字信号的 Widget（source=Color），可直接交给
/// [`crate::annotate`] 画预览框。
pub fn rects_to_widgets(
    rects: &[(u32, u32, u32, u32)],
    img_area: u32,
) -> Vec<Widget> {
    rects
        .iter()
        .enumerate()
        .map(|(idx, r)| Widget {
            id: idx,
            label: String::new(),
            rect: *r,
            color: [255, 0, 0],
            area_ratio: (r.2 * r.3) as f32 / img_area as f32,
            source: crate::WidgetSource::Color,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    /// 合成白底长图 + 若干深色卡片（模拟白底分隔的漫画格）。
    fn synth(cards: &[(u32, u32, u32, u32)], w: u32, h: u32) -> RgbImage {
        let mut img = RgbImage::new(w, h);
        for p in img.pixels_mut() {
            *p = Rgb([255, 255, 255]);
        }
        for &(x0, y0, cw, ch) in cards {
            for y in y0..y0 + ch {
                for x in x0..x0 + cw {
                    *img.get_pixel_mut(x, y) = Rgb([40, 40, 40]);
                }
            }
        }
        img
    }

    #[test]
    fn splits_cards_and_shrinks_to_content() {
        // 两张卡片, 卡片外围各留一圈浅灰渐变 (min 220) —— 应被收缩裁掉。
        // 注意顺序: 渐变带先画, 卡片本体后画 (渐变是外圈, 覆盖会吃掉本体)。
        let mut img = synth(&[], 40, 50);
        for r in [(8, 3, 24, 14), (8, 28, 24, 14)] {
            let (x0, y0, rw, rh) = r;
            for y in y0..y0 + rh {
                for x in x0..x0 + rw {
                    *img.get_pixel_mut(x, y) = Rgb([220, 220, 220]);
                }
            }
        }
        let img = synth_on(&img, &[(10, 5, 20, 10), (10, 30, 20, 10)]);
        let rects = split_panels(&img, 195, 2);
        assert_eq!(rects, vec![(10, 5, 20, 10), (10, 30, 20, 10)]);
    }

    /// 在已有图上叠加深色卡片。
    fn synth_on(base: &RgbImage, cards: &[(u32, u32, u32, u32)]) -> RgbImage {
        let mut img = base.clone();
        for &(x0, y0, cw, ch) in cards {
            for y in y0..y0 + ch {
                for x in x0..x0 + cw {
                    *img.get_pixel_mut(x, y) = Rgb([40, 40, 40]);
                }
            }
        }
        img
    }

    #[test]
    fn bright_blocks_finds_content_between_gaps() {
        // 白带(>=195)至少 2 行才算分隔
        let dark = [255, 255, 0, 0, 255, 255, 30, 255, 255];
        assert_eq!(bright_blocks(&dark, 195, 2), vec![(2, 4), (6, 7)]);
    }

    #[test]
    fn unify_takes_tightest_bounds() {
        let mut results = vec![
            ("a".into(), 200, 100, vec![(9, 0, 91, 50)]),
            ("b".into(), 200, 100, vec![(11, 0, 87, 48)]),
        ];
        unify_sizes(&mut results, Mode::Sprite);
        // left=max(9,11)=11, x1=min(100,98)=98, h=min(50,48)=48
        assert_eq!(results[0].3[0], (11, 0, 87, 48));
        assert_eq!(results[1].3[0], (11, 0, 87, 48));
    }

    #[test]
    fn unify_auto_falls_back_to_free_on_large_spread() {
        // left 极差 40 > SPRITE_SPREAD_MAX → 自由布局, 不统一
        let mut results = vec![
            ("a".into(), 200, 100, vec![(9, 0, 91, 50)]),
            ("b".into(), 200, 100, vec![(49, 0, 51, 50)]),
        ];
        unify_sizes(&mut results, Mode::Auto);
        assert_eq!(results[0].3[0], (9, 0, 91, 50));
        assert_eq!(results[1].3[0], (49, 0, 51, 50));
    }

    #[test]
    fn unify_skips_edge_touching_rects() {
        // x0=0 的格子: 卡片被源图裁断, 边界不可信 → 不参与也不被改
        let mut results = vec![
            ("a".into(), 200, 100, vec![(0, 0, 100, 50)]),
            ("b".into(), 200, 100, vec![(9, 0, 91, 50)]),
            ("c".into(), 200, 100, vec![(11, 0, 87, 48)]),
        ];
        unify_sizes(&mut results, Mode::Sprite);
        assert_eq!(results[0].3[0], (0, 0, 100, 50), "贴边格保持原样");
        // 另两格统一到彼此最紧
        assert_eq!(results[1].3[0], (11, 0, 87, 48));
        assert_eq!(results[2].3[0], (11, 0, 87, 48));
    }

    #[test]
    fn free_mode_keeps_per_panel_bounds() {
        let mut results = vec![
            ("a".into(), 200, 100, vec![(9, 0, 91, 50)]),
            ("b".into(), 200, 100, vec![(11, 0, 87, 48)]),
        ];
        unify_sizes(&mut results, Mode::Free);
        assert_eq!(results[0].3[0], (9, 0, 91, 50));
        assert_eq!(results[1].3[0], (11, 0, 87, 48));
    }

    #[test]
    fn rects_to_widgets_marks_color_source() {
        let widgets = rects_to_widgets(&[(1, 2, 30, 40)], 100 * 100);
        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].rect, (1, 2, 30, 40));
        assert_eq!(widgets[0].source, crate::WidgetSource::Color);
        assert!(widgets[0].label.is_empty());
    }
}
