//! 筛选参数：对齐 VideoSubFinder 的全局参数默认值（SSAlgorithms.cpp）。

/// 筛选参数。
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// 字幕帧长度 / 滑动窗口（`g_DL`）。
    pub dl: usize,
    /// 水平条带高度（`g_segh`）。
    pub segh: usize,
    /// 文字占比阈值（`g_tp`）。
    pub tp: f32,
    /// 最小文字长度（百分比，`g_mtpl`）。
    pub mtpl: f32,
    /// 跨帧文字差异阈值（`g_veple`）。
    pub veple: f32,
    /// ILA 差异阈值（`g_ilaple`）。
    pub ilaple: f32,
    /// 字幕最短持续（帧数，`g_max_dl_down`）。
    pub max_dl_down: usize,
    /// 字幕最长持续（帧数，`g_max_dl_up`）。
    pub max_dl_up: usize,
    /// ROI 字幕区 y 范围（`ymin`/`ymax`），None 则全图。
    pub roi_y: Option<(usize, usize)>,
    // ---- GetTransformedImage / ColorFiltration 相关（IPAlgorithms.cpp）----
    /// 颜色滤波：单段最大色差阈值（`g_scd`）。
    pub scd: i32,
    /// 颜色滤波：水平分段宽度（`g_segw`）。
    pub segw: usize,
    /// 颜色滤波：连续达标段数（`g_msegc`）。
    pub msegc: usize,
    /// 最小字幕高度比例（`g_min_h`，相对全图高 H）。
    pub min_h: f32,
    /// M-edge 组合阈值（`g_mthr`）。
    pub mthr: f32,
    /// N/H-edge 中等阈值（`g_mnthr`）。
    pub mnthr: f32,
    // ---- SecondFiltration / FilterTransformedImage 相关 ----
    /// 文字带间最大距离比例（`g_btd`）。
    pub btd: f32,
    /// 最大文字偏移比例（`g_to`）。
    pub to: f32,
    /// 最小边缘点数（`g_mpn`）。
    pub mpn: usize,
    /// 最小点密度比例（`g_mpd`）。
    pub mpd: f32,
    /// 最小边缘密度比例（`g_mpned`）。
    pub mpned: f32,
    /// 最小字符高度比例（`g_msh`）。
    pub msh: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            dl: 6,
            segh: 3,
            tp: 0.3,
            mtpl: 0.022,
            veple: 0.30,
            ilaple: 0.30,
            max_dl_down: 20,
            max_dl_up: 40,
            roi_y: None,
            scd: 800,
            segw: 8,
            msegc: 2,
            min_h: 12.0 / 720.0,
            mthr: 0.4,
            mnthr: 0.3,
            btd: 0.05,
            to: 0.1,
            mpn: 50,
            mpd: 0.3,
            mpned: 0.3,
            msh: 0.01,
        }
    }
}
