//! 业务执行层：把 `ocr_layout` 的控件候选变成对目标应用的真实操作。
//!
//! 本层才真正「理解」语义——把 "点击 Reload" 这种意图翻译成对某个坐标的点击。
//! 它不关心颜色/文字怎么来的（`ocr_layout` 负责），也不关心点击怎么注入
//! （`Executor` 负责，可插拔）。
//!
//! 关键解耦：
//! - **识别**：复用 `ocr_layout::LayoutAnalyzer`，输入截图 → `Widget` 列表。
//! - **执行**：通过 [`Executor`] trait 注入操作。提供
//!   - [`PrintExecutor`]：只打印「将要点击 (x,y)」，用于验证识别→定位链路，
//!     不真正操作（CI / 无显示环境默认用它）。
//!   - [`YdotoolExecutor`]：用 `ydotool` 向 Wayland 注入鼠标点击，需要窗口在
//!     屏幕上的偏移（[`YdotoolExecutor::window_offset`]）把窗口相对坐标转成
//!     绝对坐标。
//!
//! 这样 `ocr_agent` 对目标应用完全透明：不需要 app 暴露任何调试接口，只靠
//! 「看屏幕 + 模拟输入」操控，与人工操作等价。

use anyhow::{Context, Result};
use capturer::{Capturer, ScreenCastCapturer};
use glam::IVec2;
use image::RgbImage;
use ocr_layout::{LayoutAnalyzer, Widget};
use std::path::PathBuf;

/// 操作执行器：把「窗口相对坐标的一次点击」落地。
///
/// 坐标语义统一为**窗口相对像素**（窗口左上角为原点），由具体实现决定如何
/// 转成绝对坐标 / 注入方式。
///
/// **看 / 操作分离**：`Executor` 只负责「操作」（把坐标变成一次真实点击），
/// 不关心画面从哪来、目标是否可见。是否要先「看」（抓图识别）由调用方决定。
pub trait Executor {
    /// 在窗口相对坐标 (x, y) 处点击一次。
    fn click_window(&self, x: u32, y: u32) -> Result<()>;
}

/// 空执行器：只打印意图，不真正操作。
///
/// 用于验证「识别 → 定位」是否正确（确认 center 落在按钮上），以及无显示 /
/// 无注入权限的环境。
pub struct PrintExecutor;

impl Executor for PrintExecutor {
    fn click_window(&self, x: u32, y: u32) -> Result<()> {
        println!("[PrintExecutor] 将点击窗口坐标 ({x}, {y})");
        Ok(())
    }
}

/// 基于 `ydotool` 的真实点击执行器（Wayland / 通用 Linux）。
///
/// 底层委托给 [`screen_operator::ScreenOperator`]（屏幕绝对坐标注入）；
/// 本层只负责把 `Executor` 约定的**窗口相对坐标**加上窗口偏移换算成绝对坐标：
/// `绝对 = (窗口相对x + offset_x, 窗口相对y + offset_y)`。
///
/// 偏移 [`YdotoolExecutor::window_offset`] = (offset_x, offset_y)，可由
/// `ocr_agent` 的 `infer_window_offset` 从「全屏图 + 窗口流图」自动反推，或手动
/// 测量。纯全屏闭环（目标被前台器提到最前、窗口即屏幕坐标）时偏移取 `(0, 0)`。
///
/// **要求点击坐标处目标窗口处于最上层**（Wayland 输入模型：compositor 按坐标下
/// 最上层窗口派发），与目标是否「活跃/聚焦」无关，但与「是否被遮挡」强相关。
/// 把目标切到前台是调用方 / 前台器（`Foregrounder`）的职责。
pub struct YdotoolExecutor {
    /// 窗口左上角相对屏幕的偏移 (x, y)。**物理像素**（KWin 逻辑 × scale）。
    pub window_offset: (i32, i32),
    /// 物理→逻辑缩放比。本 crate 把 `窗口相对物理 + 物理 offset` 换算成逻辑坐标后
    /// 再交给 `screen_operator`（其移动/点击入口统一收**逻辑**坐标）。
    scale: f32,
    /// 底层绝对坐标操作器（收逻辑坐标），已用 [`KdeForegrounder`] 组装好闭环读数源。
    operator: screen_operator::ScreenOperator<
        screen_operator::YdotoolBackend,
        screen_operator::KwinProbe,
    >,
}

impl YdotoolExecutor {
    /// 构造（不带前台器）：偏移取给定值、scale=1.0。
    ///
    /// 注意：不组装 `KdeForegrounder` 时，`ScreenOperator` 的闭环移动读不到光标、
    /// 会退化到不依赖光标的路径（本机 KWin 下不可用）。实际用前请用
    /// [`with_foregrounder`] 把目标窗口的前台器装进去。
    pub fn new(window_offset: (i32, i32)) -> Self {
        Self {
            window_offset,
            scale: 1.0,
            operator: screen_operator::ScreenOperator::new(),
        }
    }

    /// 构造并组装桌面闭环：用 [`KdeForegrounder`] 提供「相对移动闭环」的读数来源
    /// （`cursor_pos`），避开本机失效的 ydotool 绝对移动。
    ///
    /// - `window_offset`：窗口左上角在屏幕上的**物理**偏移（KWin 逻辑 × scale）。
    /// - `scale`：物理→逻辑缩放比（把物理绝对目标换算成逻辑坐标交给 `ScreenOperator`）。
    /// - `fg`：目标窗口前台器（提供 `cursor_pos` 读数 + `raise` 切前台）。
    ///
    /// 组装后 `click_window` 走 `ScreenOperator::click_left_at` → 内部「读当前 + 相对移
    /// + 原地点」闭环，落点可靠。
    pub fn with_foregrounder(window_offset: (i32, i32), scale: f32, fg: KdeForegrounder) -> Self {
        Self {
            window_offset,
            scale,
            operator: screen_operator::ScreenOperator::new().with_foregrounder(fg),
        }
    }
}

/// 前台器：把**目标窗口切到最前**（raise / focus），使「截全屏能看到它」且
/// 「点击坐标能打在它身上」。
///
/// 这是与 `Executor` 正交的另一条可插拔链路：
/// - **看（截全屏 / 区域截屏）前**：目标必须前台，否则截不到 → 需 raise。
/// - **操作（点击）前**：点击坐标处目标必须最上层 → 需 raise。
/// - **录屏（ScreenCast 窗口流）前**：窗口流是 compositor 直接给的，**不受遮挡
///   影响**，raise 是可选的（调用方决定是否安装前台器）。
///
/// 因为「切前台」是 compositor 相关动作（KDE 用 `kdotool`、GNOME 用 `gdbus`、
/// wlroots 用 `hyprctl` 等），跨桌面不通用，所以做成 trait 由调用方注入；不注入
/// 时（[`Agent::new`]）视为「调用方已自行保证目标在前台」，Agent 不做任何切换。
///
/// **前台器实现已迁至 `screen_operator`**（操作侧：把窗口提到最前 + 读 KWin 光标/几何，
/// 是 `screen_operator` 相对移动闭环的读数来源）。这里 re-export 保持业务层兼容调用，
/// 业务代码继续写 `ocr_agent::KdeForegrounder` / `ocr_agent::Foregrounder` 即可。
pub use screen_operator::{Foregrounder, KdeForegrounder, NoopForegrounder};

impl Executor for YdotoolExecutor {
    fn click_window(&self, x: u32, y: u32) -> Result<()> {
        let (ox, oy) = self.window_offset;
        // 物理绝对 = 窗口相对物理 + 物理 offset；转成逻辑坐标交给 screen_operator
        // （其 click_left_at 收逻辑坐标）。
        let abs_x = ((ox + x as i32) as f32 / self.scale).round() as i32;
        let abs_y = ((oy + y as i32) as f32 / self.scale).round() as i32;
        // 委托给 screen_operator 完成绝对坐标点击（其内部已处理 ydotool 的相对移动
        // 闭环与左键 0xC0 键码，避开 `-- -a` 形式的 stack smashing 坑）。
        let abs_pos = IVec2::new(abs_x, abs_y);
        self.operator
            .click_left_at(abs_pos)
            .context("YdotoolExecutor 点击失败")
    }
}

/// 控件中心（窗口相对坐标），由包围盒算出。
fn center_of(w: &Widget) -> (u32, u32) {
    let (x, y, ww, hh) = w.rect;
    (x + ww / 2, y + hh / 2)
}

/// 从「全屏图」+「窗口流图」反推窗口在屏幕上的偏移（纯图像方法，不依赖任何
/// 外部命令 / compositor 接口，纯 Wayland 下也能用）。
///
/// 原理：ScreenCast 的窗口流是「窗口自身合成」（窗口相对坐标，不受遮挡），而
/// 同一时刻抓的全屏 Monitor 流含该窗口在屏幕上的真实位置。两图里窗口像素一致，
/// 于是在全屏图里滑窗搜索与窗口图最相似的块，峰值位置即窗口左上角的屏幕坐标。
///
/// 实现：降采样（默认因子 8）转灰度后用 **整窗 SSD** 滑窗（用整窗而非内缩模板，
/// 否则当贴块大于模板时会有多个位置 SSD 同为 0、无法唯一确定）。返回
/// `(offset_x, offset_y)`（屏幕像素）。
///
/// 这是让 `ocr_agent` 自动知道偏移的关键：无需 xdotool/kdotool（纯 Wayland 下
/// 通常不可用），也不用假设窗口居中。
pub fn infer_window_offset(full: &RgbImage, window: &RgbImage) -> (i32, i32) {
    const DOWN: u32 = 8;
    let full_small = downscale_gray(full, DOWN);
    let win_small = downscale_gray(window, DOWN);

    let (fw, fh) = (full_small.width(), full_small.height());
    let (ww, wh) = (win_small.width(), win_small.height());
    if fw < ww || fh < wh {
        return (0, 0);
    }

    let mut best = (0i32, 0i32);
    let mut best_ssd = u64::MAX;
    for y in 0..=(fh - wh) {
        for x in 0..=(fw - ww) {
            let mut ssd = 0u64;
            for ty in 0..wh {
                for tx in 0..ww {
                    let a = full_small.get_pixel(x + tx, y + ty).0[0] as i32;
                    let b = win_small.get_pixel(tx, ty).0[0] as i32;
                    let d = a - b;
                    ssd += (d * d) as u64;
                }
            }
            if ssd < best_ssd {
                best_ssd = ssd;
                best = (x as i32, y as i32);
            }
        }
    }
    // 升采样回原图坐标（降采样块位置即窗口左上角 * DOWN）。
    (best.0 * DOWN as i32, best.1 * DOWN as i32)
}

/// 把 capturer 抓来的 `RgbaImage` 转成 `RgbImage`（丢弃 alpha，OCR/颜色分析不需要）。
///
/// 实现已迁至 `image_util`（通用图像转换，与业务解耦）；这里 re-export 保持兼容。
pub use image_util::rgba_to_rgb;

/// 降采样并转灰度（取原图 1/DOWN 的每 DOWNd 像素，亮度 = 0.299R+0.587G+0.114B）。
fn downscale_gray(img: &RgbImage, down: u32) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    let nw = (w / down).max(1);
    let nh = (h / down).max(1);
    let mut out = RgbImage::new(nw, nh);
    for y in 0..nh {
        for x in 0..nw {
            let p = img.get_pixel(x * down, y * down).0;
            let g = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8;
            out.put_pixel(x, y, image::Rgb([g, g, g]));
        }
    }
    out
}

/// 按标签从控件列表中挑一个目标控件（纯函数，便于单测）。
///
/// 匹配优先级（不区分大小写）：
/// 1. 精确相等（标签 == target）；
/// 2. 整词命中（target 作为独立词出现，前后非字母数字）——优先，避免「reload」
///    这种词嵌在别的窗口的长文本里被误匹配；
/// 3. 退化到普通子串包含。
///
/// 同一优先级内，**选面积最小者**（按钮通常是画面里很小的块；而误匹配的编辑器
/// 大文本块面积很大，会被排除）。返回 `None` 表示无匹配。
pub fn find_widget_by_label<'a>(widgets: &'a [Widget], target: &str) -> Option<&'a Widget> {
    let t = target.to_lowercase();

    // 1. 精确相等。
    if let Some(w) = widgets
        .iter()
        .find(|w| !w.label.is_empty() && w.label.to_lowercase() == t)
    {
        return Some(w);
    }

    // 整词判定：target 在 label 里，且紧贴的前后字符不是字母/数字/下划线。
    let is_whole_word = |label: &str| -> bool {
        if !label.contains(&t) {
            return false;
        }
        let bytes = label.as_bytes();
        let mut start = 0;
        while let Some(pos) = label[start..].to_lowercase().find(&t) {
            let abs = start + pos;
            let before_ok =
                abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric() && bytes[abs - 1] != b'_';
            let after_ok = abs + t.len() >= bytes.len()
                || !bytes[abs + t.len()].is_ascii_alphanumeric() && bytes[abs + t.len()] != b'_';
            if before_ok && after_ok {
                return true;
            }
            start = abs + t.len();
        }
        false
    };

    // 2. 整词命中优先；3. 否则普通子串；两者都按面积最小选。
    let pick_smallest = |pred: &dyn Fn(&str) -> bool| -> Option<&'a Widget> {
        let mut best: Option<&Widget> = None;
        for w in widgets.iter() {
            if w.label.is_empty() || !pred(&w.label.to_lowercase()) {
                continue;
            }
            match best {
                None => best = Some(w),
                Some(b) => {
                    if w.area_ratio < b.area_ratio {
                        best = Some(w);
                    }
                }
            }
        }
        best
    };

    if let Some(w) = pick_smallest(&(|l: &str| is_whole_word(l))) {
        return Some(w);
    }
    pick_smallest(&(|l: &str| l.contains(&t)))
}

/// 操控代理：识别（看） + 执行（操作），两条链路解耦。
///
/// - **看**：`analyzer` 把截图变成 `Widget` 列表（识别/定位），不依赖目标是否可见。
/// - **操作**：`executor` 把坐标变成一次点击（[`Executor`]），点击前要求目标在
///   点击坐标处最上层。
/// - **前台**：`foregrounder`（可选）在操作前 / 截全屏前把目标切到最前，满足
///   上面的前置条件。录屏（窗口流）路径下是否切前台由调用方决定（装不装前台器）。
pub struct Agent {
    analyzer: LayoutAnalyzer,
    executor: Box<dyn Executor>,
    foregrounder: Option<Box<dyn Foregrounder>>,
    /// 调试目录（可选）：开启后每个「看」步骤都会把标注图（窗口 OCR 标注 +
    /// 全屏红框=app位置 + 绿十字=点击落点）存到该目录，便于人工核对识别/定位。
    debug_dir: Option<PathBuf>,
    /// 调试帧计数（每次保存标注图自增），用于生成唯一文件名。
    debug_frame: u32,
}

impl Agent {
    /// 构造（不含前台器）：调用方需自行保证目标在点击/截全屏时处于前台。
    pub fn new(analyzer: LayoutAnalyzer, executor: Box<dyn Executor>) -> Self {
        Self {
            analyzer,
            executor,
            foregrounder: None,
            debug_dir: None,
            debug_frame: 0,
        }
    }

    /// 构造（带前台器）：在截全屏前、点击前自动把目标切到最前。
    pub fn with_foregrounder(
        analyzer: LayoutAnalyzer,
        executor: Box<dyn Executor>,
        foregrounder: Box<dyn Foregrounder>,
    ) -> Self {
        Self {
            analyzer,
            executor,
            foregrounder: Some(foregrounder),
            debug_dir: None,
            debug_frame: 0,
        }
    }

    /// 开启调试出图：把每个「看」步骤的标注图存到 `dir`（窗口 OCR 标注 +
    /// 全屏红框=app位置 + 绿十字=点击落点）。传 `None` 可关闭。
    pub fn set_debug(&mut self, dir: Option<PathBuf>) {
        self.debug_dir = dir;
        self.debug_frame = 0;
    }

    /// 若开启了调试目录，保存一张标注图并自增帧计数；否则什么都不做。
    fn save_debug(&mut self, name: &str, img: &RgbImage) -> Option<PathBuf> {
        let dir = self.debug_dir.as_ref()?;
        let _ = std::fs::create_dir_all(dir);
        let fname = format!("live_{:03}_{}", self.debug_frame, name);
        self.debug_frame += 1;
        let path = dir.join(&fname);
        match img.save(&path) {
            Ok(_) => {
                eprintln!("[debug] 已保存标注图 -> {}", path.display());
                Some(path)
            }
            Err(e) => {
                eprintln!("[debug] 保存标注图失败 {}: {}", path.display(), e);
                None
            }
        }
    }

    /// 在调试目录里存一张「全屏 + 红框(app) + 绿十字(点击落点)」图（配合
    /// `infer_window_offset` 反推的 offset 与 OCR 命中的控件中心）。
    fn save_debug_fullscreen(
        &mut self,
        full: &RgbImage,
        offset: (i32, i32),
        win_size: (u32, u32),
        hit: Option<&Widget>,
    ) {
        if self.debug_dir.is_none() {
            return;
        }
        let mut marked = full.clone();
        let (ox, oy) = offset;
        let (ww, wh) = win_size;
        let x0 = ox.max(0) as u32;
        let y0 = oy.max(0) as u32;
        let x1 = (x0 + ww).min(marked.width());
        let y1 = (y0 + wh).min(marked.height());
        // 红框：app 位置。
        for xx in x0..x1 {
            for yy in [y0, y1.saturating_sub(1)] {
                if yy < marked.height() {
                    *marked.get_pixel_mut(xx, yy) = image::Rgb([255, 0, 0]);
                }
            }
        }
        for yy in y0..y1 {
            for xx in [x0, x1.saturating_sub(1)] {
                if xx < marked.width() {
                    *marked.get_pixel_mut(xx, yy) = image::Rgb([255, 0, 0]);
                }
            }
        }
        // 绿十字：点击落点（若 OCR 命中目标）。
        if let Some(w) = hit {
            let (rx, ry, rww, rhh) = w.rect;
            let ax = (ox + rx as i32 + rww as i32 / 2).max(0) as u32;
            let ay = (oy + ry as i32 + rhh as i32 / 2).max(0) as u32;
            for d in 0..12u32 {
                for (px, py) in [
                    (ax.saturating_sub(d), ay),
                    (ax + d, ay),
                    (ax, ay.saturating_sub(d)),
                    (ax, ay + d),
                ] {
                    if px < marked.width() && py < marked.height() {
                        *marked.get_pixel_mut(px, py) = image::Rgb([0, 255, 0]);
                    }
                }
            }
            eprintln!(
                "[debug] 目标落点 ≈ ({}, {})（绿十字，应在红框内按钮处）",
                ax, ay
            );
        }
        self.save_debug("fullscreen.png", &marked);
    }

    /// 运行中替换执行器（如先以占位执行器构造，待拿到窗口位置后再换上真正
    /// 带 offset 的 `YdotoolExecutor`）。前台器不受影响。
    pub fn set_executor(&mut self, executor: Box<dyn Executor>) {
        self.executor = executor;
    }

    /// 把目标窗口切到最前（若装了前台器）。用于「截全屏前」和「操作（点击）前」
    /// 两个必须点——录屏（窗口流）路径是否调用由调用方决定。
    fn raise_target(&self) -> Result<()> {
        if let Some(fg) = self.foregrounder.as_ref() {
            fg.raise()?;
        }
        Ok(())
    }

    /// 「看」：把截图分析成控件候选列表（纯识别，不点击）。
    ///
    /// 与 `click_by_label` 解耦——调用方可以只看不点（如先确认目标在画面里，
    /// 或录屏窗口流下识别按钮坐标）。`img` 来源不限（截全屏 / 窗口流均可）。
    pub fn recognize(&mut self, img: &RgbImage) -> Result<Vec<Widget>> {
        self.analyzer.analyze(img)
    }

    /// 按标签点击一个控件（「操作」）。
    ///
    /// 点击前**自动把目标切到最前**（若装了前台器），满足 Wayland 点击的前置
    /// 条件：点击坐标处目标必须最上层。
    ///
    /// 匹配优先级（均不区分大小写）：
    /// 1. **精确相等**：标签 == target（如 "Load" 精确命中 Load，不会被 "Reload"
    ///    的包含匹配抢走）。
    /// 2. **子串包含**：无精确命中时，才退回「标签包含 target」，且选面积更大的
    ///    （避免点到同名文字碎片）。
    /// 命中后点击其几何中心。
    pub fn click_by_label(&mut self, img: &RgbImage, target: &str) -> Result<()> {
        self.raise_target()?; // 操作前必须：目标切前台
        let widgets = self.analyzer.analyze(img)?;
        let w = find_widget_by_label(&widgets, target)
            .ok_or_else(|| anyhow::anyhow!("未找到标签匹配 '{target}' 的控件"))?;
        Self::dispatch(self.executor.as_ref(), w)
    }

    /// 直接点击已识别好的控件 `w`（不重新 analyze 图片）。
    ///
    /// 用于「识别 / 显示 / 点击」必须复用**同一份** OCR 结果的场景：OCR 对同一张图
    /// 多次 `analyze` 可能因内部状态/非确定性得到不同中心（如 Reload 中心有时 242 有时
    /// 308），若 debug 显示用一份、点击又 analyze 一份，就会「图上绿十字在按钮上、实际
    /// 点到的却是另一份偏移中心」——表现为移动探针命中、闭环却 delta=0。本方法让调用方
    /// 用已经 analyze 过、且已画进 debug 图的那份 `widgets` 去点击，保证所见即所点。
    ///
    /// **注意：本方法不调用 `raise_target()`**。调用方若在抓帧前已 raise（如
    /// `verify_click_stream` 开头就 raise 过），这里**不能再 raise**——本机 KWin 的
    /// `raiseWindow`+`keepAbove` 会让窗口在点击前被重排/移动，导致「抓帧时算的 offset」
    /// 与「点击时窗口实际位置」不一致，绿十字（按旧 offset 画）看着在按钮上、实际点击
    /// 却落到隔壁按钮（曾表现为点 − 号、count 从 100 变 99）。窗口流遮挡无关，点击前
    /// 无需再次 raise；若独立使用本方法且目标可能不在最前，请自行先 raise。
    pub fn click_widget(&mut self, w: &Widget) -> Result<()> {
        Self::dispatch(self.executor.as_ref(), w)
    }

    /// 打印匹配信息并委托执行器点击控件中心。
    fn dispatch(executor: &dyn Executor, w: &Widget) -> Result<()> {
        let (cx, cy) = center_of(w);
        println!(
            "[Agent] 匹配控件 #{}(label={:?}, area={:.2}%) → 点击中心 ({}, {})",
            w.id,
            w.label,
            w.area_ratio * 100.0,
            cx,
            cy
        );
        executor.click_window(cx, cy)
    }

    /// 读取计数器当前值：找标签能解析为整数的控件（如 "0"）。
    ///
    /// 返回 `None` 表示没识别到数字。多次调用可对比验证点击是否生效。
    pub fn read_count(&mut self, img: &RgbImage) -> Result<Option<i32>> {
        let widgets = self.analyzer.analyze(img)?;
        // 1. 优先：整标签就是纯整数（计数被 OCR 单独成块）。
        for w in &widgets {
            if let Ok(n) = w.label.trim().parse::<i32>() {
                return Ok(Some(n));
            }
        }
        // 2. 退化：testing_08 的计数嵌在 "Load 0 Press ..." / "Count: 0" 这类文本里，
        //    OCR 不会单独成块。找含 "load"/"count"（不区分大小写）的控件，取其中
        //    第一个整数。多个候选取面积最大者（主窗口，而非标题栏的零星数字）。
        let mut best: Option<(i32, f32)> = None;
        for w in &widgets {
            let lower = w.label.to_lowercase();
            if lower.contains("load") || lower.contains("count") {
                if let Some(n) = Self::first_int(&w.label) {
                    let score = w.area_ratio;
                    if best.map(|(_, s)| score > s).unwrap_or(true) {
                        best = Some((n, score));
                    }
                }
            }
        }
        Ok(best.map(|(n, _)| n))
    }

    /// 从字符串里取第一个连续整数（允许负号）。找不到返回 None。
    fn first_int(s: &str) -> Option<i32> {
        let mut digits = String::new();
        let mut in_num = false;
        for c in s.chars() {
            if c.is_ascii_digit() || (c == '-' && digits.is_empty() && in_num == false) {
                // 负号仅在数字开头有效
                if c == '-' && !digits.is_empty() {
                    break;
                }
                digits.push(c);
                in_num = true;
            } else if in_num {
                break;
            }
        }
        digits.parse::<i32>().ok()
    }

    /// 暴露底层 analyzer（如需自定义参数后重新分析）。
    pub fn analyzer(&mut self) -> &mut LayoutAnalyzer {
        &mut self.analyzer
    }

    /// 暴露底层执行器（用于运行时调整执行器内部状态，如回填反推得到的窗口偏移）。
    pub fn executor_mut(&mut self) -> &mut Box<dyn Executor> {
        &mut self.executor
    }
}

/// 闭环验证结果。
#[derive(Debug, Clone)]
pub struct VerifyResult {
    /// 点击前计数。
    pub before: Option<i32>,
    /// 点击后计数。
    pub after: Option<i32>,
    /// 被点击的控件标签。
    pub label: String,
}

impl VerifyResult {
    /// 点击带来的计数变化（点击后 − 点击前）；任一侧未识别到数字则为 None。
    pub fn delta(&self) -> Option<i32> {
        match (self.before, self.after) {
            (Some(b), Some(a)) => Some(a - b),
            _ => None,
        }
    }
}

impl Agent {
    /// 闭环验证：点击某控件后重新抓取一帧，读出点击后的计数，与点击前对比。
    ///
    /// 这是「识别 → 定位 → 点击 → 观测」完整链路的收口：`capture_after` 由调用方
    /// 提供（通常是 `capturer` 重新抓一帧），`ocr_agent` 不直接依赖捕获后端，保持
    /// 解耦。`capture_after` 必须在点击**之后**调用，才能反映操作效果。
    ///
    /// 典型用法（伪代码）：
    /// ```ignore
    /// let after = || -> Result<RgbImage> { Ok(capturer.capture_app_token("")?.0.to_rgb8()) };
    /// let r = agent.verify_click(&img_before, "Reload", after)?;
    /// assert_eq!(r.delta(), Some(50)); // testing_08 的 Reload 使 count += 50
    /// ```
    pub fn verify_click<F>(
        &mut self,
        img_before: &RgbImage,
        label: &str,
        capture_after: F,
    ) -> Result<VerifyResult>
    where
        F: FnOnce() -> Result<RgbImage>,
    {
        // 操作前必须切前台（点击靠坐标派发，目标须最上层）。
        self.raise_target()?;
        let before = self.read_count(img_before)?;
        self.click_by_label(img_before, label)?;
        // 注意：`img_before` 由调用方在「截全屏前」自行负责切前台（见文档）。
        // 若用录屏窗口流抓 img_before，则无需切前台（窗口流不受遮挡影响）。
        let img_after = capture_after()?;
        let after = self.read_count(&img_after)?;
        Ok(VerifyResult {
            before,
            after,
            label: label.to_string(),
        })
    }
}

impl Agent {
    /// 闭环验证（ScreenCast **Monitor 全屏**方案）：用 portal 的 Monitor 源抓全屏
    /// （与 `verify_click_screenshot` 等价，但走 ScreenCast 接口而非 Screenshot 接口，
    /// 不会被「截图授权窗口自动关闭」问题困扰；restore_token 同样可持久化免弹窗）。
    ///
    /// 与「窗口流」方案的区别：Monitor 全屏图里 `Widget.rect` 直接就是**屏幕绝对坐标**，
    /// 因此 `YdotoolExecutor` 的 `window_offset` 设为 `(0, 0)` 即可（相对即绝对），
    /// 无需再查窗口位置。代价是抓取受遮挡影响——故点击前 `click_by_label` 内部会
    /// `raise_target()` 把目标提到最前，保证抓帧时目标可见、坐标正确。
    ///
    /// 典型用法：
    /// ```ignore
    /// let cap = ScreenCastCapturer::with_restore_token(token);
    /// let mut agent = Agent::with_foregrounder(
    ///     analyzer, Box::new(YdotoolExecutor::new((0, 0))),
    ///     Box::new(KdeForegrounder::new("testing_08")),
    /// );
    /// let r = agent.verify_click_screencast(&cap, "Reload").await?;
    /// assert_eq!(r.delta(), Some(50));
    /// ```
    pub async fn verify_click_screencast(
        &mut self,
        cap: &ScreenCastCapturer,
        label: &str,
    ) -> Result<VerifyResult> {
        // 截全屏（Monitor）前必须切前台（否则被挡目标截不到 / 坐标错位）。
        self.raise_target()?;
        let (before_rgba, _tok) = cap.capture_fullscreen_token().await?;
        let img_before = rgba_to_rgb(&before_rgba);
        let before = self.read_count(&img_before)?;
        // 点击（click_by_label 内部再 raise 一次 + 用绝对坐标点）。
        self.click_by_label(&img_before, label)?;
        // 再切前台，保证点击后抓帧时目标仍可见。
        self.raise_target()?;
        let (after_rgba, _tok) = cap.capture_fullscreen_token().await?;
        let img_after = rgba_to_rgb(&after_rgba);
        let after = self.read_count(&img_after)?;
        Ok(VerifyResult {
            before,
            after,
            label: label.to_string(),
        })
    }
}

impl Agent {
    /// 闭环验证（自动抓帧版）：由 `capturer` 自动抓「点击前 / 点击后」两帧，
    /// 省去调用方手搓闭包，**录屏侧完全自动化**。
    ///
    /// - `capturer`：实现 `capturer::Capturer` 的后端（如 `ScreenCastCapturer`）。
    /// - `region`：屏幕坐标 `(x, y, w, h)`，两帧都裁切到同一区域（窗口区域）。
    ///   区域固定即可反映点击前后同一窗口的变化；偏移可用 `infer_window_offset`
    ///   从全屏 + 窗口流反推后填入。
    ///
    /// 调用方只需负责「点击」由哪个 `Executor` 注入（`PrintExecutor` 不真点，
    /// `YdotoolExecutor` 真点）。本方法覆盖「识别 → 自动抓帧 → 读数 → 对比」整条
    /// 链路，仅依赖 capturer 提供帧，与具体 compositor 解耦。
    ///
    /// 典型用法（伪代码，需 async 上下文或 `async_io::block_on` 驱动）：
    /// ```ignore
    /// let cap = capturer::ScreenCastCapturer::new();
    /// let rt = agent.verify_click_capture(
    ///     &cap, (ox, oy, ww, hh), "Reload"
    /// ).await?;
    /// assert_eq!(rt.delta(), Some(50));
    /// ```
    pub async fn verify_click_capture<C: capturer::Capturer + ?Sized>(
        &mut self,
        capturer: &C,
        region: (u32, u32, u32, u32),
        label: &str,
    ) -> Result<VerifyResult> {
        let (x, y, w, h) = region;
        // 1. 截全屏前必须切前台（否则 capture_region 基于全屏截图，截不到被挡目标）。
        self.raise_target()?;
        // 2. 自动抓「点击前」帧。
        let before_rgba = capturer.capture_region(x, y, w, h).await?;
        let img_before = rgba_to_rgb(&before_rgba);
        let before = self.read_count(&img_before)?;
        // 3. 点击目标控件（操作前 click_by_label 内部已再 raise 一次，双保险）。
        self.click_by_label(&img_before, label)?;
        // 4. 截全屏前再次切前台，确保点击后抓帧时目标仍可见。
        self.raise_target()?;
        // 5. 自动抓「点击后」帧。
        let after_rgba = capturer.capture_region(x, y, w, h).await?;
        let img_after = rgba_to_rgb(&after_rgba);
        let after = self.read_count(&img_after)?;
        Ok(VerifyResult {
            before,
            after,
            label: label.to_string(),
        })
    }
}

impl Agent {
    /// 闭环验证（纯全屏方案，无需 restore_token / 录屏窗口流）。
    ///
    /// 流程：截全屏（含自动切前台）→ 识别按钮的**屏幕绝对坐标** → ydotool 直接点
    /// 该绝对坐标 → 再截全屏 → 读数对比。
    ///
    /// - 坐标语义：全屏图里 `Widget.rect` 就是屏幕绝对坐标，因此 `YdotoolExecutor`
    ///   的 `window_offset` 应设为 `(0, 0)`（点击函数把「相对坐标 + offset」当绝对，
    ///   这里相对即绝对）。
    /// - 截全屏前自动 `raise_target()`（目标须前台才能截到）；点击前 `click_by_label`
    ///   内部也会再 raise（操作前必须最上层）。
    /// - 录屏窗口流方案见 [`verify_click_capture`]（遮挡无关，但点仍受最上层约束）。
    ///
    /// 典型用法（伪代码）：
    /// ```ignore
    /// let mut agent = Agent::with_foregrounder(
    ///     analyzer, Box::new(YdotoolExecutor::new((0, 0))),
    ///     Box::new(KdeForegrounder::new("testing_08")),
    /// );
    /// let r = agent.verify_click_screenshot("Reload").await?;
    /// assert_eq!(r.delta(), Some(50));
    /// ```
    pub async fn verify_click_screenshot(&mut self, label: &str) -> Result<VerifyResult> {
        // 截全屏前必须切前台（否则截不到被挡目标）。
        self.raise_target()?;
        let before_rgba = capturer::capture_screenshot().await?;
        let img_before = rgba_to_rgb(&before_rgba);
        let before = self.read_count(&img_before)?;
        // 点击（click_by_label 内部再 raise 一次 + 用绝对坐标点）。
        self.click_by_label(&img_before, label)?;
        // 截全屏前再次切前台，保证点击后抓帧时目标仍可见。
        self.raise_target()?;
        let after_rgba = capturer::capture_screenshot().await?;
        let img_after = rgba_to_rgb(&after_rgba);
        let after = self.read_count(&img_after)?;
        Ok(VerifyResult {
            before,
            after,
            label: label.to_string(),
        })
    }
}

impl Agent {
    /// 闭环验证（ScreenCast 窗口流方案，推荐）：用 portal 的**窗口流**抓 testing_08
    /// 自身的合成表面（不受遮挡，无需前台、无需全屏截图授权），OCR 得到的是
    /// **窗口相对坐标**；点击时由 `YdotoolExecutor` 的 `window_offset`（= 窗口在屏幕
    /// 上的位置，调用方从 `ScreenCastCapturer::capture_app_geom` 拿到的
    /// `Stream::position()`）换算成绝对坐标注入。
    ///
    /// 这是「看 / 操作分离」的标准落地：
    /// - **看**（识别）：窗口流，遮挡无关，连前台器都不强制需要；
    /// - **操作**（点击）：Wayland 输入仍要求目标在点击点最上层，故 `click_by_label`
    ///   内部会再 `raise_target()` 一次。
    ///
    /// 调用方职责：
    /// 1. 用 `ScreenCastCapturer::with_restore_token(token)` 构造 `cap`（首次无 token
    ///    时弹一次对话框选窗，拿到 token 后持久化，之后全自动不弹窗）；
    /// 2. 先 `cap.capture_app_geom("")` 拿一次 `position` 与 `restore_token`，据此
    ///    构造 `YdotoolExecutor::new(position)` 作为 executor（offset = 窗口屏幕位置），
    ///    并把 token 持久化供下次复用；
    /// 3. 再调本方法。
    ///
    /// 典型用法：
    /// ```ignore
    /// let cap = ScreenCastCapturer::with_restore_token(token);
    /// let (_img, pos, new_token) = cap.capture_app_geom("").await?;
    /// let exec = YdotoolExecutor::new(pos.unwrap_or((0, 0)));
    /// let mut agent = Agent::with_foregrounder(analyzer, Box::new(exec),
    ///     Box::new(KdeForegrounder::new("testing_08")));
    /// let r = agent.verify_click_stream(&cap, "Reload").await?;
    /// assert_eq!(r.delta(), Some(50));
    /// ```
    ///
    /// `cap_full`：可选的全屏（Monitor 源）捕获器。若提供且本 Agent 开启了调试出图
    /// （`set_debug`），则每帧都会额外抓一张全屏，用 `infer_window_offset` 反推窗口在
    /// 屏幕上的位置，并把「全屏 + 红框(app) + 绿十字(点击落点)」存到调试目录，便于
    /// 人工核对「看 + 定位」对不对。不传（`None`）则走纯窗口流（无全屏标注图）。
    ///
    /// `debug_offset`：debug 全屏标注用的「窗口屏幕位置」(物理像素)。若提供则用此值
    /// （推荐由 KWin 几何×缩放比算出的稳定真值）画红框/绿十字；不提供则退回
    /// `infer_window_offset` 图像反推（不稳定，仅供对比）。
    pub async fn verify_click_stream(
        &mut self,
        cap: &ScreenCastCapturer,
        cap_full: Option<&ScreenCastCapturer>,
        label: &str,
        debug_offset: Option<(i32, i32)>,
    ) -> Result<VerifyResult> {
        // 「看」采用**两帧策略**：连续抓两帧、间隔 1s、用**第二帧**读数与识别。
        // 理由：第一帧可能含过渡态（点击前的重绘、画面过度效果如阴影/模糊未稳定）
        // 或 PipeWire 旧 buffer，第二帧才是稳定画面。点击前/后都如此，保证 before/after
        // 读的是同一类稳定帧，delta 才可比。debug 模式会把所有抽到的帧都存盘。
        const FRAME_GAP: std::time::Duration = std::time::Duration::from_secs(1);

        // 1. 看（识别）——窗口流，遮挡无关，无需前台。但仍 raise 一次无妨（双保险）。
        self.raise_target()?;
        // 抓两帧 before。
        let mut before_frames: Vec<RgbImage> = Vec::new();
        for f in 0..2 {
            let (rgba, _pos, _tok) = cap.capture_app_geom("").await?;
            before_frames.push(rgba_to_rgb(&rgba));
            if f == 0 {
                std::thread::sleep(FRAME_GAP);
            }
        }
        // 用第二帧（稳定）读数与识别。**只 analyze 一次**，后续 debug 显示与点击都复用
        // 这份 widgets，避免二次 analyze 漂移（OCR 对同图多次 analyze 可能给出不同中心，
        // 导致图上绿十字在按钮上、实际点击却偏到另一份偏移中心）。
        let img_before = before_frames[1].clone();
        let before_widgets = self.analyzer.analyze(&img_before).unwrap_or_default();
        let before = self.read_count(&img_before)?;

        // debug：存所有 before 帧（原图 + 标注），并复用同一份 before_widgets 显示/定位。
        if self.debug_dir.is_some() {
            for (i, fr) in before_frames.iter().enumerate() {
                self.save_debug(&format!("window_before_{}.png", i + 1), fr);
                let ann = ocr_layout::annotate(fr, &before_widgets);
                self.save_debug(&format!("window_before_{}_annot.png", i + 1), &ann);
            }
            eprintln!(
                "[debug] before 帧2 OCR 共识别 {} 个，标签: {:?}",
                before_widgets.len(),
                before_widgets
                    .iter()
                    .map(|w| w.label.as_str())
                    .collect::<Vec<_>>()
            );
            if let Some(cf) = cap_full {
                if let Ok(full_rgba) = cf.capture_fullscreen().await {
                    let full = rgba_to_rgb(&full_rgba);
                    let off =
                        debug_offset.unwrap_or_else(|| infer_window_offset(&full, &img_before));
                    let hit = find_widget_by_label(&before_widgets, label);
                    eprintln!(
                        "[debug] 窗口屏幕位置 offset=(x={}, y={})（{}）",
                        off.0,
                        off.1,
                        if debug_offset.is_some() {
                            "KWin真值"
                        } else {
                            "图像反推"
                        }
                    );
                    self.save_debug_fullscreen(
                        &full,
                        off,
                        (img_before.width(), img_before.height()),
                        hit,
                    );
                }
            }
        }

        // 2. 操作——用**同一份** before_widgets 找目标并点击（所见即所点）。
        let hit = find_widget_by_label(&before_widgets, label)
            .ok_or_else(|| anyhow::anyhow!("未找到标签匹配 '{label}' 的控件"))?;
        self.click_widget(hit)?;

        // 3. 点后**不再 raise**：窗口流遮挡无关，且点击前已 raise 过一次，窗口仍在最前。
        //    直接抓两帧 after（同样间隔 1s、用第二帧），避开点击后的过渡态 / 旧 buffer。
        let mut after_frames: Vec<RgbImage> = Vec::new();
        for f in 0..2 {
            let (rgba, _pos, _tok) = cap.capture_app_geom("").await?;
            after_frames.push(rgba_to_rgb(&rgba));
            if f == 0 {
                std::thread::sleep(FRAME_GAP);
            }
        }
        if self.debug_dir.is_some() {
            for (i, fr) in after_frames.iter().enumerate() {
                self.save_debug(&format!("window_after_{}.png", i + 1), fr);
                let ws = self.analyze_for_debug(fr);
                eprintln!(
                    "[debug] after 帧{} OCR 共识别 {} 个，标签: {:?}",
                    i + 1,
                    ws.len(),
                    ws.iter().map(|w| w.label.as_str()).collect::<Vec<_>>()
                );
                let ann = ocr_layout::annotate(fr, &ws);
                self.save_debug(&format!("window_after_{}_annot.png", i + 1), &ann);
            }
        }
        let img_after = after_frames[1].clone();
        let after = self.read_count(&img_after)?;

        Ok(VerifyResult {
            before,
            after,
            label: label.to_string(),
        })
    }

    /// 调试辅助：仅做 OCR 识别，不点击（避免在调试模式下重复分析或副作用）。
    fn analyze_for_debug(&mut self, img: &RgbImage) -> Vec<Widget> {
        self.analyzer.analyze(img).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocr_layout::WidgetSource;

    fn mk(id: usize, label: &str, area: f32) -> Widget {
        Widget {
            id,
            label: label.to_string(),
            rect: (0, 0, 10, 10),
            color: [0, 0, 0],
            area_ratio: area,
            source: WidgetSource::Color,
        }
    }

    #[test]
    fn exact_match_beats_substring() {
        // "Load" 应精确命中 Load，而非被 "Reload" 的子串包含抢走。
        let ws = [mk(0, "Reload", 0.05), mk(1, "Load", 0.04)];
        let hit = find_widget_by_label(&ws, "Load").expect("应命中");
        assert_eq!(hit.label, "Load");
    }

    #[test]
    fn substring_fallback_picks_largest() {
        // 没有精确 "load" 时，退回子串包含，选面积更大的 Reload。
        let ws = [mk(0, "Reload", 0.05), mk(1, "Unrelated", 0.01)];
        let hit = find_widget_by_label(&ws, "load").expect("应命中");
        assert_eq!(hit.label, "Reload");
    }

    #[test]
    fn no_match_returns_none() {
        let ws = [mk(0, "Reload", 0.05)];
        assert!(find_widget_by_label(&ws, "Save").is_none());
    }

    #[test]
    fn empty_label_ignored() {
        let ws = [mk(0, "", 0.05), mk(1, "Load", 0.04)];
        let hit = find_widget_by_label(&ws, "load").expect("应命中 Load");
        assert_eq!(hit.label, "Load");
    }

    #[test]
    fn verify_result_delta() {
        // 正常：点击后 − 点击前。
        let r = VerifyResult {
            before: Some(0),
            after: Some(50),
            label: "Reload".to_string(),
        };
        assert_eq!(r.delta(), Some(50));

        // 任一侧未识别到数字 → None。
        let r = VerifyResult {
            before: None,
            after: Some(50),
            label: "Reload".to_string(),
        };
        assert_eq!(r.delta(), None);

        // 计数器下降也是正常差值（如 Decrement 按钮）。
        let r = VerifyResult {
            before: Some(10),
            after: Some(9),
            label: "Decrement".to_string(),
        };
        assert_eq!(r.delta(), Some(-1));
    }

    #[test]
    fn infer_offset_recovers_planted_window() {
        use image::Rgb;
        // 全屏 200×160，底色灰；在 (40, 48)（8 的整数倍，便于降采样精确还原）
        // 处贴一块 80×60 的**带纹理**窗。关键是：全屏里的贴块必须是窗口块的「逐
        // 像素拷贝」（用窗口相对坐标算纹理），否则平移后图案不严格相等，匹配会在
        // 多个位置都接近。纯色窗也会因 SSD=0 多处重合而无法唯一确定。
        let off_x = 40u32;
        let off_y = 48u32;
        let tex = |x: u32, y: u32| -> u8 { ((x * 3 + y * 7) % 200) as u8 };

        let mut full = RgbImage::from_pixel(200, 160, Rgb([80, 80, 80]));
        for wy in 0..60u32 {
            for wx in 0..80u32 {
                let fx = off_x + wx;
                let fy = off_y + wy;
                let v = tex(wx, wy);
                full.put_pixel(fx, fy, Rgb([v, v, 230]));
            }
        }
        // 窗口流 = 同一带纹理块（窗口相对坐标）。
        let mut window = RgbImage::from_pixel(80, 60, Rgb([0, 0, 0]));
        for wy in 0..60u32 {
            for wx in 0..80u32 {
                let v = tex(wx, wy);
                window.put_pixel(wx, wy, Rgb([v, v, 230]));
            }
        }
        let (ox, oy) = infer_window_offset(&full, &window);
        assert_eq!((ox, oy), (40, 48), "应反推出窗口真实偏移");
    }
}
