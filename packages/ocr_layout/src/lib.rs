//! 把「颜色区域」与「文字框」合并成 UI 控件矩形。
//!
//! 这是介于纯感知（`color_analysis` / `rapidocr_ort`）与业务执行之间的**布局层**：
//! 不认识「按钮」「输入框」的语义，只产出「这里有一块视觉上独立的区域，里面有
//! 这些文字」的 [`Widget`] 候选，供上层（如 `ocr_agent`）赋予语义并操作。
//!
//! 信号优先级（按之前讨论）：
//! 1. **颜色连通域为主**：UI 中功能不同的相邻元素通常颜色不同，一个颜色连通域
//!    往往对应一个视觉独立的元素边界。用 `color_analysis::segment_by_color`。
//! 2. **文字框为辅**：`rapidocr` 给出文字内容与几何中心，用来给颜色区域贴标签，
//!    并补回「颜色相同但文字不同」的元素（如两个同色按钮靠文字区分）。
//!
//! 输出 [`Widget`]：每个候选控件的包围盒、代表色、面积占比、关联文字、以及它
//! 主要来自哪种信号（`source`）。

pub mod panels;

// 重导出：纯图片 UI（漫画格/精灵图）的切分能力，见 [`panels`]。
pub use panels::{Mode, split_panels, unify_sizes};

use anyhow::Result;
use color_analysis::{SegmentOpts, segment_by_color};
use image::RgbImage;
use ndarray::Array3;
use rapidocr_ort::{ModelProfile, OcrBoxResult, OcrEngine};
use serde::Serialize;
use std::path::Path;

/// 一个候选 UI 控件（视觉上独立的区域 + 其内文字）。
///
/// 不含业务语义：不声称它是「按钮」还是「输入框」，只说「这块区域 + 这些字」。
#[derive(Clone, Debug, Serialize)]
pub struct Widget {
    /// 控件序号（按面积从大到小）。
    pub id: usize,
    /// 区域内关联到的文字（按 x 排序拼接）；无文字则为空串。
    pub label: String,
    /// 包围盒 `(x, y, w, h)`（像素，原点左上）。已是合并后的整体边界。
    pub rect: (u32, u32, u32, u32),
    /// 区域主导色（来自主颜色区域）。
    pub color: [u8; 3],
    /// 面积占全图比例（0–1）。
    pub area_ratio: f32,
    /// 该控件主要来自哪种信号：颜色连通域 / 纯文字锚点。
    pub source: WidgetSource,
}

/// 控件信号来源（用于排查 / 调参）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum WidgetSource {
    /// 主要由颜色连通域得到（文字只是贴标签）。
    Color,
    /// 主要由文字框得到（颜色层没把它分成独立区域，靠 OCR 框补回）。
    Text,
}

/// 布局分析器：组合颜色分割 + 文字识别。
pub struct LayoutAnalyzer {
    /// OCR 引擎。`None` 表示只做纯颜色分析（无文字标签）。
    ocr: Option<OcrEngine>,
    /// 颜色分割参数（可运行时覆盖）。
    opts: SegmentOpts,
    /// 背景判定阈值：区域像素数 ≥ 全图此比例视为背景，丢弃。
    bg_ratio: f32,
    /// 最小边长（像素）：颜色区域任一边 < 此值的视为线状伪影（窗口边框 /
    /// 分隔线），不作为控件输出。默认 3。
    min_dim: u32,
}

impl LayoutAnalyzer {
    /// 用指定 OCR 模型套件 + 模型目录构建（同时保留颜色参数默认）。
    pub fn with_ocr(profile: ModelProfile, model_dir: &Path, opts: SegmentOpts) -> Result<Self> {
        let ocr = Some(OcrEngine::from_profile(profile, model_dir)?);
        Ok(Self {
            ocr,
            opts,
            bg_ratio: 0.9,
            min_dim: 3,
        })
    }

    /// 纯颜色分析（不加载 OCR 模型，无文字标签）。
    pub fn color_only(opts: SegmentOpts) -> Self {
        Self {
            ocr: None,
            opts,
            bg_ratio: 0.9,
            min_dim: 3,
        }
    }

    /// 自定义背景阈值（默认 0.9）。
    pub fn set_bg_ratio(&mut self, r: f32) -> &mut Self {
        self.bg_ratio = r.clamp(0.0, 1.0);
        self
    }

    /// 自定义最小边长阈值（默认 3）；小于它的线状区域不作为控件。
    pub fn set_min_dim(&mut self, d: u32) -> &mut Self {
        self.min_dim = d.max(1);
        self
    }

    /// 对一张 RGB 图做布局分析，返回候选控件（按面积从大到小）。
    ///
    /// 需要 `&mut self` 是因为底层 `OcrEngine::detect` 借用 `&mut self`；颜色分割
    /// 本身不需要可变状态。
    pub fn analyze(&mut self, img: &RgbImage) -> Result<Vec<Widget>> {
        let (w, h) = img.dimensions();
        let total = (w * h) as f32;

        // 1. 颜色连通域。
        let regions = segment_by_color(img, self.opts);

        // 2. 文字框（若有 OCR）。
        let texts: Vec<OcrBoxResult> = match &mut self.ocr {
            Some(engine) => {
                let data = img.clone().into_raw();
                let arr = Array3::from_shape_vec((h as usize, w as usize, 3), data)
                    .map_err(|e| anyhow::anyhow!("图像重塑失败: {e}"))?;
                engine.detect(&arr)?
            }
            None => Vec::new(),
        };

        // 3. 过滤背景区域与退化线状区域。
        //    - 覆盖全图比例过高 → 背景。
        //    - 任一边长 < min_dim（默认 3px）→ 线状伪影（如窗口顶部 1px 高光
        //      边、1px 分隔线），不是控件，剔除。否则这种全宽 1px 条会因面积
        //      占比极小而漏过背景判定，作为无意义控件输出。
        let fg: Vec<_> = regions
            .into_iter()
            .filter(|r| (r.pixel_count as f32) < total * self.bg_ratio)
            .filter(|r| {
                let (_, _, w, h) = r.rect;
                w >= self.min_dim && h >= self.min_dim
            })
            .collect();

        // 4. 合并「文字笔画细条」到其所属容器。
        //    一个区域若远小于某个更大的、y 重叠且水平相邻/包含它的区域，
        //    则视为该容器的子笔画，不单独输出，但其像素并入容器边界。
        let merged = merge_small_into_container(&fg);

        // 5. 每个合并后的容器：贴文字标签、算面积（主信号始终是颜色）。
        //    同时记录哪些文字框已被某容器吸收，用于第 6 步补 Text 源控件。
        let mut absorbed_text: Vec<bool> = vec![false; texts.len()];
        let mut widgets: Vec<Widget> = Vec::new();
        for (i, reg) in merged.into_iter().enumerate() {
            let (rx, ry, rw, rh) = reg.rect;
            // 收集中心落在该 rect 内的文字，按 x 排序拼接。
            let mut in_idx: Vec<usize> = texts
                .iter()
                .enumerate()
                .filter(|(_, t)| point_in_rect(t.center, reg.rect))
                .map(|(idx, _)| idx)
                .collect();
            in_idx.sort_by(|&a, &b| texts[a].center[0].partial_cmp(&texts[b].center[0]).unwrap());
            let label: String = in_idx
                .iter()
                .map(|&idx| {
                    absorbed_text[idx] = true;
                    texts[idx].text.as_str()
                })
                .collect::<Vec<_>>()
                .join(" ");

            widgets.push(Widget {
                id: i,
                label,
                rect: (rx, ry, rw, rh),
                color: reg.color,
                area_ratio: reg.pixel_count as f32 / total,
                source: WidgetSource::Color,
            });
        }

        // 6. 补回「颜色层没分成独立区域、靠 OCR 框才能定位」的控件：
        //    未被任何容器吸收的纯文字框（典型如两个同色按钮靠文字区分）。
        for (idx, t) in texts.iter().enumerate() {
            if absorbed_text[idx] {
                continue;
            }
            let [minx, maxx] = t.x_range;
            let [miny, maxy] = t.y_range;
            let (x, y) = (minx.floor() as u32, miny.floor() as u32);
            let (w, h) = ((maxx - minx).ceil() as u32, (maxy - miny).ceil() as u32);
            if w == 0 || h == 0 {
                continue;
            }
            widgets.push(Widget {
                id: widgets.len(),
                label: t.text.clone(),
                rect: (x, y, w, h),
                color: [0, 0, 0],
                area_ratio: (w * h) as f32 / total,
                source: WidgetSource::Text,
            });
        }

        // 按面积从大到小排序，id 重排。
        widgets.sort_by(|a, b| b.area_ratio.partial_cmp(&a.area_ratio).unwrap());
        for (i, wd) in widgets.iter_mut().enumerate() {
            wd.id = i;
        }
        Ok(widgets)
    }
}

/// 把控件列表画回原图，生成一张带标注的图（不修改入参）。
///
/// 每个控件：
/// - 用按 `id` 取色的矩形边框标出包围盒（不同控件不同色，便于区分）。
/// - 在几何中心画一个「＋」十字（即操作回灌的点击目标）。
/// - 在左上角用内置 3×5 点阵字画出控件 `id`，方便与 JSON 里的 `label` 对应。
///
/// 设计：零额外依赖，只用 `image` 的 `put_pixel` 手绘；文字用极简点阵，
/// 不引入字体渲染。需要「图上直接出文字标签」可后续接 `imageproc`。
pub fn annotate(img: &RgbImage, widgets: &[Widget]) -> RgbImage {
    let mut out = img.clone();
    let (w, h) = out.dimensions();

    for wd in widgets {
        let (x, y, rw, rh) = wd.rect;
        // 按 id 取一个稳定且区分度高的色（HSV 等距取色再转 RGB）。
        let color = hsv_to_rgb((wd.id as f32 * 47.0) % 360.0, 0.7, 1.0);

        // 1. 矩形边框（四条 1px 边，越界部分截断到图内）。
        let x0 = x.min(w.saturating_sub(1));
        let y0 = y.min(h.saturating_sub(1));
        let x1 = (x + rw).min(w).saturating_sub(1);
        let y1 = (y + rh).min(h).saturating_sub(1);
        for xx in x0..=x1 {
            put(&mut out, xx, y0, color);
            put(&mut out, xx, y1, color);
        }
        for yy in y0..=y1 {
            put(&mut out, x0, yy, color);
            put(&mut out, x1, yy, color);
        }

        // 2. 中心十字（点击目标）。
        let cx = x + rw / 2;
        let cy = y + rh / 2;
        for d in 0..8u32 {
            put(
                &mut out,
                cx.saturating_sub(d).min(w.saturating_sub(1)),
                cy,
                [255, 255, 255],
            );
            put(
                &mut out,
                (cx + d).min(w.saturating_sub(1)),
                cy,
                [255, 255, 255],
            );
            put(
                &mut out,
                cx,
                cy.saturating_sub(d).min(h.saturating_sub(1)),
                [255, 255, 255],
            );
            put(
                &mut out,
                cx,
                (cy + d).min(h.saturating_sub(1)),
                [255, 255, 255],
            );
        }

        // 3. 左上角点阵 id（3×5 字，逐位画）。
        draw_id(&mut out, wd.id, x0, y0, color);
    }
    out
}

/// 安全写像素（越界忽略）。
fn put(img: &mut RgbImage, x: u32, y: u32, c: [u8; 3]) {
    if x < img.width() && y < img.height() {
        img.put_pixel(x, y, image::Rgb(c));
    }
}

/// HSV（h∈[0,360), s,v∈[0,1]）→ RGB。
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to = |f: f32| (f + m).clamp(0.0, 1.0) * 255.0;
    [to(r1) as u8, to(g1) as u8, to(b1) as u8]
}

/// 3×5 点阵数字字形（0–9），每行为 3 bit（1=亮）。
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

/// 用点阵字把 id（十进制）画在 (ox, oy) 处，每位 3×5，位间留 1px 间隔。
fn draw_id(img: &mut RgbImage, mut id: usize, ox: u32, oy: u32, color: [u8; 3]) {
    // 多位数从高位到低位画；先算出位数。
    let mut digits = Vec::new();
    if id == 0 {
        digits.push(0);
    } else {
        while id > 0 {
            digits.push(id % 10);
            id /= 10;
        }
        digits.reverse();
    }
    let mut cx = ox;
    for d in digits {
        let glyph = DIGITS[d];
        for (row, &line) in glyph.iter().enumerate() {
            for col in 0..3 {
                if (line >> (2 - col)) & 1 == 1 {
                    put(img, cx + col as u32, oy + row as u32, color);
                }
            }
        }
        cx += 4; // 3 宽 + 1 间隔
    }
}

/// 判断一个点是否落在 `(x,y,w,h)` 矩形内（含左/上边界，不含右/下边界）。
fn point_in_rect(p: [f32; 2], rect: (u32, u32, u32, u32)) -> bool {
    let (x, y, w, h) = rect;
    p[0] >= x as f32 && p[0] < (x + w) as f32 && p[1] >= y as f32 && p[1] < (y + h) as f32
}

/// 把「远小于容器的文字笔画细条」合并进它所属的容器区域。
///
/// 规则：对区域 A，若存在另一个区域 B 满足
/// - A 面积 < B 面积的三分之一，且
/// - A 与 B 在 y 上重叠（相交），且
/// - A 整体（或其几何中心）水平落在 B 的 x 范围内（或紧邻），
/// 则把 A 视为 B 的子笔画：扩展 B 的包围盒以包含 A，A 不再单独输出。
///
/// 这是在 `testing_08` 上观察到的现实：按钮是「大色块 + 内部细文字笔画（3px 竖条）」，
/// 颜色层会把它们分成多个区域，需要归并回按钮整体。
fn merge_small_into_container(
    regions: &[color_analysis::ColorRegion],
) -> Vec<color_analysis::ColorRegion> {
    let n = regions.len();
    let mut absorbed = vec![false; n];
    // 容器索引 -> 已扩展后的包围盒 (minx,miny,maxx,maxy) 与累计颜色。
    let mut containers: Vec<(u32, u32, u32, u32, u64, u64, u64, usize)> = Vec::new();
    // 累加器：minx,miny,maxx,maxy,sum_r,sum_g,sum_b,count

    // 先按面积从大到小处理，大区域优先当容器。
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| regions[b].pixel_count.cmp(&regions[a].pixel_count));

    for &i in &order {
        if absorbed[i] {
            continue;
        }
        let a = &regions[i];
        let (ax, ay, aw, ah) = a.rect;
        let a_area = a.pixel_count;

        // 尝试并入已有某个容器。
        let mut merged_into: Option<usize> = None;
        for (ci, (bx, by, bw, bh, _, _, _, _)) in containers.iter().enumerate() {
            let b_area = ((bx + bw - *bx) * (by + bh - *by)) as usize;
            if a_area * 3 < b_area {
                // y 重叠？
                let y_overlap = !(ay + ah <= *by || ay >= *by + *bh);
                // 水平落在 B 的 x 范围内（留 4px 容差给紧邻笔画）？
                let x_inside = (ax as i64) >= (*bx as i64 - 4)
                    && ((ax + aw) as i64) <= (*bx as i64 + *bw as i64 + 4);
                if y_overlap && x_inside {
                    merged_into = Some(ci);
                    break;
                }
            }
        }

        if let Some(ci) = merged_into {
            let (bx, by, bw, bh, sr, sg, sb, cnt) = &mut containers[ci];
            let minx = (*bx).min(ax);
            let miny = (*by).min(ay);
            let maxx = (*bx + *bw).max(ax + aw);
            let maxy = (*by + *bh).max(ay + ah);
            *bx = minx;
            *by = miny;
            *bw = maxx - minx;
            *bh = maxy - miny;
            // 累加颜色（用 A 的原色近似；精确需回看图，这里以区域平均色计）。
            *sr += a.color[0] as u64 * a_area as u64;
            *sg += a.color[1] as u64 * a_area as u64;
            *sb += a.color[2] as u64 * a_area as u64;
            *cnt += a_area;
            absorbed[i] = true;
        } else {
            // 成为新容器。
            containers.push((
                ax,
                ay,
                aw,
                ah,
                a.color[0] as u64 * a_area as u64,
                a.color[1] as u64 * a_area as u64,
                a.color[2] as u64 * a_area as u64,
                a_area,
            ));
        }
    }

    containers
        .into_iter()
        .map(
            |(x, y, w, h, sr, sg, sb, cnt)| color_analysis::ColorRegion {
                color: [
                    (sr / cnt as u64) as u8,
                    (sg / cnt as u64) as u8,
                    (sb / cnt as u64) as u8,
                ],
                rect: (x, y, w, h),
                pixel_count: cnt,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn color_only_splits_two_distinct_blocks() {
        // 左半红、右半蓝的图：纯颜色分析应分出两个控件。
        let mut img = RgbImage::from_pixel(40, 20, Rgb([0, 0, 0]));
        for y in 0..20u32 {
            for x in 0..20u32 {
                img.put_pixel(x, y, Rgb([255, 0, 0]));
            }
            for x in 20..40u32 {
                img.put_pixel(x, y, Rgb([0, 0, 255]));
            }
        }
        let mut analyzer = LayoutAnalyzer::color_only(SegmentOpts::default());
        let widgets = analyzer.analyze(&img).expect("analyze 失败");

        // 背景是被过滤的整图底色；前景应有两个色块。
        let fg: Vec<&Widget> = widgets.iter().filter(|w| w.area_ratio < 0.9).collect();
        assert_eq!(fg.len(), 2, "应分出左右两个前景控件");
        let colors: Vec<[u8; 3]> = fg.iter().map(|w| w.color).collect();
        assert!(colors.contains(&[255, 0, 0]));
        assert!(colors.contains(&[0, 0, 255]));
        // 两个控件都不应来自文字（纯颜色模式无 OCR）。
        assert!(fg.iter().all(|w| w.source == WidgetSource::Color));
    }

    #[test]
    fn background_full_image_is_filtered() {
        // 整图单色：除背景外不应有前景控件。
        let img = RgbImage::from_pixel(30, 30, Rgb([100, 100, 100]));
        let mut analyzer = LayoutAnalyzer::color_only(SegmentOpts::default());
        let widgets = analyzer.analyze(&img).expect("analyze 失败");
        assert!(
            widgets.iter().all(|w| w.area_ratio >= 0.9),
            "单色图只应剩背景控件"
        );
    }
}
