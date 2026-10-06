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
use std::collections::HashMap;

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

/// 「卡片框(非白)」检测阈值: 白色画布/脏白 (min >= 245) 判为「白」剔除,
/// 其下 (彩色卡片背景、暗内容、薄浅色抗锯齿边) 都算「卡片体」。
/// 对「带彩色背景、以白分隔」的漫画/UI, 用此阈值切出**整张卡片**(0 白边),
/// 避免 content 检测 (195) 把浅色背景误当「缝」而把一张卡打碎成多块。
pub const FRAME_THRESHOLD: u8 = 245;

impl Mode {
    /// CLI / 文件名标识 ("auto" / "sprite" / "free")。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Sprite => "sprite",
            Mode::Free => "free",
        }
    }
}

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

/// 2D 连通域切出的卡片: 包围盒 + 形状掩膜 (alpha 裁切用).
#[derive(Debug, Clone)]
pub struct Panel {
    /// 包围盒 (x, y, w, h).
    pub bbox: (u32, u32, u32, u32),
    /// 形状掩膜, 长 `w*h` (行主序), 255=卡片内(不透明), 0=卡片外(透明).
    pub mask: Vec<u8>,
    /// 卡片实心区域面积 (像素数).
    pub area: u32,
}

/// 8-邻接连通域标记. `fg` 为 0/1 掩膜 (行主序, 长 w*h).
/// 返回 (每像素标签 (0=非前景), 每标签像素数 (下标=标签, 0 恒 0)).
fn label_cc(fg: &[u8], w: usize, h: usize) -> (Vec<i32>, Vec<usize>) {
    let n = w * h;
    let mut label = vec![0i32; n];
    let mut counts: Vec<usize> = vec![0];
    let mut next = 1i32;
    let mut stack = Vec::with_capacity(1024);
    for i in 0..n {
        if fg[i] == 0 || label[i] != 0 {
            continue;
        }
        label[i] = next;
        stack.push(i);
        let mut count = 0usize;
        while let Some(p) = stack.pop() {
            count += 1;
            let (py, px) = (p / w, p % w);
            for dy in -1isize..=1 {
                for dx in -1isize..=1 {
                    if dy == 0 && dx == 0 {
                        continue;
                    }
                    let ny = py as isize + dy;
                    let nx = px as isize + dx;
                    if ny < 0 || nx < 0 || ny >= h as isize || nx >= w as isize {
                        continue;
                    }
                    let ni = (ny as usize) * w + nx as usize;
                    if fg[ni] != 0 && label[ni] == 0 {
                        label[ni] = next;
                        stack.push(ni);
                    }
                }
            }
        }
        counts.push(count);
        next += 1;
    }
    (label, counts)
}



/// 对二值掩膜 (255=实心, 0=空) 做形态学膨胀 (N 像素).
/// 用于桥接卡片间的细窄白缝/白布, 使断裂的连通域合并.
fn dilate_mask(mask: &mut [u8], w: usize, h: usize, px: u32) {
    if px == 0 {
        return;
    }
    for _ in 0..px {
        let mut new_mask = vec![0u8; mask.len()];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let mut solid = mask[i] == 255;
                if !solid {
                    for dy in -1isize..=1 {
                        for dx in -1isize..=1 {
                            if dy == 0 && dx == 0 {
                                continue;
                            }
                            let ny = y as isize + dy;
                            let nx = x as isize + dx;
                            if ny >= 0 && nx >= 0 && ny < h as isize && nx < w as isize
                                && mask[(ny as usize) * w + (nx as usize)] == 255
                            {
                                solid = true;
                                break;
                            }
                        }
                        if solid {
                            break;
                        }
                    }
                }
                new_mask[i] = solid as u8 * 255;
            }
        }
        mask.copy_from_slice(&new_mask);
    }
}

/// 对二值掩膜 (255=实心, 0=空) 做形态学腐蚀 (N 像素).
/// 用于裁掉卡片边缘的反锯齿/亮晕边, 实现「0 白边」的紧致贴纸裁切.
fn erode_mask(mask: &mut [u8], w: usize, h: usize, px: u32) {
    if px == 0 {
        return;
    }
    for _ in 0..px {
        let mut new_mask = vec![0u8; mask.len()];
        for y in 1..h.saturating_sub(1) {
            for x in 1..w.saturating_sub(1) {
                let i = y * w + x;
                let mut solid = mask[i] == 255;
                if solid {
                    for dy in -1isize..=1 {
                        for dx in -1isize..=1 {
                            if dy == 0 && dx == 0 {
                                continue;
                            }
                            let ny = y as isize + dy;
                            let nx = x as isize + dx;
                            if ny < 0 || nx < 0 || ny >= h as isize || nx >= w as isize
                                || mask[(ny as usize) * w + (nx as usize)] == 0
                            {
                                solid = false;
                                break;
                            }
                        }
                        if !solid {
                            break;
                        }
                    }
                }
                new_mask[i] = solid as u8 * 255;
            }
        }
        mask.copy_from_slice(&new_mask);
    }
}

/// 2D 连通域切卡片: 对「非白」(每像素最暗通道 < `threshold`) 掩膜做 8-邻接连通域,
/// 每张卡片当一个「实心区域」——**填掉内部白洞** (白色对话框等; 判内外靠
/// "触图像边界的白 CC = 外白布, 其余白 = 内部洞"), 得到跟随卡片实际轮廓
/// (圆角/抖动) 的形状. 返回各卡片, 按上→下、左→右排序.
///
/// 与 1D [`split_panels`] 不同: 这里卡片是**连通域** (2D), 亮背景不再把一张卡
/// 打碎; 形状 mask 供 alpha 裁切, 卡片外透明 (0 白边 / 贴纸式).
///
/// `min_area` 过滤噪点连通域 (如卡间白缝里的暗噪点), 单位像素.
pub fn split_panels_cc(img: &RgbImage, threshold: u8, min_area: u32, bridge_px: u32, erode_px: u32) -> Vec<Panel> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let n = w * h;
    // 每像素最暗通道
    let dark: Vec<u8> = img.pixels().map(|p| *p.0.iter().min().unwrap()).collect();
    // 白色掩膜 (最暗 >= threshold = 白布/白缝)
    let white: Vec<u8> = dark.iter().map(|&v| (v >= threshold) as u8).collect();
    // 白色 CC: 触图像边界的白 CC = 外白布 (卡间白缝经边缘白布连通, 也是外)
    let (wl, _wcounts) = label_cc(&white, w, h);
    let maxl = wl.iter().max().copied().unwrap_or(0) as usize;
    let mut exterior = vec![false; maxl + 1];
    let mut mark = |l: i32| {
        if l > 0 {
            exterior[l as usize] = true;
        }
    };
    for x in 0..w {
        mark(wl[x]);
    } // 顶边
    let b = (h - 1) * w;
    for x in 0..w {
        mark(wl[b + x]);
    } // 底边
    for y in 0..h {
        mark(wl[y * w]);
    } // 左边
    for y in 0..h {
        mark(wl[y * w + (w - 1)]);
    } // 右边
    // 卡片掩膜 = 非「外白布」 (含卡片本体 + 内部白洞, 如白色对话框)
    let card: Vec<u8> = (0..n)
        .map(|i| {
            let l = wl[i];
            if l == 0 || !exterior[l as usize] {
                255
            } else {
                0
            }
        })
        .collect();
    // 形态学闭运算: 膨胀桥接白缝, 再腐蚀恢复边缘 (此处仅膨胀, 腐蚀在单卡 mask 上做)
    let mut bridge_mask = card.clone();
    dilate_mask(&mut bridge_mask, w, h, bridge_px);
    // 卡片 CC (在膨胀后的掩膜上, 使断裂的卡片合并)
    let (cl, ccounts) = label_cc(&bridge_mask, w, h);
    let cmax = cl.iter().max().copied().unwrap_or(0);
    let min_area = min_area as usize;
    let mut panels: Vec<Panel> = Vec::new();
    for l in 1..=cmax {
        let area = ccounts[l as usize];
        if area < min_area {
            continue;
        }
        // 该 CC 的包围盒
        let mut x0 = w;
        let mut y0 = h;
        let mut x1 = 0usize;
        let mut y1 = 0usize;
        for i in 0..n {
            if cl[i] == l {
                let (x, y) = (i % w, i / w);
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
        let mut mask = vec![0u8; bw * bh];
        for dy in 0..bh {
            for dx in 0..bw {
                let i = (y0 + dy) * w + (x0 + dx);
                if cl[i] == l {
                    mask[dy * bw + dx] = 255;
                }
            }
        }
        // 腐蚀掉膨胀桥接的部分 + 额外的白边/晕边
        erode_mask(&mut mask, bw, bh, bridge_px + erode_px);
        panels.push(Panel {
            bbox: (x0 as u32, y0 as u32, bw as u32, bh as u32),
            mask,
            area: area as u32,
        });
    }
    panels.sort_by_key(|p| (p.bbox.1, p.bbox.0));
    panels
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
///
/// 返回每组的「实际生效模式」`(源图宽, mode)`：未做统一（可信格不足 2、
/// auto 极差判定不达标、显式 free）的组都记为 [`Mode::Free`]，即各格保留
/// 独立收缩结果。
pub fn unify_sizes(
    results: &mut [(String, u32, u32, Vec<(u32, u32, u32, u32)>)],
    mode: Mode,
) -> HashMap<u32, Mode> {
    let mut groups: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, (_, w, _, _)) in results.iter().enumerate() {
        groups.entry(*w).or_default().push(i);
    }

    let mut resolved: HashMap<u32, Mode> = HashMap::new();
    for (source_w, idxs) in groups {
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
            resolved.insert(source_w, Mode::Free);
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
                resolved.insert(source_w, Mode::Free);
                continue;
            }
        }
        if mode == Mode::Free {
            resolved.insert(source_w, Mode::Free);
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
        resolved.insert(source_w, Mode::Sprite);
    }
    resolved
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
