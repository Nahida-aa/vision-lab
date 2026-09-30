//! 基础像素级图像算子（复刻 VideoSubFinder `IPAlgorithms.cpp` / `SSAlgorithms.cpp`）。
//!
//! 输入输出统一用**行优先一维 buffer**（与 C++ `simple_buffer` 同构）：
//! - `Vec<u8>`：单通道灰度（0-255，白=255），或 3 通道 BGR 交错（`w*h*3`）。
//! - `Vec<u16>`：Sobel 边缘图 / ILA（Inter-Line Analysis）时间图。
//!
//! 索引 `i = y*w + x`；3 通道像素在 `[i*3, i*3+3)`。

use opencv::prelude::*;

use super::params::Params;

// geometry 的 SIMD 图像算子（Sobel 边缘，AVX2 快路径 + 标量回退）。
use geometry::imgproc as gimg;

/// 控制帧内并行度（std::thread::scope）：默认关闭——每个 worker 已经用多线程做
/// 帧间并行，帧内再切 3-5 线程只有 ~0.2ms 的 Sobel 活，spawn/同步/L2 抖动反而
/// 拖慢（实测整视频 8.2s→7.0s）。SF_PARALLEL=1 可强制开启（对照用）。
pub fn intra_frame_parallel() -> bool {
    use std::sync::Once;
    static VAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = VAL.set(std::env::var("SF_PARALLEL").map_or(false, |v| v == "1"));
    });
    *VAL.get().unwrap_or(&false)
}

/// 仅并行 im_ff 的 Thr1/Thr2 两条独立 cmoe 路径。两条路径互不依赖，但实测即使
/// pipe-only 也仅 4.79s→4.69s（~2%）：thr 是内存带宽绑定而非 CPU 绑定，多线程
/// 抢带宽几乎无收益，整视频跑反而因与主线程争带宽变慢。默认关，SF_THR=1 开。
pub fn parallel_thr() -> bool {
    use std::sync::Once;
    static VAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = VAL.set(std::env::var("SF_THR").map_or(false, |v| v == "1"));
    });
    *VAL.get().unwrap_or(&false)
}

/// 轻量分阶段计时器（供性能剖析）。默认不开启（开销为 0）。
#[derive(Default, Clone)]
pub struct Profiler {
    pub enabled: bool,
    pub color_filtration_ms: f64,
    pub bgr_to_yuv_ms: f64,
    pub im_ff_ms: f64,
    pub im_ne_he_ms: f64,
    pub filter_ms: f64,
    pub analyse_ms: f64,
    /// find_and_apply_local_thresholding（直方图阈值化）的累计耗时（im_ff 内）。
    pub thr_ms: f64,
    /// 主线程状态机串行路径拆分（合计已并入 analyse_ms）。
    pub sm_intersect_ms: f64,
    pub sm_compare_ms: f64,
    pub sm_analize_ms: f64,
    pub sm_other_ms: f64,
}

/// 状态机串行路径（主线程）耗时累加器，按类别分列。
/// 主线程同一时刻只会落一个类别，用 thread_local Cell 极轻量累计。
#[derive(Default, Clone, Copy)]
pub struct StateTimers {
    pub intersect_ms: f64,
    pub compare_ms: f64,
    pub analize_ms: f64,
    pub advance_ms: f64,
    pub other_ms: f64,
}

thread_local! {
    static SM_TIMERS: std::cell::Cell<StateTimers> = std::cell::Cell::new(StateTimers::default());
}

/// 进入某类别计时，返回 Guard：drop/结束计时累计回该类别。
pub struct SmScope {
    cat: u8,
    t0: std::time::Instant,
}

const SM_INT: u8 = 1;
const SM_CMP: u8 = 2;
const SM_ANA: u8 = 3;
const SM_ADV: u8 = 4;

/// 开始一段时长测量；`None` 表示不采集（阈值为 0 时的开销路径）。
pub fn sm_begin(cat: SmCat) -> SmScope {
    let _ = std::hint::black_box(());
    SmScope { cat: cat as u8, t0: std::time::Instant::now() }
}

pub enum SmCat {
    Intersect,
    Compare,
    Analize,
    Advance,
}

impl Drop for SmScope {
    fn drop(&mut self) {
        let d = self.t0.elapsed().as_secs_f64() * 1000.0;
        SM_TIMERS.with(|c| {
            let mut t = c.get();
            match self.cat {
                SM_INT => t.intersect_ms += d,
                SM_CMP => t.compare_ms += d,
                SM_ANA => t.analize_ms += d,
                SM_ADV => t.advance_ms += d,
                _ => t.other_ms += d,
            }
            c.set(t);
        });
    }
}

/// 读取并清零主线程状态机计时。
pub fn sm_take() -> StateTimers {
    SM_TIMERS.with(|c| c.replace(StateTimers::default()))
}

/// 把主线程状态机计时并入当前 Profiler 字段（供 FrameCache::profiler 汇总）。
pub fn sm_merge_into(pf: &mut Profiler) {
    let t = sm_take();
    pf.analyse_ms += t.intersect_ms + t.compare_ms + t.analize_ms + t.advance_ms + t.other_ms;
    pf.sm_intersect_ms += t.intersect_ms;
    pf.sm_compare_ms += t.compare_ms;
    pf.sm_analize_ms += t.analize_ms;
    pf.sm_other_ms += t.advance_ms + t.other_ms;
}

impl Profiler {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn enable(&mut self) {
        self.enabled = true;
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn total_ms(&self) -> f64 {
        self.color_filtration_ms
            + self.bgr_to_yuv_ms
            + self.im_ff_ms
            + self.im_ne_he_ms
            + self.filter_ms
            + self.analyse_ms
    }
    /// 打印各阶段累计耗时。
    pub fn dump(&self, frames: usize) {
        println!("[subtitle_finder] 逐帧转换耗时剖析（{} 帧）：", frames);
        println!("  color_filtration : {:8.1} ms", self.color_filtration_ms);
        println!("  bgr_to_yuv       : {:8.1} ms", self.bgr_to_yuv_ms);
        println!("  im_ff(边缘+阈值) : {:8.1} ms", self.im_ff_ms);
        println!("    └ 直方图阈值化 : {:8.1} ms", self.thr_ms);
        println!("  im_ne+im_he      : {:8.1} ms", self.im_ne_he_ms);
        println!("  filter(连通域)   : {:8.1} ms", self.filter_ms);
        if self.sm_intersect_ms > 0.0 || self.sm_compare_ms > 0.0 || self.sm_analize_ms > 0.0 {
            println!("  状态机(主线程)  :");
            println!("    └ get_intersect: {:8.1} ms", self.sm_intersect_ms);
            println!("    └ compare      : {:8.1} ms", self.sm_compare_ms);
            println!("    └ analize      : {:8.1} ms", self.sm_analize_ms);
            println!("    └ other        : {:8.1} ms", self.sm_other_ms);
        }
        println!("  总计             : {:8.1} ms", self.total_ms());
        println!(
            "  平均/帧          : {:8.3} ms",
            self.total_ms() / frames.max(1) as f64
        );
    }
}

/// `AddTwoImages`：`ImRES = Im1`，然后把 `Im2` 中 ==255 的像素在 `ImRES` 置 255（并集）。
pub fn add_two_images(a: &[u8], b: &[u8], size: usize) -> Vec<u8> {
    let mut res = a[..size].to_vec();
    for i in 0..size {
        if b[i] == 255 {
            res[i] = 255;
        }
    }
    res
}

/// `CombineTwoImages`：把 `Im2` 非 0 的像素写入 `ImRes` 中为 0 的位置（white=255）。
/// 就地修改 `res`。
pub fn combine_two_images(res: &mut [u8], b: &[u8], white: u8) {
    for (r, &bi) in res.iter_mut().zip(b) {
        if *r == 0 && bi != 0 {
            *r = white;
        }
    }
}

/// `IntersectTwoImages`：`ImRes` 中 `Im2` 为 0 的像素置 `zero`（取公共非零区）。
/// 泛型支持 `Im2` 为任意整数类型（u8/u16 等）。就地修改 `a`（对应 C++ `ImRes`）。
pub fn intersect_two_images_inplace<T: Copy, T2: Copy + Default + PartialEq>(
    a: &mut [T],
    b: &[T2],
    zero: T,
) {
    let bz = T2::default();
    for (ai, &bi) in a.iter_mut().zip(b) {
        if bi == bz {
            *ai = zero;
        }
    }
}

/// 3×3 矩形形态学膨胀，迭代 `iters` 次（对齐 OpenCV `dilate` 默认 3×3 矩形核）。
/// 返回新图。
pub fn dilate(im: &[u8], w: usize, h: usize, iters: i32) -> Vec<u8> {
    // 用 OpenCV `cv::dilate`，与 C++ 完全一致（IPAlgorithms.cpp:1754：
    // `cv::dilate(cv_im_gr, cv_im_gr, cv::Mat(), cv::Point(-1,-1), (g_min_h*H)/2)`）。
    // 之前手写 scatter（白像素写 3×3 邻域 × iters）在 iters=6 时高达 ~19ms/帧
    // （1MB 图 × 6 次克隆 + 全图扫描），OpenCV 内部用分离核 + SIMD，快一个量级，
    // 且输出（3×3 MORPH_RECT × iters 迭代）与 C++ 逐像素一致。
    if iters <= 0 || im.len() != w * h {
        return im.to_vec();
    }
    let src = wrap_mat(h as i32, w as i32, opencv::core::CV_8UC1, im.as_ptr() as *mut u8);
    let mut dst = opencv::core::Mat::default();
    // 空 kernel = 3×3 MORPH_RECT，anchor(-1,-1)=中心，border 用 OpenCV 默认
    // （BORDER_CONSTANT + morphologyDefaultBorderValue），与 C++ `cv::dilate(...,cv::Mat(),Point(-1,-1),iters)` 一致。
    let border = opencv::imgproc::morphology_default_border_value().expect("morphologyDefaultBorderValue 失败");
    opencv::imgproc::dilate(
        &src,
        &mut dst,
        &opencv::core::Mat::default(),
        opencv::core::Point::new(-1, -1),
        iters,
        opencv::core::BORDER_CONSTANT,
        border,
    )
    .expect("cv::dilate 失败");
    // 把 Mat 数据拷回 Vec（dst 是 OpenCV 分配的，非零拷贝 wrap）。
    let mut out = vec![0u8; w * h];
    out.copy_from_slice(dst.data_bytes().expect("dilate Mat 数据"));
    out
}

/// `IntersectYImages`（单图）：`ImRes[i]` 若与 `Im2[i]` 相差超过 `g_max_dl_down/up`
/// 则清 0。`ImRes`/`Im2` 为 `u16` ILA 时间图。就地修改 `res`。
pub fn intersect_y_images(
    res: &mut [u16],
    b: &[u16],
    max_dl_down: i32,
    max_dl_up: i32,
) {
    for (r, &bi) in res.iter_mut().zip(b) {
        if *r != 0 {
            let ri = *r as i32;
            let bi = bi as i32;
            if bi < ri - max_dl_down || bi > ri + max_dl_up {
                *r = 0;
            }
        }
    }
}

/// `ColorFiltration`：按水平 `segw` 分段统计每行色差，把含字幕的行聚成带，
/// 输出 `(LB, LE, N)`——每条文字带（垂直范围）的起止行。
///
/// `bgr` 为 `w*h*3` 的 BGR 交错数据。返回 `(LB, LE)` 两数组与带数 N。
pub fn color_filtration(bgr: &[u8], w: usize, h: usize, p: &Params) -> (Vec<i32>, Vec<i32>, usize) {
    let segw = p.segw;
    let scd = p.scd;
    let msegc = p.msegc as i32;
    let mx = (w - 1) / segw; // 每行段数

    // 1) 每行标记：是否存在满足色差阈值的连续段。
    let mut line = vec![0i32; h];
    for y in 0..h {
        let ib = w * y;
        let mut cnt = 0i32;
        for nx in 0..mx {
            let ia = ib + nx * segw;
            let b0 = bgr[ia * 3] as i32;
            let g0 = bgr[ia * 3 + 1] as i32;
            let r0 = bgr[ia * 3 + 2] as i32;
            let mi = ia + segw;
            let mut dif = 0i32;
            let mut b0 = b0;
            let mut g0 = g0;
            let mut r0 = r0;
            for i in (ia + 1)..=mi {
                let b1 = bgr[i * 3] as i32;
                let g1 = bgr[i * 3 + 1] as i32;
                let r1 = bgr[i * 3 + 2] as i32;
                dif += (r1 - r0).abs() + (g1 - g0).abs() + (b1 - b0).abs();
                r0 = r1;
                g0 = g1;
                b0 = b1;
            }
            if dif >= scd {
                cnt += 1;
            } else {
                cnt = 0;
            }
            if cnt == msegc {
                line[y] = 1;
                break;
            }
        }
    }

    // 2) 把连续的行聚成段 (lb, le)。
    let mut raw_lb = Vec::new();
    let mut raw_le = Vec::new();
    let mut sbegin = true;
    let mut y = 0usize;
    let mut last_y = 0usize;
    while y < h {
        if line[y] == 1 {
            if sbegin {
                raw_lb.push(y as i32);
                sbegin = false;
            }
        } else if !sbegin {
            raw_le.push(y as i32 - 1);
            sbegin = true;
        }
        last_y = y;
        y += 1;
    }
    if !sbegin {
        raw_le.push(last_y as i32);
    }
    let n = raw_lb.len();
    if n == 0 {
        return (vec![], vec![], 0);
    }

    // 3) 合并太近的段，扩展上下边界。
    let dd = 12i32;
    let bd = 2 * dd;
    let md = (p.min_h * h as f32) as i32;

    let mut lb = vec![0i32; n];
    let mut le = vec![0i32; n];
    let mut k = 0usize;
    lb[0] = (raw_lb[0] - dd).max(0);
    for i in 0..(n - 1) {
        if (raw_lb[i + 1] - raw_le[i] - 1) >= bd {
            if (raw_le[i] - lb[k]) >= md {
                le[k] = raw_le[i] + dd;
                k += 1;
                lb[k] = raw_lb[i + 1] - dd;
            } else {
                lb[k] = raw_lb[i + 1] - dd;
            }
        }
    }
    if (raw_le[n - 1] - lb[k]) >= md {
        le[k] = (raw_le[n - 1] + dd).min(h as i32 - 1);
        k += 1;
    }

    lb.truncate(k);
    le.truncate(k);
    (lb, le, k)
}

/// `FindAndApplyLocalThresholding`：按 (dw,dh) 分块局部阈值二值化（<thr→0）。就地。
fn find_and_apply_local_thresholding(im: &mut [u16], dw: usize, dh: usize, w: usize, h: usize) {
    const MAX_EDGE_STR: usize = 11 * 16 * 256;
    // 用常量上界作为块的初始 min，省掉一次 O(w*h) 全图 max 扫描（原实现
    // `im.iter().max()`）。edge_str 以 MAX_EDGE_STR 为界，边缘值必 < 它，安全。
    let mxx = MAX_EDGE_STR - 1;

    let mx = w / dw;
    let my = h / dh;
    if mx == 0 || my == 0 {
        return;
    }
    let da = dh * w - mx * dw;
    let di = w - dw;

    let mut edge_str = vec![0i32; MAX_EDGE_STR];
    let mut ia = 0usize;
    for _ny in 0..my {
        for _nx in 0..mx {
            let (mut min, mut max) = (mxx, 0usize);
            let mut i = ia;
            for _y in 0..dh {
                for _x in 0..dw {
                    let val = im[i] as usize;
                    if val == 0 {
                        i += 1;
                        continue;
                    }
                    if val > max {
                        max = val;
                    }
                    if val < min {
                        min = val;
                    }
                    edge_str[val] += 1;
                    i += 1;
                }
                i += di;
            }
            let mid = (min + max) / 2;
            let mut li = min;
            let mut lmax = edge_str[li];
            for v in min..mid {
                if edge_str[v] > lmax {
                    li = v;
                    lmax = edge_str[li];
                }
            }
            let mut ri = mid;
            let mut rmax = edge_str[ri];
            for v in mid..=max {
                if edge_str[v] > rmax {
                    ri = v;
                    rmax = edge_str[ri];
                }
            }
            let (mut thr, mut val);
            if lmax < rmax {
                thr = li;
                val = lmax;
            } else {
                thr = ri;
                val = rmax;
            }
            for v in (li + 1)..ri {
                if edge_str[v] < val {
                    thr = v;
                    val = edge_str[v];
                }
            }
            i = ia;
            // 阈值应用：块连续（di==0，即 dw==w）时用 SIMD，否则逐像素。
            if di == 0 {
                gimg::zero_below_threshold(&mut im[ia..ia + dh * dw], thr as u16);
            } else {
                for _y in 0..dh {
                    for _x in 0..dw {
                        if im[i] < thr as u16 {
                            im[i] = 0;
                        }
                        i += 1;
                    }
                    i += di;
                }
            }
            edge_str[..=max].iter_mut().for_each(|e| *e = 0);
            ia += dw;
        }
        ia += da;
    }

    // 处理底部剩余行。
    let rem = h % dh;
    if rem == 0 {
        return;
    }
    let ia0 = (h - rem) * w;
    for nx in 0..mx {
        let ia = ia0 + nx * dw;
        let (mut min, mut max) = (mxx, 0usize);
        let mut i = ia;
        for _y in 0..rem {
            for _x in 0..dw {
                let val = im[i] as usize;
                if val == 0 {
                    i += 1;
                    continue;
                }
                if val > max {
                    max = val;
                }
                if val < min {
                    min = val;
                }
                edge_str[val] += 1;
                i += 1;
            }
            i += di;
        }
        let mid = (min + max) / 2;
        let mut li = min;
        let mut lmax = edge_str[li];
        for v in min..mid {
            if edge_str[v] > lmax {
                li = v;
                lmax = edge_str[li];
            }
        }
        let mut ri = mid;
        let mut rmax = edge_str[ri];
        for v in mid..=max {
            if edge_str[v] > rmax {
                ri = v;
                rmax = edge_str[ri];
            }
        }
        let (mut thr, mut val);
        if lmax < rmax {
            thr = li;
            val = lmax;
        } else {
            thr = ri;
            val = rmax;
        }
        for v in (li + 1)..ri {
            if edge_str[v] < val {
                thr = v;
                val = edge_str[v];
            }
        }
        i = ia;
        // 阈值应用：块连续（di==0）时用 SIMD。
        if di == 0 {
            gimg::zero_below_threshold(&mut im[ia..ia + rem * dw], thr as u16);
        } else {
            for _y in 0..rem {
                for _x in 0..dw {
                    if im[i] < thr as u16 {
                        im[i] = 0;
                    }
                    i += 1;
                }
                i += di;
            }
        }
        edge_str[..=max].iter_mut().for_each(|e| *e = 0);
    }
}

/// `BorderClear`：清边界 dd 像素宽。
fn border_clear<T: Copy + Default>(im: &mut [T], dd: usize, w: usize, h: usize) {
    let zero = T::default();
    // 上/下。
    for v in im[..w * dd].iter_mut() {
        *v = zero;
    }
    for v in im[w * (h - dd)..].iter_mut() {
        *v = zero;
    }
    // 左/右（每行前/后 dd 像素）。
    for y in 0..h {
        let row = y * w;
        for x in 0..dd {
            im[row + x] = zero;
            im[row + w - dd + x] = zero;
        }
    }
}

/// `EasyBorderClear`：清最外一圈像素。
fn easy_border_clear<T: Copy + Default>(im: &mut [T], w: usize, h: usize) {
    let zero = T::default();
    // 首/末行。
    for v in im[..w].iter_mut() {
        *v = zero;
    }
    for v in im[w * (h - 1)..].iter_mut() {
        *v = zero;
    }
    // 首/末列。
    for y in 0..h {
        im[y * w] = zero;
        im[y * w + w - 1] = zero;
    }
}

/// `GetImCMOEWithThr1`：组合边缘（Y+U+V）+ 局部阈值 + ESS/ECP 增强。
/// `offsets`/`dhs` 描述每带在拼接 buffer 中的偏移/高度。返回拼接后的 u16 图。
#[allow(clippy::too_many_arguments)]
fn get_im_cmoe_with_thr1(
    y_moe: &[u16],
    u_moe: &[u16],
    v_moe: &[u16],
    w: usize,
    h: usize,
    offsets: &[usize],
    dhs: &[usize],
    mthr: f32,
    prof: Option<&mut Profiler>,
) -> Vec<u16> {
    let mut prof = prof;
    let mx = w - 1;
    let my = h - 1;
    let mut cmoe = vec![0u16; w * h];
    easy_border_clear(&mut cmoe, w, h);
    let mut i = w + 1;
    for _y in 1..my {
        for _x in 1..mx {
            cmoe[i] = y_moe[i] + u_moe[i] + v_moe[i];
            i += 1;
        }
        i += 2;
    }
    let t_thr = std::time::Instant::now();
    find_and_apply_local_thresholding(&mut cmoe, w, 32, w, h);
    if let Some(pf) = prof.as_deref_mut() {
        pf.thr_ms += t_thr.elapsed().as_secs_f64() * 1000.0;
    }
    let res2 = gimg::aply_ess(&cmoe, w, h);
    let mut res2 = res2;
    border_clear(&mut res2, 2, w, h);
    let res3 = gimg::aply_ecp(&res2, w, h);
    border_clear(&mut cmoe, 2, w, h);
    let mx = w - 2;
    let my = h - 2;
    let mut i = (w + 1) << 1;
    for _y in 2..my {
        for _x in 2..mx {
            cmoe[i] = ((res2[i] + res3[i]) / 2) as u16;
            i += 1;
        }
        i += 4;
    }
    // 每带局部阈值。
    for (k, &off) in offsets.iter().enumerate() {
        let d = dhs[k];
        let sub = &mut cmoe[off..off + w * d];
        gimg::apply_moderate_threshold(sub, mthr);
    }
    cmoe
}

/// `GetImCMOEWithThr2`：组合边缘（Y+(U+V)*5）+ 局部阈值 + ESS/ECP 增强。
#[allow(clippy::too_many_arguments)]
fn get_im_cmoe_with_thr2(
    y_moe: &[u16],
    u_moe: &[u16],
    v_moe: &[u16],
    w: usize,
    h: usize,
    offsets: &[usize],
    dhs: &[usize],
    mthr: f32,
    prof: Option<&mut Profiler>,
) -> Vec<u16> {
    let mut prof = prof;
    let mx = w - 1;
    let my = h - 1;
    let mut cmoe = vec![0u16; w * h];
    easy_border_clear(&mut cmoe, w, h);
    let mut i = w + 1;
    for _y in 1..my {
        for _x in 1..mx {
            cmoe[i] = y_moe[i] + (u_moe[i] + v_moe[i]) * 5;
            i += 1;
        }
        i += 2;
    }
    let t_thr = std::time::Instant::now();
    find_and_apply_local_thresholding(&mut cmoe, w, 32, w, h);
    if let Some(pf) = prof.as_deref_mut() {
        pf.thr_ms += t_thr.elapsed().as_secs_f64() * 1000.0;
    }
    let res5 = gimg::aply_ess(&cmoe, w, h);
    let mut res5 = res5;
    border_clear(&mut res5, 2, w, h);
    let res6 = gimg::aply_ecp(&res5, w, h);
    border_clear(&mut cmoe, 2, w, h);
    let mx = w - 2;
    let my = h - 2;
    let mut i = (w + 1) << 1;
    for _y in 2..my {
        for _x in 2..mx {
            cmoe[i] = ((res5[i] as i32 + res6[i] as i32) / 2) as u16;
            i += 1;
        }
        i += 4;
    }
    for (k, &off) in offsets.iter().enumerate() {
        let d = dhs[k];
        let sub = &mut cmoe[off..off + w * d];
        gimg::apply_moderate_threshold(sub, mthr);
    }
    cmoe
}

/// 逐帧颜色/边缘变换：BGR 帧 → `(ImFF, ImSF, ImTF, ImNE, ImY, LB, LE, N)`。
///
/// 复刻 `GetTransformedImage`（IPAlgorithms.cpp 1796）：
/// 1. `ColorFiltration` 找文字带 (LB,LE,N)。
/// 2. BGR → YUV 拆出 Y/U/V。
/// 3. 并行 `GetImFF`/`GetImNE`/`GetImHE`。
/// 4. NE 与 HE 并集（`CombineTwoImages`）。
///
/// `bgr` 为 `w*h*3` BGR 交错；`W/H` 为含缩放的全图尺寸（此处用 w/h 即可）。
/// 返回 `(im_ff, im_sf, im_tf, im_ne, im_y, lb, le, n, has_text)`，其中 `lb/le` 为
/// segh 对齐后的带边界，`im_tf` 为 `FilterTransformedImage` 过滤后的文字图，
/// `has_text` 为过滤结果（1 = 有文字）。
#[allow(clippy::type_complexity)]
pub fn get_transformed_image(
    bgr: &[u8],
    w: usize,
    h: usize,
    p: &Params,
    prof: Option<&mut Profiler>,
) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<i32>, Vec<i32>, usize, i32) {
    let mut prof = prof;
    let t0 = std::time::Instant::now();
    // 1) 颜色滤波找文字带。
    let (lb, le, n) = color_filtration(bgr, w, h, p);
    if let Some(pf) = prof.as_deref_mut() {
        pf.color_filtration_ms += t0.elapsed().as_secs_f64() * 1000.0;
    }
    if n == 0 {
        return (
            vec![0; w * h],
            vec![0; w * h],
            vec![0; w * h],
            vec![0; w * h],
            vec![0; w * h],
            lb,
            le,
            n,
            0,
        );
    }

    let t0 = std::time::Instant::now();
    // 2) BGR → Y/U/V（OpenCV BGR2YUV 的整数公式）。
    let (mut im_y, mut im_u, mut im_v) = (vec![0u8; w * h], vec![0u8; w * h], vec![0u8; w * h]);
    bgr_to_yuv(bgr, &mut im_y, &mut im_u, &mut im_v, w, h);
    if let Some(pf) = prof.as_deref_mut() {
        pf.bgr_to_yuv_ms += t0.elapsed().as_secs_f64() * 1000.0;
    }

    let t0 = std::time::Instant::now();
    // 3) GetImFF / GetImNE / GetImHE（C++ `run_in_parallel`；这里也用 3 线程并行）。
    let (im_ff, im_sf, lb_a, le_a, im_ne, im_he) = if intra_frame_parallel() {
        std::thread::scope(|s| {
            let y = &im_y;
            let u = &im_u;
            let v = &im_v;
            let lb = &lb;
            let le = &le;
            let h1 = s.spawn(move || get_im_ff(y, u, v, lb, le, n, w, h, p, None));
            let h2 = s.spawn(move || get_im_ne(y, u, v, w, h, p));
            let h3 = s.spawn(move || get_im_he(y, u, v, w, h, p));
            let (ff, sf, lba, lea) = h1.join().expect("get_im_ff 线程失败");
            let ne = h2.join().expect("get_im_ne 线程失败");
            let he = h3.join().expect("get_im_he 线程失败");
            (ff, sf, lba, lea, ne, he)
        })
    } else {
        let (ff, sf, lba, lea) = get_im_ff(&im_y, &im_u, &im_v, &lb, &le, n, w, h, p, None);
        let ne = get_im_ne(&im_y, &im_u, &im_v, w, h, p);
        let he = get_im_he(&im_y, &im_u, &im_v, w, h, p);
        (ff, sf, lba, lea, ne, he)
    };
    if let Some(pf) = prof.as_deref_mut() {
        pf.im_ff_ms += t0.elapsed().as_secs_f64() * 1000.0;
    }

    tracing::trace!(
        ff_wc = im_ff.iter().filter(|&&v| v == 255).count(),
        ne_wc = im_ne.iter().filter(|&&v| v == 255).count(),
        he_wc = im_he.iter().filter(|&&v| v == 255).count(),
        "get_transformed_image: 边缘图白点"
    );
    let sf_26 = im_sf[26 * w..45 * w].iter().filter(|&&v| v == 255).count();
    let ff_26 = im_ff[26 * w..45 * w].iter().filter(|&&v| v == 255).count();
    tracing::trace!(ff_26, sf_26, "get_transformed_image: 26-44 白点");

    // 4) NE + HE 并集。
    let mut im_ne = im_ne;
    combine_two_images(&mut im_ne, &im_he, 255);

    let t0 = std::time::Instant::now();
    // 5) FilterTransformedImage → 最终 ImTF 与 has_text。
    let mut im_tf = vec![0u8; w * h];
    let mut im_sf_mut = im_sf.clone();
    let has_text = super::filter::filter_transformed_image(
        &im_ff,
        &mut im_sf_mut,
        &mut im_tf,
        &im_ne,
        &lb_a,
        &le_a,
        n,
        w,
        h,
        p,
    );
    if let Some(pf) = prof.as_deref_mut() {
        pf.filter_ms += t0.elapsed().as_secs_f64() * 1000.0;
    }

    (im_ff, im_sf_mut, im_tf, im_ne, im_y, lb_a, le_a, n, has_text)
}

/// BGR → YUV（对齐 OpenCV `COLOR_BGR2YUV` 全量程公式）。
/// 零拷贝 wrap 一段外部内存为 OpenCV Mat（不拥有数据，drop 仅释放 header）。
/// 只读场景传入 `*const u8` 转 `*mut u8`（与 `bgr_to_yuv` 既有用法一致）。
fn wrap_mat(rows: i32, cols: i32, typ: i32, data: *mut u8) -> opencv::core::Mat {
    // SAFETY: 调用方保证 data 指向 rows*cols* 字节的有效可写内存，且 Mat 生命周期
    // 不出本函数体；Mat 零拷贝 wrap 不拥有数据，drop 仅释放 header。
    unsafe {
        opencv::core::Mat::new_rows_cols_with_data_unsafe_def(
            rows,
            cols,
            typ,
            data as *mut std::ffi::c_void,
        )
        .expect("Mat wrap 失败")
    }
}

/// C++ 用 `cv::cvtColor(COLOR_BGR2YUV)`，其 Y 全量程 0-255（非 BT.601 的 16-235）。
pub fn bgr_to_yuv(bgr: &[u8], y: &mut [u8], u: &mut [u8], v: &mut [u8], w: usize, h: usize) {
    // 用 OpenCV `cvtColor(COLOR_BGR2YUV)`，保证与 C++ GetTransformedImage 完全一致。
    // 之前用浮点公式（Y=0.299R+0.587G+0.114B 等），V 通道在个别像素与 OpenCV 的
    // 整数定点实现差 ±1 → FF 阈值边缘像素判定不同 → 幽灵带 → 字幕段丢失。
    // 见 docs/cpp-alignment-notes.md「六、未解决的已知差异（幽灵带）」。
    //
    // 性能：旧实现每帧把 bgr 拷进 Mat、再手动逐像素解交错 YUV，开销比 OpenCV
    // 单算子多 ~3×（0.66ms vs 0.2ms）。这里改用零拷贝 wrap 输入 + `cv::split`
    // 一次写满 y/u/v 三通道（split 用 OpenCV 内部 SIMD 解交错），避免逐像素循环。
    // 零拷贝 wrap 输入 bgr（不开 copy，Mat drop 不释放外部数据）。
    let src = wrap_mat(h as i32, w as i32, opencv::core::CV_8UC3, bgr.as_ptr() as *mut u8);
    let mut dst = opencv::core::Mat::default();
    opencv::imgproc::cvt_color_def(&src, &mut dst, opencv::imgproc::COLOR_BGR2YUV)
        .expect("cvtColor 失败");
    let mut mv_vec = opencv::core::Vector::<opencv::core::Mat>::from_iter([
        wrap_mat(h as i32, w as i32, opencv::core::CV_8UC1, y.as_mut_ptr()),
        wrap_mat(h as i32, w as i32, opencv::core::CV_8UC1, u.as_mut_ptr()),
        wrap_mat(h as i32, w as i32, opencv::core::CV_8UC1, v.as_mut_ptr()),
    ]);
    opencv::core::split(&dst, &mut mv_vec).expect("core::split 失败");
}

/// `GetImFF`：Sobel M-edge + 局部/组合阈值，输出前景文字二值图 + 对齐带边界。
#[allow(clippy::too_many_arguments)]
pub fn get_im_ff(
    y_full: &[u8],
    u_full: &[u8],
    v_full: &[u8],
    lb_in: &[i32],
    le_in: &[i32],
    n: usize,
    w: usize,
    h: usize,
    p: &Params,
    prof: Option<&mut Profiler>,
) -> (Vec<u8>, Vec<u8>, Vec<i32>, Vec<i32>) {
    let mut prof = prof;
    // 拼接文字带区域。
    let mut offsets = vec![0usize; n];
    let mut dhs = vec![0usize; n];
    let mut cnts = vec![0usize; n];
    let mut hh = 0usize;
    let mut i = 0usize;
    for k in 0..n {
        offsets[k] = i;
        dhs[k] = (le_in[k] - lb_in[k] + 1) as usize;
        cnts[k] = w * dhs[k];
        i += cnts[k];
        hh += dhs[k];
    }
    let ww = w;

    // 拼接 Y/U/V 带。
    let mut im_y = vec![0u8; ww * hh];
    let mut im_u = vec![0u8; ww * hh];
    let mut im_v = vec![0u8; ww * hh];
    for k in 0..n {
        let src_off = w * lb_in[k] as usize;
        im_y[offsets[k]..offsets[k] + cnts[k]].copy_from_slice(&y_full[src_off..src_off + cnts[k]]);
        im_u[offsets[k]..offsets[k] + cnts[k]].copy_from_slice(&u_full[src_off..src_off + cnts[k]]);
        im_v[offsets[k]..offsets[k] + cnts[k]].copy_from_slice(&v_full[src_off..src_off + cnts[k]]);
    }

    // 每通道 M-edge（Sobel）：三条独立路径但单遍仅 ~0.2ms，spawn 不划算，串行走。
    let (y_moe, u_moe, v_moe) = if intra_frame_parallel() {
        std::thread::scope(|s| {
            let l1 = s.spawn(|| gimg::sobel_m_edge(&im_y, ww, hh));
            let l2 = s.spawn(|| gimg::sobel_m_edge(&im_u, ww, hh));
            let l3 = s.spawn(|| gimg::sobel_m_edge(&im_v, ww, hh));
            (
                l1.join().expect("sobel_m_edge(Y) 线程失败"),
                l2.join().expect("sobel_m_edge(U) 线程失败"),
                l3.join().expect("sobel_m_edge(V) 线程失败"),
            )
        })
    } else {
        (
            gimg::sobel_m_edge(&im_y, ww, hh),
            gimg::sobel_m_edge(&im_u, ww, hh),
            gimg::sobel_m_edge(&im_v, ww, hh),
        )
    };

    // 组合阈值（Thr1 / Thr2）——两条独立路径：默认并行（2 个 scoped 线程，合计
    // ~8ms/帧的 cmoe 全幅工作 → ~4ms），对齐 C++ 内层 run_in_parallel。SF_THR=0 串行。
    // 线程内传 None 避免 prof（&mut）跨线程借用；此处按并行区墙时累计 thr_ms（近似）。
    let t_thr = std::time::Instant::now();
    let (res1, res4) = if parallel_thr() {
        std::thread::scope(|s| {
            let t1 = s.spawn(|| {
                get_im_cmoe_with_thr1(&y_moe, &u_moe, &v_moe, ww, hh, &offsets, &dhs, p.mthr, None)
            });
            let t2 = s.spawn(|| {
                get_im_cmoe_with_thr2(&y_moe, &u_moe, &v_moe, ww, hh, &offsets, &dhs, p.mthr, None)
            });
            (
                t1.join().expect("get_im_cmoe_with_thr1 线程失败"),
                t2.join().expect("get_im_cmoe_with_thr2 线程失败"),
            )
        })
    } else {
        (
            get_im_cmoe_with_thr1(&y_moe, &u_moe, &v_moe, ww, hh, &offsets, &dhs, p.mthr, None),
            get_im_cmoe_with_thr2(&y_moe, &u_moe, &v_moe, ww, hh, &offsets, &dhs, p.mthr, None),
        )
    };
    if let Some(pf) = prof.as_deref_mut() {
        pf.thr_ms += t_thr.elapsed().as_secs_f64() * 1000.0;
    }

    // 写回全图 ImFF。
    let mut im_ff = vec![0u8; w * h];
    for k in 0..n {
        let dst_off = w * lb_in[k] as usize;
        for j in 0..cnts[k] {
            im_ff[dst_off + j] = if res1[offsets[k] + j] != 0 || res4[offsets[k] + j] != 0 {
                255
            } else {
                0
            };
        }
    }

    // ImSF 初始 = ImFF。
    let mut im_sf = im_ff.clone();

    // 对齐 LB/LE 到 segh 边界（C++ 中 GetImFF 就地改 LB/LE）。
    let segh = p.segh;
    let mut lb = lb_in.to_vec();
    let mut le = le_in.to_vec();
    for k in 0..n {
        let val = (lb[k] % segh as i32).abs();
        lb[k] -= val;
        let val = (le[k] % segh as i32).abs();
        let val = if val > 0 { segh as i32 - val } else { 0 };
        if le[k] + val < h as i32 {
            le[k] += val;
        }
    }
    if le[n - 1] as usize + p.segh > h {
        let _val = (le[n - 1] - (h as i32 - p.segh as i32)) as usize;
        le[n - 1] = (h - p.segh) as i32;
        // ImSF 中 (h-segh) 行以下的像素清零。
        let start = w * (le[n - 1] as usize + 1);
        for v in im_sf[start..].iter_mut() {
            *v = 0;
        }
    }

    (im_ff, im_sf, lb, le)
}

/// `GetImNE`：垂直边缘（N-edge）组合阈值二值化。
pub fn get_im_ne(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize, p: &Params) -> Vec<u8> {
    let mut im_ne = vec![0u8; w * h];
    easy_border_clear(&mut im_ne, w, h);

    let (y_noe, u_noe, v_noe) = if intra_frame_parallel() {
        std::thread::scope(|s| {
            let l1 = s.spawn(|| gimg::sobel_n_edge(y, w, h));
            let l2 = s.spawn(|| gimg::sobel_n_edge(u, w, h));
            let l3 = s.spawn(|| gimg::sobel_n_edge(v, w, h));
            (
                l1.join().expect("sobel_n_edge(Y) 线程失败"),
                l2.join().expect("sobel_n_edge(U) 线程失败"),
                l3.join().expect("sobel_n_edge(V) 线程失败"),
            )
        })
    } else {
        (
            gimg::sobel_n_edge(y, w, h),
            gimg::sobel_n_edge(u, w, h),
            gimg::sobel_n_edge(v, w, h),
        )
    };

    let mx = w - 1;
    let my = h - 1;
    let mut res1 = vec![0u16; w * h];
    let mut res2 = vec![0u16; w * h];
    easy_border_clear(&mut res1, w, h);
    easy_border_clear(&mut res2, w, h);
    let mut i = w + 1;
    for _y in 1..my {
        for _x in 1..mx {
            res1[i] = y_noe[i] + u_noe[i] + v_noe[i];
            res2[i] = y_noe[i] + (u_noe[i] + v_noe[i]) * 5;
            i += 1;
        }
        i += 2;
    }
    gimg::apply_moderate_threshold(&mut res1, p.mnthr);
    gimg::apply_moderate_threshold(&mut res2, p.mnthr);

    let mut i = w + 1;
    for _y in 1..my {
        for _x in 1..mx {
            im_ne[i] = if res1[i] != 0 || res2[i] != 0 { 255 } else { 0 };
            i += 1;
        }
        i += 2;
    }
    im_ne
}

/// `GetImHE`：水平边缘（H-edge）组合阈值二值化。
pub fn get_im_he(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize, p: &Params) -> Vec<u8> {
    let mut im_he = vec![0u8; w * h];
    easy_border_clear(&mut im_he, w, h);

    let (y_hoe, u_hoe, v_hoe) = if intra_frame_parallel() {
        std::thread::scope(|s| {
            let l1 = s.spawn(|| gimg::sobel_h_edge(y, w, h));
            let l2 = s.spawn(|| gimg::sobel_h_edge(u, w, h));
            let l3 = s.spawn(|| gimg::sobel_h_edge(v, w, h));
            (
                l1.join().expect("sobel_h_edge(Y) 线程失败"),
                l2.join().expect("sobel_h_edge(U) 线程失败"),
                l3.join().expect("sobel_h_edge(V) 线程失败"),
            )
        })
    } else {
        (
            gimg::sobel_h_edge(y, w, h),
            gimg::sobel_h_edge(u, w, h),
            gimg::sobel_h_edge(v, w, h),
        )
    };

    let mx = w - 1;
    let my = h - 1;
    let mut res1 = vec![0u16; w * h];
    let mut res2 = vec![0u16; w * h];
    easy_border_clear(&mut res1, w, h);
    easy_border_clear(&mut res2, w, h);
    let mut i = w + 1;
    for _y in 1..my {
        for _x in 1..mx {
            res1[i] = y_hoe[i] + u_hoe[i] + v_hoe[i];
            res2[i] = y_hoe[i] + (u_hoe[i] + v_hoe[i]) * 5;
            i += 1;
        }
        i += 2;
    }
    gimg::apply_moderate_threshold(&mut res1, p.mnthr);
    gimg::apply_moderate_threshold(&mut res2, p.mnthr);

    let mut i = w + 1;
    for _y in 1..my {
        for _x in 1..mx {
            im_he[i] = if res1[i] != 0 || res2[i] != 0 { 255 } else { 0 };
            i += 1;
        }
        i += 2;
    }
    im_he
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params::default()
    }

    #[test]
    fn add_two_images_union() {
        let a = vec![255u8, 0, 0, 255];
        let b = vec![0u8, 255, 0, 255];
        let r = add_two_images(&a, &b, 4);
        assert_eq!(r, vec![255, 255, 0, 255]);
    }

    #[test]
    fn combine_two_images_fills_zeros() {
        let mut a = vec![255u8, 0, 0];
        let b = vec![0u8, 0, 255];
        combine_two_images(&mut a, &b, 255);
        assert_eq!(a, vec![255, 0, 255]);
    }

    #[test]
    fn intersect_inplace_zeroes() {
        let mut a = vec![255u8, 100, 0, 255];
        let b = vec![0u8, 255, 0, 255];
        intersect_two_images_inplace(&mut a, &b, 0u8);
        assert_eq!(a, vec![0, 100, 0, 255]);
    }

    #[test]
    fn intersect_y_images_temporal() {
        // res=5 与 b=50 差 45 > max_dl_up(40) → 清 0。
        let mut a = vec![5u16, 20, 0];
        let b = vec![50u16, 30, 99];
        intersect_y_images(&mut a, &b, 20, 40);
        assert_eq!(a, vec![0, 20, 0]); // 20 与 30 差 10 在范围内保留；0 保持 0
    }

    #[test]
    fn color_filtration_no_text_returns_empty() {
        // 全纯色（无边缘）→ 无文字带。
        let bgr = vec![128u8; 3 * 40 * 12];
        let (lb, le, n) = color_filtration(&bgr, 40, 12, &params());
        assert_eq!(n, 0);
        assert!(lb.is_empty() && le.is_empty());
    }

    #[test]
    fn color_filtration_detects_contrast_band() {
        // 构造一行色差大的条纹（模拟文字边缘），应检出至少一条带。
        let (w, h) = (40, 12);
        let mut bgr = vec![0u8; 3 * w * h];
        // 第 5 行：黑白交替，色差大。
        for x in 0..w {
            let c: u8 = if x % 2 == 0 { 0 } else { 255 };
            let i = (5 * w + x) * 3;
            bgr[i] = c;
            bgr[i + 1] = c;
            bgr[i + 2] = c;
        }
        let (lb, le, n) = color_filtration(&bgr, w, h, &params());
        assert!(n >= 1, "应检测到文字带, n={}", n);
        // 带应覆盖第 5 行（约 0..=11 因 dd 扩展）。
        assert!(lb[0] <= 5 && 5 <= le[0]);
    }

    #[test]
    fn sobel_m_edge_detects_edges() {
        // 一条垂直边缘（左黑右白）→ M-edge 在边缘处有响应。
        let (w, h) = (8, 8);
        let mut img = vec![0u8; w * h];
        for y in 0..h {
            for x in 4..w {
                img[y * w + x] = 255;
            }
        }
        let moe = gimg::sobel_m_edge(&img, w, h);
        // 边缘在 x=4 列附近，应在内部像素有非 0 响应。
        let center = 3 * w + 4;
        assert!(moe[center] > 0, "边缘处应检测到 M-edge 响应");
        // 远离边缘处应接近 0。
        let flat = 3 * w + 1;
        assert_eq!(moe[flat], 0, "平坦区应无边缘");
    }

    #[test]
    fn apply_moderate_threshold_binarizes() {
        let mut v = vec![0u16, 100, 200, 255];
        gimg::apply_moderate_threshold(&mut v, 0.5);
        // 阈值 = 255*0.5 = 127（截断 127）。<127→0，>=127→255。
        assert_eq!(v, vec![0, 0, 255, 255]);
    }

    /// 参考实现：迭代 3×3 膨胀（对应 OpenCV cv::dilate(..., Mat(), Point(-1,-1), iters)）。
    fn dilate_reference(im: &[u8], w: usize, h: usize, iters: i32) -> Vec<u8> {
        let mut cur = im.to_vec();
        for _ in 0..iters.max(0) {
            let mut next = cur.clone();
            for y in 0..h {
                for x in 0..w {
                    if cur[y * w + x] != 0 {
                        for dy in -1..=1i32 {
                            for dx in -1..=1i32 {
                                let nx = x as i32 + dx;
                                let ny = y as i32 + dy;
                                if nx >= 0 && ny >= 0 && nx < w as i32 && ny < h as i32 {
                                    next[ny as usize * w + nx as usize] = 255;
                                }
                            }
                        }
                    }
                }
            }
            cur = next;
        }
        cur
    }

    /// 现 dilate（迭代 3×3 scatter）必须与参考实现逐像素一致（防回归）。
    #[test]
    fn dilate_matches_reference() {
        // 确定性伪随机 + 结构化用例。
        for &(w, h) in &[(3, 3), (5, 4), (17, 9), (32, 20), (64, 40)] {
            for &iters in &[1i32, 2, 3, 6] {
                // 随机稀疏点。
                let mut seed = (w * 31 + h * 7 + iters as usize) as u32;
                let mut im = vec![0u8; w * h];
                for v in im.iter_mut() {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    if (seed >> 16) % 10 == 0 {
                        *v = 255;
                    }
                }
                let r1 = dilate(&im, w, h, iters);
                let r2 = dilate_reference(&im, w, h, iters);
                assert_eq!(r1, r2, "随机用例不匹配 w={} h={} iters={}", w, h, iters);

                // 全白 / 全黑。
                let all_white = vec![255u8; w * h];
                assert_eq!(dilate(&all_white, w, h, iters), dilate_reference(&all_white, w, h, iters));
                let all_black = vec![0u8; w * h];
                assert_eq!(dilate(&all_black, w, h, iters), dilate_reference(&all_black, w, h, iters));
            }
        }
    }

    #[test]
    fn get_transformed_image_produces_ff_on_text_frame() {
        // 合成一帧：暗背景 + 中央亮色字幕块（有对比边缘），应产生 ImFF 前景。
        let (w, h) = (64, 48);
        let mut bgr = vec![20u8; 3 * w * h]; // 暗背景
        // 字幕区：第 20..32 行，x 10..54，做条纹状（模拟文字笔画，有内部对比）。
        for y in 20..32 {
            for x in 10..54 {
                let c: u8 = if (x - 10) % 3 < 2 { 230 } else { 30 };
                let i = (y * w + x) * 3;
                bgr[i] = c;
                bgr[i + 1] = c;
                bgr[i + 2] = c;
            }
        }
        let (im_ff, _im_sf, im_tf, im_ne, _im_y, lb, le, n, has_text) =
            get_transformed_image(&bgr, w, h, &params(), None);
        assert!(n >= 1, "颜色滤波应检出字幕带, n={}", n);
        // ImFF 应在字幕块内有白色像素。
        let white_count = im_ff.iter().filter(|&&v| v == 255).count();
        assert!(white_count > 0, "ImFF 应有前景像素, white={}", white_count);
        // NE 也应检出边缘。
        let ne_count = im_ne.iter().filter(|&&v| v == 255).count();
        assert!(ne_count > 0, "ImNE 应有边缘像素");
        // 带应覆盖字幕块。
        let block_y = 20i32;
        assert!(lb[0] <= block_y && block_y <= le[0], "带 {}..{} 应覆盖字幕 {}", lb[0], le[0], block_y);
        // 过滤后应检出文字。
        assert_eq!(has_text, 1, "应检出文字");
        let tf_count = im_tf.iter().filter(|&&v| v == 255).count();
        assert!(tf_count > 0, "ImTF 应有过滤后文字, white={}", tf_count);
    }
}
