//! 字幕时间轴状态机：FastSearchSubtitles。
//!
//! 复刻 VideoSubFinder 的核心状态机：用 `bf/ef`（起止帧）、`bt/et`（起止时间）、
//! `DL`、`max_dl_down/up` 跟踪字幕段，只在「字幕内容变化」时输出关键帧。
//! 这是「时间无偏移」的关键，必须精确对齐 C++ `FastSearchSubtitles`。

use std::path::Path;

use anyhow::Result;
use tracing::trace;

use crate::compare;
use crate::filter;
use crate::frame;
use crate::imgops;
use crate::params::Params;
use crate::Keyframe;

use std::collections::HashMap;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

/// 一帧的解码 + 变换产物。
#[derive(Clone)]
pub(crate) struct FrameData {
    pub bgr: Vec<u8>,
    /// 去背景字幕前景图 ImTF（ISA，0/255）。
    pub im: Vec<u8>,
    /// 边缘图 ImNE（N+HE 并集）。
    pub ne: Vec<u8>,
    /// ILA 时间图（Y+255 的 u16）。
    pub y: Vec<u16>,
    /// 帧时间（ms）。
    pub pos: i64,
    /// 是否有字幕（FilterTransformedImage 的 has_text）。
    pub has_text: bool,
}

/// 顺序解码 + 变换的滑动窗口缓存（对应 C++ `RunSearch` 的环形缓冲）。
///
/// 用 [`frame::FrameStepper`] 流式解码：状态机 `fn` 单调推进时按需 `advance_to`，
/// 窗口只保留最近若干帧（对齐 C++ `m_N ≈ DL+threads`），避免全量驻留内存
/// （否则 170s/5100 帧 ≈ 30GB 会 OOM）。
pub struct FrameCache<'a> {
    path: &'a Path,
    p: &'a Params,
    w: usize,
    h: usize,
    prof: Option<imgops::Profiler>,
    stream: Option<TransformStream<'a>>,
    /// 滑动窗口内容；索引 = fn - window_start。
    window: std::collections::VecDeque<FrameData>,
    window_start: i32,
    /// 累计解码帧数（EOF 后固定 = 视频总帧数）。
    decoded_total: i32,
    /// 视频总时长（毫秒），stream 打开时填入；0 表示未知。
    total_duration_ms: i64,
    /// 视频总帧数，stream 打开时填入；0 表示未知（进度条据此退化）。
    total_frames: i64,
}

/// 窗口保留的最大帧数（覆盖状态机访问 [fn-1, fn+3*DL]，DL=6 → ~20 帧）。
const MAX_WINDOW: i32 = 3 * 6 + 2;

/// 状态机每次推进 `fn` 时的前瞻解码帧数（覆盖 get_intersect_images 的 [fn, fn+DL-1]）。
const FORWARD: i32 = 3 * 6; // = 18

impl<'a> FrameCache<'a> {
    pub fn new(path: &'a Path, p: &'a Params) -> Self {
        Self {
            path,
            p,
            w: 0,
            h: 0,
            prof: None,
            stream: None,
            window: std::collections::VecDeque::new(),
            window_start: 0,
            decoded_total: 0,
            total_duration_ms: 0,
            total_frames: 0,
        }
    }

    /// 开启分阶段计时（性能剖析）。
    pub fn with_profiling(mut self) -> Self {
        let mut prof = imgops::Profiler::new();
        prof.enable();
        self.prof = Some(prof);
        self
    }

    /// 取剖析计时器（先汇总后台 transform worker 的耗时进 `self.prof`）。
    pub fn profiler(&mut self) -> Option<&imgops::Profiler> {
        if let Some(pf) = self.prof.as_mut() {
            if let Some(s) = self.stream.as_ref().and_then(|s| s.take_profiler_sum()) {
                pf.color_filtration_ms += s.color_filtration_ms;
                pf.bgr_to_yuv_ms += s.bgr_to_yuv_ms;
                pf.im_ff_ms += s.im_ff_ms;
                pf.im_ne_he_ms += s.im_ne_he_ms;
                pf.filter_ms += s.filter_ms;
                pf.analyse_ms += s.analyse_ms;
                pf.thr_ms += s.thr_ms;
            }
            // 主线程状态机串行路径计时。
            imgops::sm_merge_into(pf);
        }
        self.prof.as_ref()
    }

    /// 主线程在流水线上阻塞等待的累计时长（毫秒）。
    pub fn stream_wait_ms(&self) -> f64 {
        self.stream.as_ref().map(|s| s.wait_ms).unwrap_or(0.0)
    }

    /// 惰性打开变换流水线：后台解码线程 + N 个 transform worker 前瞻计算，
    /// 状态机消费已算好的 `FrameData`（对齐 C++ AddConvertImageTask 前瞻）。
    fn open_stepper(&mut self) -> Result<()> {
        if self.stream.is_none() {
            let stream = TransformStream::open(self.path, self.p, self.prof.as_mut().as_deref())?;
            self.total_duration_ms = stream.total_duration_ms();
            self.total_frames = stream.total_frames();
            self.stream = Some(stream);
        }
        Ok(())
    }

    /// 视频总时长（毫秒），stepper 打开后有效；0 表示未知（进度条据此退化）。
    pub fn total_duration_ms(&self) -> i64 {
        self.total_duration_ms
    }

    /// 视频总帧数，stepper 打开后有效；0 表示未知。
    pub fn total_frames(&self) -> i64 {
        self.total_frames
    }

    /// 推进解码窗口，确保 [window_start, target] 已解码，并丢弃窗口外的旧帧。
    /// EOF 后 `decoded_total` 固定为视频总帧数。
    pub fn advance_to(&mut self, target: i32) -> Result<()> {
        let _sm = imgops::sm_begin(imgops::SmCat::Advance);
        self.open_stepper()?;
        let stream = self.stream.as_mut().expect("stream 已打开");
        if self.w == 0 {
            let (w, h) = stream.dim();
            self.w = w;
            self.h = h;
        }
        // 从变换流水线按顺序消费 FrameData，直到覆盖 target（或 EOF）。
        while self.decoded_total <= target {
            match stream.recv_frame()? {
                Some((n, fd)) => {
                    let n = n as i32;
                    // 跳过已消费的帧（多 worker 无序产出，由 stream 保证顺序；此处防御）。
                    if n < self.decoded_total {
                        continue;
                    }
                    trace!(
                        frame = n,
                        pos = fd.pos,
                        has_text = fd.has_text,
                        isa_wc = fd.im.iter().filter(|&&v| v == 255).count(),
                        "transform frame"
                    );
                    self.window.push_back(fd);
                    self.decoded_total += 1;
                }
                None => {
                    // EOF：decoded_total 固定，不再推进。
                    break;
                }
            }
        }
        // 清理窗口：只保留 [target - MAX_WINDOW, target]。
        let keep_from = target - MAX_WINDOW;
        while !self.window.is_empty() && self.window_start < keep_from {
            self.window.pop_front();
            self.window_start += 1;
        }
        Ok(())
    }

    pub fn w(&self) -> usize {
        self.w
    }
    pub fn h(&self) -> usize {
        self.h
    }
    pub fn params(&self) -> &Params {
        self.p
    }
    /// 累计解码帧数（EOF 后 = 视频总帧数）。
    pub fn len(&self) -> usize {
        self.decoded_total as usize
    }
    pub fn is_empty(&self) -> bool {
        self.decoded_total == 0
    }
}

/// transform 流水线：单解码线程 + N 个 transform worker。
///
/// 对齐 C++ `AddConvertImageTask` 的前瞻 transform：解码线程把 flat BGR 帧按帧号
/// 均匀分发（idx % n_workers）给各个 worker，worker 对分到的帧调用
/// `get_transformed_image` 产出 `FrameData`，送入共享输出；消费方用 reorder 缓冲
/// 严格按帧号升序取用，从而让状态机直接消费已算好的结果而不阻塞在 transform 上。
struct TransformStream<'a> {
    rx_out: Option<Receiver<Result<Option<(usize, FrameData)>>>>,
    /// 各 worker 私有的 Profiler，结束时收集（供 cache 合并）。
    profs: Arc<Mutex<Vec<imgops::Profiler>>>,
    /// 各 worker 的输入 channel（Drop 时发哨兵以唤醒线程）。
    in_txs: Vec<SyncSender<InMsg>>,
    /// 已打开的解码 pipeline 的句柄（drop 时回收线程），仅用于保序/维度的元数据。
    w: usize,
    h: usize,
    total_duration_ms: i64,
    total_frames: i64,
    /// reorder 缓冲：下一帧应取的帧号。
    next_in: usize,
    pending: HashMap<usize, FrameData>,
    /// 消费端在 `rx.recv()` 上阻塞等待的总时长（毫秒；衡量流水线吞吐 vs 主线程串行）。
    wait_ms: f64,
    handles: Vec<std::thread::JoinHandle<()>>,
    p: std::marker::PhantomData<&'a Params>,
}

/// 输入 channel 一条消息：`Ok(Some((帧号, flat BGR, pts)))` 或 `Ok(None)`（EOF）。
type InMsg = Result<Option<(usize, Vec<u8>, i64)>>;

impl<'a> TransformStream<'a> {
    /// 打开视频并启动解码线程 + N 个 transform worker。`prof` 作为开启剖析的指示
    /// （每个 worker 各自计时，finally 由 [`Self::take_profiler_sum`] 合并）。
    fn open(path: &Path, p: &'a Params, prof: Option<&imgops::Profiler>) -> Result<Self> {
        use frame::FramePipeline;
        let mut dec = FramePipeline::open(path)?;
        let total_frames = dec.total_frames();
        let total_duration_ms = dec.total_duration_ms();
        let (w, h) = dec.dim();

        // worker 数量：transform 默认帧内串行（见 intra_frame_parallel），用 N 个
        // 单线程 transform worker 做帧间并行。实测 16 核下 n=3~4 最优（7.0s），
        // 更多 worker 因内存带宽/allocator 争用不再加速。可用 SF_NWORKERS 覆盖。
        let n_cpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let n_workers = std::env::var("SF_NWORKERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or((n_cpu / 3).clamp(1, 4));
        let n_workers = n_workers.max(1);

        // 每个 worker 一条输入 channel（idx % n_workers 路由）。
        let mut in_txs: Vec<SyncSender<InMsg>> = Vec::with_capacity(n_workers);
        let mut in_rxs: Vec<Receiver<InMsg>> = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            let (tx, rx) = sync_channel(4);
            in_txs.push(tx);
            in_rxs.push(rx);
        }

        // 共享输出（多 worker send → 消费者 reorder）。
        let (tx_out, rx_out): (SyncSender<Result<Option<(usize, FrameData)>>>, _) =
            sync_channel(16);

        let p_copy: Params = *p;
        let profs: Arc<Mutex<Vec<imgops::Profiler>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::with_capacity(n_workers + 1);

        // 解码线程：连续解码并路由给各 worker；EOF 给所有 worker 发哨兵。
        // 用 clone 让 decode 线程独享输入通道（struct 保留一份供 Drop 唤醒用）。
        let dec_in_txs: Vec<SyncSender<InMsg>> = in_txs.iter().cloned().collect();
        let th = std::thread::spawn(move || {
            let mut idx = 0usize;
            loop {
                let item = match dec.recv() {
                    Ok(Some(f)) => Some(f),
                    _ => None,
                };
                match item {
                    Some((flat, pts)) => {
                        let msg: InMsg = Ok(Some((idx, flat, pts)));
                        let wk = idx % dec_in_txs.len();
                        if dec_in_txs[wk].send(msg).is_err() {
                            break; // worker 已 drop
                        }
                        idx += 1;
                    }
                    None => {
                        for tx in &dec_in_txs {
                            let _ = tx.send(Ok(None));
                        }
                        break;
                    }
                }
            }
        });
        handles.push(th);

        // N 个 transform worker。
        let enable_prof = prof.is_some();
        for (_k, rx_in) in in_rxs.into_iter().enumerate() {
            let tx_out = tx_out.clone();
            let profs = Arc::clone(&profs);
            let h = std::thread::spawn(move || {
                let mut wp = imgops::Profiler::new();
                if enable_prof {
                    wp.enable();
                }
                // 处理本 worker 分到的帧（idx % n == k）。
                loop {
                    match rx_in.recv() {
                        Ok(Ok(Some((idx, flat, pts)))) => {
                            let (_ff, _sf, im_tf, im_ne, im_y, _lb, _le, _n, has_text) =
                                imgops::get_transformed_image(&flat, w, h, &p_copy, Some(&mut wp));
                            let y: Vec<u16> = if has_text == 1 {
                                im_y.iter().map(|&v| v as u16 + 255).collect()
                            } else {
                                vec![0; w * h]
                            };
                            let fd = FrameData {
                                bgr: flat,
                                im: im_tf,
                                ne: im_ne,
                                y,
                                pos: pts,
                                has_text: has_text == 1,
                            };
                            if tx_out.send(Ok(Some((idx, fd)))).is_err() {
                                break;
                            }
                        }
                        Ok(Ok(None)) => break,
                        Ok(Err(e)) => {
                            let _ = tx_out.send(Err(e));
                            break;
                        }
                        Err(_) => break,
                    }
                }
                profs.lock().unwrap().push(wp);
            });
            handles.push(h);
        }

        Ok(Self {
            rx_out: Some(rx_out),
            profs,
            in_txs,
            w,
            h,
            total_duration_ms,
            total_frames,
            next_in: 0,
            wait_ms: 0.0,
            pending: HashMap::new(),
            handles,
            p: std::marker::PhantomData,
        })
    }

    fn dim(&self) -> (usize, usize) {
        (self.w, self.h)
    }
    fn total_duration_ms(&self) -> i64 {
        self.total_duration_ms
    }
    fn total_frames(&self) -> i64 {
        self.total_frames
    }

    /// 取下一帧（按帧号严格升序）。`Ok(None)` EOF。
    fn recv_frame(&mut self) -> Result<Option<(usize, FrameData)>> {
        // 从 reorder 缓冲优先取 next_in。
        loop {
            if let Some(fd) = self.pending.remove(&self.next_in) {
                let idx = self.next_in;
                self.next_in += 1;
                return Ok(Some((idx, fd)));
            }
            let rx = match self.rx_out.as_ref() {
                Some(rx) => rx,
                None => return Ok(None),
            };
            // 收一个新完成的帧。
            let t = std::time::Instant::now();
            match rx.recv() {
                Ok(Ok(Some((idx, fd)))) => {
                    self.wait_ms += t.elapsed().as_secs_f64() * 1000.0;
                    if idx == self.next_in {
                        self.next_in += 1;
                        return Ok(Some((idx, fd)));
                    }
                    // 乱序：先存缓冲，继续等 next_in。
                    self.pending.insert(idx, fd);
                }
                Ok(Ok(None)) => {
                    self.wait_ms += t.elapsed().as_secs_f64() * 1000.0;
                    return Ok(None);
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    // channel 断开：清空缓冲后 EOF。
                    self.pending.clear();
                    return Ok(None);
                }
            }
        }
    }

    /// 合并所有 worker 的 Profiler（仅剖析开启时有意义）。
    fn take_profiler_sum(&self) -> Option<imgops::Profiler> {
        let g = self.profs.lock().unwrap();
        let mut sum = imgops::Profiler::new();
        for p in g.iter() {
            sum.color_filtration_ms += p.color_filtration_ms;
            sum.bgr_to_yuv_ms += p.bgr_to_yuv_ms;
            sum.im_ff_ms += p.im_ff_ms;
            sum.im_ne_he_ms += p.im_ne_he_ms;
            sum.filter_ms += p.filter_ms;
            sum.analyse_ms += p.analyse_ms;
            sum.thr_ms += p.thr_ms;
            sum.enabled |= p.enabled;
        }
        if sum.enabled {
            Some(sum)
        } else {
            None
        }
    }
}

impl Drop for TransformStream<'_> {
    fn drop(&mut self) {
        // 1) 发哨兵唤醒所有 worker（即使空闲在 recv()）。
        for tx in &self.in_txs {
            let _ = tx.send(Ok(None));
        }
        // 2) 断开输出，解除 worker 在 tx_out.send() 上的阻塞。
        self.rx_out.take();
        // 3) 回收所有线程。
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// 取帧，越界/已被窗口丢弃返回 `None`（对应 C++ 帧不可得，状态机据此优雅结束）。
fn try_frame<'a>(cache: &'a FrameCache<'a>, fn_: i32) -> Option<&'a FrameData> {
    if fn_ < 0 || fn_ < cache.window_start {
        return None;
    }
    let idx = (fn_ - cache.window_start) as usize;
    if idx >= cache.window.len() {
        return None;
    }
    cache.window.get(idx)
}

fn get_frame<'a>(cache: &'a FrameCache<'a>, fn_: i32) -> Result<&'a FrameData> {
    try_frame(cache, fn_).ok_or_else(|| anyhow::anyhow!("帧 {} 越界", fn_))
}

/// `AnalizeImageForSubPresence`：判断交集 ISA 图是否含字幕。返回 bool。
/// 先把 ILA（u16 时间图）应用到 ISA（u8 前景），再转 `Array2` 调用 `analyse_image`。
pub(crate) fn analyse_image_flat(im: &[u8], ila: Option<&[u16]>, w: usize, h: usize, p: &Params) -> bool {
    let mut isa = im.to_vec();
    if let Some(il) = ila {
        imgops::intersect_two_images_inplace(&mut isa, il, 0u8);
    }
    let arr = ndarray::Array2::from_shape_vec((h, w), isa).expect("ISA 尺寸错误");
    crate::preprocess::analyse_image(&arr, p)
}

/// `IntersectImages`（多图版）：`im_res = ∩ ims[min..=max]`。就地。
fn intersect_images_range(im_res: &mut [u8], ims: &[&[u8]], min_id: usize, max_id: usize, w: usize, h: usize) {
    let size = w * h;
    for i in 0..size {
        if im_res[i] == 255 {
            for &im in &ims[min_id..=max_id] {
                if im[i] != 255 {
                    im_res[i] = 0;
                    break;
                }
            }
        }
    }
}

/// `IntersectYImages`（多图版）：`ImRes` 与 `ImY[min..=max]` 做时间交叠检查。就地。
fn intersect_y_images_range(im_res: &mut [u16], ims: &[&[u16]], min_id: usize, max_id: usize, p: &Params) {
    let size = im_res.len();
    for i in 0..size {
        if im_res[i] != 0 {
            let r = im_res[i] as i32;
            for &im in &ims[min_id..=max_id] {
                let v = im[i] as i32;
                if v < r - p.max_dl_down as i32 || v > r + p.max_dl_up as i32 {
                    im_res[i] = 0;
                    break;
                }
            }
        }
    }
}

/// `GetIntersectImages(fn)` 等价：`ImInt = ImForward[fn..fn+DL-1]` 交集 + `AnalyseImage`。
/// 返回 `(im_int, y_int, bln)`；帧越界返回 `None`（对应 C++ 帧不可得）。
pub(crate) fn get_intersect_images(
    cache: &FrameCache,
    fn_: usize,
) -> Option<(Vec<u8>, Vec<u16>, bool)> {
    let _sm = imgops::sm_begin(imgops::SmCat::Intersect);
    let p = cache.params();
    let w = cache.w();
    let h = cache.h();
    let dl = p.dl;

    // 对齐 C++ `AddIntersectImagesTask`（SSAlgorithms.cpp:820-838）：遍历 [fn, fn+DL-1]，
    // 只要有一帧 has_text=0 → bln=0（立即短路），pImInt 只取第一帧（不交集）。全部
    // has_text=1 才做交集 + AnalyseImage。之前 Rust 跳过了 has_text=0 帧，只对
    // has_text=1 帧交集 → 残留帧（has_text 交替 1/0）时可能误判有字幕 → 段过度切分。
    // 对齐 C++ `AddIntersectImagesTask`（SSAlgorithms.cpp:820-838）：遍历 [fn, fn+DL-1]，
    // 只要有一帧 has_text=0 → bln=0（立即短路），pImInt 只取第一帧（不交集）。全部
    // has_text=1 才做交集 + AnalyseImage。
    let f0 = try_frame(cache, fn_ as i32)?;
    if !f0.has_text {
        return Some((f0.im.clone(), f0.y.clone(), false));
    }
    for i in 1..dl {
        match try_frame(cache, (fn_ + i) as i32) {
            Some(f) if f.has_text => {}
            Some(_) => {
                return Some((f0.im.clone(), f0.y.clone(), false));
            }
            None => break,
        }
    }

    // 全部 has_text=1 → 交集 [fn, fn+DL-1]。
    let mut im_int = f0.im.clone();
    let mut y_int = f0.y.clone();
    let mut ims: Vec<&[u8]> = Vec::with_capacity(dl);
    let mut imys: Vec<&[u16]> = Vec::with_capacity(dl);
    ims.push(&f0.im);
    imys.push(&f0.y);
    for i in 1..dl {
        match try_frame(cache, (fn_ + i) as i32) {
            Some(f) => {
                ims.push(&f.im);
                imys.push(&f.y);
            }
            None => break,
        }
    }
    intersect_images_range(&mut im_int, &ims, 1, ims.len() - 1, w, h);
    intersect_y_images_range(&mut y_int, &imys, 1, imys.len() - 1, p);

    let bln = analyse_image_flat(&im_int, Some(&y_int), w, h, p);
    // 诊断：交集后 331-339 内容（幽灵带来源）。
    let wc331_im = im_int[331 * w..340 * w].iter().filter(|&&v| v == 255).count();
    let wc331_y = y_int[331 * w..340 * w].iter().filter(|&&v| v != 0).count();
    trace!(fn_, bln, wc331_im, wc331_y, "get_intersect_images: 331-339 交集后");
    Some((im_int, y_int, bln))
}

/// `CompareTwoSubsByOffset`：把当前字幕段 `(im_int_s, y_s, ne_s)` 与 `ImForward[offset]`
/// 比较，判断是否内容变化（返回 false 表示变化）。帧越界返回 `None`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn compare_by_offset(
    cache: &FrameCache,
    fn_: usize,
    im_int_s: &[u8],
    y_s: &[u16],
    ne_s: &[u8],
    prev_ne: &[u8],
    offset: usize,
) -> Option<bool> {
    let _sm = imgops::sm_begin(imgops::SmCat::Compare);
    let p = cache.params();
    let w = cache.w();
    let h = cache.h();
    let dl = p.dl;

    let ne12 = if offset == 0 {
        prev_ne.to_vec()
    } else {
        try_frame(cache, (fn_ + offset - 1) as i32)?.ne.clone()
    };

    let f_off = try_frame(cache, (fn_ + offset) as i32)?;
    trace!(
        caller = "OFFSET",
        fn_, offset,
        im1 = im_int_s.iter().filter(|&&v| v == 255).count(),
        ve1 = ne_s.iter().filter(|&&v| v == 255).count(),
        "compare 输入"
    );
    let mut bln = compare::compare_two_subs_optimal(
        im_int_s,
        Some(y_s),
        ne_s,
        Some(&ne12),
        &f_off.im,
        None,
        &f_off.ne,
        w,
        h,
        0,
        w as i32 - 1,
        p,
    );

    if !bln {
        // 交集 ImInt2 = ImForward[offset..DL-2]。
        let mut im_int2 = f_off.im.clone();
        let mut y_int2 = f_off.y.clone();
        // 收集 offset+1..=DL-2 帧。
        let mut ims: Vec<Vec<u8>> = Vec::new();
        let mut imys: Vec<Vec<u16>> = Vec::new();
        for i in (offset + 1)..=(dl - 2) {
            let f = try_frame(cache, (fn_ + i) as i32)?;
            ims.push(f.im.clone());
            imys.push(f.y.clone());
        }
        let im_refs: Vec<&[u8]> = ims.iter().map(|v| v.as_slice()).collect();
        let iy_refs: Vec<&[u16]> = imys.iter().map(|v| v.as_slice()).collect();
        if !im_refs.is_empty() {
            intersect_images_range(&mut im_int2, &im_refs, 0, im_refs.len() - 1, w, h);
            intersect_y_images_range(&mut y_int2, &iy_refs, 0, iy_refs.len() - 1, p);
        }
        bln = compare::compare_two_subs_optimal(
            im_int_s,
            Some(y_s),
            ne_s,
            Some(&ne12),
            &im_int2,
            Some(&y_int2),
            &f_off.ne,
            w,
            h,
            0,
            w as i32 - 1,
            p,
        );
    }

    Some(bln)
}

/// `FindOffsetForNewSub`：找第一个与当前字幕段内容不同的 forward offset。返回 offset。
#[allow(clippy::too_many_arguments)]
pub(crate) fn find_offset_for_new_sub(
    cache: &FrameCache,
    fn_: usize,
    im_int_s: &[u8],
    y_s: &[u16],
    ne_s: &[u8],
    prev_ne: &[u8],
) -> Option<usize> {
    let dl = cache.params().dl;
    for offset in 0..(dl - 1) {
        let same = compare_by_offset(cache, fn_, im_int_s, y_s, ne_s, prev_ne, offset)?;
        if !same {
            return Some(offset);
        }
    }
    Some(dl - 1)
}

/// 顶层入口：解码视频并跑状态机。
pub fn find_keyframes(video: &Path, params: &Params) -> Result<Vec<Keyframe>> {
    let mut cache = FrameCache::new(video, params);
    find_keyframes_with_cache(&mut cache, params)
}

/// 用已构造的缓存跑状态机。
///
/// 不再全量解码：先解码第一帧确定维度，再由 [`run_state_machine`] 在推进 `fn`
/// 时按需 `advance_to` 逐帧解码（滑动窗口）。
///
/// `on_progress` 为可选进度回调：每推进一帧调用一次 `(decoded, total)`，
/// `decoded`=已解码帧数，`total`=视频总帧数（未知时为 0），供 CLI 渲染进度条
/// （默认空闭包，不影响性能与算法）。
pub fn find_keyframes_with_cache(
    cache: &mut FrameCache,
    params: &Params,
) -> Result<Vec<Keyframe>> {
    find_keyframes_with_cache_progress(cache, params, &mut |_, _| {})
}

/// 带进度回调的 [`find_keyframes_with_cache`]。
pub fn find_keyframes_with_cache_progress(
    cache: &mut FrameCache,
    params: &Params,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<Keyframe>> {
    // 先解码首批帧，从而 `cache.w()/h()` 已确定（维度在首次解码时填入）。
    cache.advance_to(FORWARD)?;
    let w = cache.w();
    let h = cache.h();
    let total = cache.total_frames().max(0) as u64;
    run_state_machine(cache, w, h, params, "", total, on_progress)
}

/// 状态机本体：对 `cache` 逐帧筛选（按需 `advance_to` 流式解码），输出关键帧。
#[allow(unused_assignments)]
fn run_state_machine(
    cache: &mut FrameCache,
    w: usize,
    h: usize,
    p: &Params,
    video_label: &str,
    total: u64,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<Keyframe>> {
    let dl = p.dl;
    let ddl = dl / 2;
    let ddl1_ofset = ddl - 1; // = 2
    let ddl2_ofset = 2 * ddl - 1; // = 5
    let size = w * h;

    if !video_label.is_empty() {
        eprintln!("subtitle_finder: 解码 {} 帧，{}x{}", cache.len(), w, h);
    }

    // 存储图像（对应 C++ 的状态变量）。
    let mut im_int_s = vec![0u8; size]; // ImIntS
    let mut im_int_sp = vec![0u8; size]; // ImIntSP
    let mut im_ne_s = vec![0u8; size]; // ImNES
    let mut im_ne_sp = vec![0u8; size]; // ImNESP
    let mut im_fs = vec![0u8; size * 3]; // ImFS（保存的 BGR）
    let mut im_fsp = vec![0u8; size * 3]; // ImFSP
    let mut im_y_s = vec![0u16; size]; // ImYS
    let mut im_y_sp = vec![0u16; size]; // ImYSP
    let mut prev_im_ne = vec![0u8; size]; // prevImNE

    let mut bf: i32 = -2;
    let mut ef: i32 = -2;
    let mut et: i64 = -2;
    let mut pbf: i32 = -2;
    let mut bt: i64 = -2;
    let mut pbt: i64 = -2;
    let mut pet: i64 = -2;
    let mut finded_prev: i32 = 0;
    let mut cmp_prev: i32 = 0;
    let mut found_sub: i32 = 0;

    let mut fn_: i32 = 0;
    let mut fn_start: i32 = 0;
    let mut prev_pos: i64 = -2;

    // 保存关键帧。
    let mut keyframes: Vec<Keyframe> = Vec::new();
    let mut save_keyframe = |im_fs: &[u8], mask: &[u8], start_ms: i64, end_ms: i64| {
        let arr = flat_bgr_to_array3(im_fs, w, h);
        let mask_arr = ndarray::Array2::from_shape_vec((h, w), mask.to_vec()).expect("mask 尺寸错误");
        keyframes.push(Keyframe {
            start_ms: start_ms.max(0) as u64,
            end_ms: end_ms.max(0) as u64,
            frame: arr,
            mask: mask_arr,
        });
    };

    // 检测阶段：找字幕起始。
    'outer: loop {
        // 流式推进：确保 [fn_start, fn_start+FORWARD] 已解码（fn_start 单调推进）。
        cache.advance_to(fn_start + FORWARD)?;
        // 内部搜索循环：仅当未找到字幕时运行（C++ `while(found_sub == 0)`）。
        if found_sub == 0 {
            loop {
                // 流式推进：检测内循环会连续推进 fn_start，需每次确保窗口覆盖
                // [fn_start, fn_start+FORWARD]，否则 fn_start 涨过 FORWARD 后
                // get_frame 越界会提前 break 'outer（长视频空字幕段会触发）。
                cache.advance_to(fn_start + FORWARD)?;
                // 推进 fn_start 的 ddl 步。
                // C++ 中先并行解码 fn_start+ddl1_ofset 与 fn_start+ddl2_ofset 帧。
                let f1 = match get_frame(&cache, fn_start + ddl1_ofset as i32) {
                    Ok(f) => f.clone(),
                    Err(_) => break 'outer, // 帧越界 → 结束
                };
                let bln1 = f1.has_text;
                // C++：bln1/bln2 都是 GetConvertImage 的返回（帧存在且有字幕）。
                // bln2 需在 else 分支复用（决定 fn_start 走 ddl 还是 2*ddl），故提级。
                let mut bln2 = false;
                let mut bln = false;
                if bln1 {
                    let f2 = match get_frame(&cache, fn_start + ddl2_ofset as i32) {
                        Ok(f) => f.clone(),
                        Err(_) => break 'outer,
                    };
                    bln2 = f2.has_text;
                    if bln2 {
                        // ImInt = ImForward[fn_start+ddl1_ofset] ∩ ImForward[fn_start+ddl2_ofset]
                        let mut im_int = f1.im.clone();
                        imgops::intersect_two_images_inplace(&mut im_int, &f2.im, 0u8);
                        // ImYInt = ImYForward[0] ∩Y ImYForward[1]
                        let mut y_int = f1.y.clone();
                        imgops::intersect_y_images(&mut y_int, &f2.y, p.max_dl_down as i32, p.max_dl_up as i32);

                        bln = analyse_image_flat(&im_int, Some(&y_int), w, h, p);
                        if bln {
                            // 中间帧 [ddl1_ofset+1, ddl2_ofset-1]。
                            for i in (ddl1_ofset + 1)..=(ddl2_ofset - 1) {
                                let fi = match get_frame(&cache, fn_start + i as i32) {
                                    Ok(f) => f,
                                    Err(_) => break 'outer,
                                };
                                imgops::intersect_two_images_inplace(&mut im_int, &fi.im, 0u8);
                                imgops::intersect_y_images(&mut y_int, &fi.y, p.max_dl_down as i32, p.max_dl_up as i32);
                            }
                            bln = analyse_image_flat(&im_int, Some(&y_int), w, h, p);
                        }
                    }
                }

                if bln {
                    found_sub = 1;
                    fn_ = fn_start;
                    trace!(fn_start, "检测: 判定新字幕起点");
                    break;
                } else {
                    if bln1 {
                        // C++ line 1396：`if (bln2)` 用的是该帧 has_text，不是「帧是否存在」。
                        // 之前误用 `.is_ok()`（帧存在即 true）→ 无字幕帧也走 ddl 步，检测步进错。
                        fn_start += if bln2 { ddl as i32 } else { 2 * ddl as i32 };
                    } else {
                        fn_start += ddl as i32;
                    }
                    if fn_start >= cache.len() as i32 {
                        break 'outer;
                    }
                }
            }
        }

        if found_sub == 0 {
            break 'outer;
        }

        // 追踪阶段：fn_ 为字幕起始。
        let f0 = match get_frame(&cache, fn_) {
            Ok(f) => f,
            Err(_) => {
                // EOF：保存当前进行中的字幕段。
                if bf != -2 {
                    let last = cache.len().saturating_sub(1) as i32;
                    if let Some(f) = try_frame(&cache, last) {
                        et = f.pos;
                    }
                    if last - bf + 1 >= p.dl as i32 {
                        let mut im_int = im_int_s.clone();
                        let mut im_y = im_y_s.clone();
                        if filter::analize_for_sub_presence(&im_ne_s, &mut im_int, &mut im_y, w, h, p) == 1 {
                            save_keyframe(&im_fs, &im_int, bt, et);
                        }
                    }
                    bf = -2;
                }
                break 'outer;
            }
        };
        prev_pos = if fn_ > 0 { get_frame(&cache, fn_ - 1)?.pos } else { -1 };
        let cur_pos = f0.pos;

        // bln = GetIntersectImages(fn)：ImInt = intersect(fn..fn+DL-1)。
        let (im_int, y_int, mut bln) = match get_intersect_images(&cache, fn_ as usize) {
            Some(v) => v,
            None => {
                // EOF：保存当前进行中的字幕段。
                if bf != -2 {
                    let last = cache.len().saturating_sub(1) as i32;
                    if let Some(f) = try_frame(&cache, last) {
                        et = f.pos;
                    }
                    if last - bf + 1 >= p.dl as i32 {
                        let mut im_int = im_int_s.clone();
                        let mut im_y = im_y_s.clone();
                        if filter::analize_for_sub_presence(&im_ne_s, &mut im_int, &mut im_y, w, h, p) == 1 {
                            save_keyframe(&im_fs, &im_int, bt, et);
                        }
                    }
                    bf = -2;
                }
                break 'outer; // 帧越界 → 结束
            }
        };

        // fn == bf → 记录当前为字幕段存储。
        if fn_ == bf {
            im_int_s = im_int.clone();
            im_ne_s = f0.ne.clone();
            im_y_s = y_int.clone();
            im_fs = f0.bgr.clone();
        }

        if fn_ > ef {
            if bln && cur_pos != prev_pos {
                if bf == -2 {
                    bf = fn_;
                    ef = bf;
                    bt = cur_pos;
                    im_int_s = im_int.clone();
                    im_ne_s = f0.ne.clone();
                    im_y_s = y_int.clone();
                    im_fs = f0.bgr.clone();
                } else {
                    // CompareTwoSubsOptimal(ImIntS, &ImYS, ImNES, prevImNE, ImInt, &ImYInt, ImNE)
                    trace!(
                        caller = "TRACK",
                        fn_, bf,
                        im1 = im_int_s.iter().filter(|&&v| v == 255).count(),
                        ve1 = im_ne_s.iter().filter(|&&v| v == 255).count(),
                        "compare 输入"
                    );
                    bln = compare::compare_two_subs_optimal(
                        &im_int_s, Some(&im_y_s), &im_ne_s, Some(&prev_im_ne),
                        &im_int, Some(&y_int), &f0.ne, w, h, 0, w as i32 - 1, p,
                    );
                    if !bln {
                        trace!(fn_, bf, "追踪: 判定字幕内容变化");
                    }
                    if bln && (fn_ - bf + 1 == 3) {
                        im_fs = f0.bgr.clone();
                        im_ne_s = f0.ne.clone();
                        im_int_s = im_int.clone();
                        im_y_s = y_int.clone();
                    }
                    if !bln {
                        // bln == 0 → 字幕内容变化。
                        if finded_prev == 1 {
                            cmp_prev = compare::compare_two_subs_optimal(
                                &im_int_sp, Some(&im_y_sp), &im_ne_sp, Some(&im_ne_sp),
                                &im_int_s, Some(&im_y_s), &im_ne_s, w, h, 0, w as i32 - 1, p,
                            ) as i32;
                            if cmp_prev == 0 {
                                // 保存前一段。
                                if filter::analize_for_sub_presence(&im_ne_sp, &mut im_int_sp, &im_y_sp, w, h, p) == 1 {
                                    save_keyframe(&im_fsp, &im_int_sp, pbt, pet);
                                }
                                pbf = bf;
                                pbt = bt;
                            }
                        } else {
                            pbf = bf;
                            pbt = bt;
                        }

                        let mut pef = fn_ - 1;
                        let mut new_pet = cur_pos - 1;

                        let mut offset = 0usize;
                        if fn_ > bf + 1 {
                            offset = match find_offset_for_new_sub(
                                &cache, fn_ as usize, &im_int_s, &im_y_s, &im_ne_s, &prev_im_ne,
                            ) {
                                Some(o) => o,
                                None => {
                                    // 接近 EOF 时找不到前向帧（`compare_by_offset` 帧越界返回
                                    // None）→ 无法确定新句 offset。但**当前段仍应保存**（C++
                                    // 前向缓冲在 EOF 保留末帧，不会 break，会正常保存段）。
                                    // 之前 `break 'outer` 直接退出 → 当前段丢失（末尾段丢失）。
                                    // 用最后一帧作为段尾保存。
                                    trace!(fn_, bf, "内容变化: offset 搜索 EOF，保存当前段");
                                    let last = cache.len().saturating_sub(1) as i32;
                                    if let Some(f) = try_frame(&cache, last) {
                                        pet = f.pos;
                                    } else {
                                        pet = cur_pos;
                                    }
                                    pef = last;
                                    let mut im_int = im_int_s.clone();
                                    let mut im_y = im_y_s.clone();
                                    if pef - bf + 1 >= p.dl as i32
                                        && filter::analize_for_sub_presence(&im_ne_s, &mut im_int, &mut im_y, w, h, p) == 1
                                    {
                                        save_keyframe(&im_fs, &im_int, bt, pet);
                                    }
                                    bf = -2;
                                    break 'outer;
                                }
                            };
                            pef = fn_ + offset as i32 - 1;
                            // 段尾 pet：用旧句最后可见帧 frame(fn_+offset-1) 的真实 PTS。
                            // 与 et 同理，C++ 的 `PosForward[offset]-1` 虚推 ~1 帧。
                            match try_frame(&cache, fn_ + offset as i32 - 1) {
                                Some(f_prev) => new_pet = f_prev.pos,
                                None => match try_frame(&cache, fn_ + offset as i32) {
                                    Some(f) => new_pet = f.pos - 1,
                                    None => break 'outer,
                                },
                            }
                        }
                        pet = new_pet;

                        if pef - pbf + 1 >= dl as i32 {
                            if !((finded_prev == 1) && (cmp_prev == 1)) {
                                trace!(
                                    fn_, bf, finded_prev, cmp_prev,
                                    s_isa = im_int_s.iter().filter(|&&v| v == 255).count(),
                                    s_y = im_y_s.iter().filter(|&&v| v != 0).count(),
                                    "内容变化: 存 im_int_sp = im_int_s"
                                );
                                im_int_sp = im_int_s.clone();
                                im_fsp = im_fs.clone();
                                im_ne_sp = im_ne_s.clone();
                                im_y_sp = im_y_s.clone();
                            }
                            finded_prev = 1;
                        } else {
                            finded_prev = 0;
                        }

                        bf = fn_ + offset as i32;
                        ef = bf;
                        bt = f0.pos + offset as i64 * 1000 / 30;
                        let f_off = match try_frame(&cache, fn_ + offset as i32) {
                            Some(f) => f,
                            None => break 'outer,
                        };
                        im_ne_s = f_off.ne.clone();
                        im_fs = f_off.bgr.clone();
                        if offset == 0 {
                            im_int_s = im_int.clone();
                            im_y_s = y_int.clone();
                        } else {
                            im_int_s = f_off.im.clone();
                            im_y_s = f_off.y.iter().map(|&v| v as u16 + 255).collect();
                        }
                    } else {
                        // bln != 0 → 内容一致，扩展 YS。
                        imgops::intersect_y_images(&mut im_y_s, &f0.y, p.max_dl_down as i32, p.max_dl_up as i32);
                    }
                }
            } else if (bln == false && cur_pos != prev_pos) || (bln == true && cur_pos == prev_pos) {
                trace!(fn_, cur_pos, prev_pos, bln, bf, ef, "段尾分支: 进入 (bln 变化/帧停)");
                if finded_prev == 1 {
                    trace!(
                        fn_,
                        sp_isa = im_int_sp.iter().filter(|&&v| v == 255).count(),
                        sp_y = im_y_sp.iter().filter(|&&v| v != 0).count(),
                        sp_ne = im_ne_sp.iter().filter(|&&v| v == 255).count(),
                        s_isa = im_int_s.iter().filter(|&&v| v == 255).count(),
                        s_y = im_y_s.iter().filter(|&&v| v != 0).count(),
                        s_ne = im_ne_s.iter().filter(|&&v| v == 255).count(),
                        "段尾 merge: compare(im_int_sp, im_int_s) 输入白点"
                    );
                    bln = compare::compare_two_subs_optimal(
                        &im_int_sp, Some(&im_y_sp), &im_ne_sp, Some(&im_ne_sp),
                        &im_int_s, Some(&im_y_s), &im_ne_s, w, h, 0, w as i32 - 1, p,
                    );
                    if bln {
                        bf = pbf;
                        ef = bf;
                        bt = pbt;
                        finded_prev = 0;
                    }
                }
                if bf != -2 {
                    if cur_pos != prev_pos {
                        // 逐个 offset 比较，找字幕结束。
                        // 对齐 C++ `for (offset = 0; offset < DL - 1; offset++)`：
                        // 循环变量 offset 在**跑满**（全部 match）后值为 DL-1=5，不是 0。
                        // Rust 之前只在 !bln 时改 offset，跑满后仍是 0 → ef 偏小 5 帧 → 段尾过早。
                        let mut offset = dl - 1; // 默认=跑满后的终端值
                        let mut p_prev_ne = prev_im_ne.clone();
                        for off in 0..(dl - 1) {
                            let f_off = match try_frame(&cache, fn_ + off as i32) {
                                Some(f) => f,
                                None => {
                                    offset = off;
                                    break;
                                }
                            };
                            bln = compare::compare_two_subs_optimal(
                                &im_int_s, Some(&im_y_s), &im_ne_s, Some(&p_prev_ne),
                                &im_int_s, Some(&im_y_s), &f_off.ne,
                                w, h, 0, w as i32 - 1, p,
                            );
                            trace!(fn_, off, bln, fpos = f_off.pos, "段尾 offset 搜索: compare");
                            if !bln {
                                // 交集重试。
                                let mut ne_ff = f_off.ne.clone();
                                imgops::intersect_two_images_inplace(&mut ne_ff, &im_ne_s, 0u8);
                                bln = compare::compare_two_subs_optimal(
                                    &im_int_s, Some(&im_y_s), &im_ne_s, Some(&p_prev_ne),
                                    &im_int_s, Some(&im_y_s), &ne_ff, w, h, 0, w as i32 - 1, p,
                                );
                            }
                            if !bln {
                                offset = off;
                                break;
                            }
                            p_prev_ne = f_off.ne.clone();
                        }
                        ef = fn_ + offset as i32 - 1;
                        // 段尾 et：用旧句最后可见帧 frame(fn_+offset-1) 的真实 PTS。
                        // C++ `et = PosForward[offset] - 1`（新句首帧 PTS-1）会虚推 ~1 帧：
                        // 实测字幕 20333 消失，C++ 标 20365。真实末帧 PTS 才准。
                        match try_frame(&cache, fn_ + offset as i32 - 1) {
                            Some(f) => et = f.pos,
                            None => match try_frame(&cache, fn_ + offset as i32) {
                                Some(f) => et = f.pos - 1,
                                None => et = cur_pos,
                            },
                        }
                    } else {
                        ef = fn_ - 1;
                        et = cur_pos;
                    }

                    if ef - bf + 1 < dl as i32 {
                        if finded_prev == 1 {
                            bln = compare::compare_two_subs_optimal(
                                &im_int_s, Some(&im_y_s), &im_ne_sp, Some(&im_ne_sp),
                                &im_int_s, Some(&im_y_s), &im_ne_s, w, h, 0, w as i32 - 1, p,
                            );
                            if !bln {
                                let mut ne_sf = im_ne_s.clone();
                                imgops::intersect_two_images_inplace(&mut ne_sf, &im_ne_sp, 0u8);
                                bln = compare::compare_two_subs_optimal(
                                    &im_int_s, Some(&im_y_s), &im_ne_sp, Some(&im_ne_sp),
                                    &im_int_s, Some(&im_y_s), &ne_sf, w, h, 0, w as i32 - 1, p,
                                );
                                if !bln {
                                    let mut ne_spf = im_ne_sp.clone();
                                    imgops::intersect_two_images_inplace(&mut ne_spf, &im_ne_s, 0u8);
                                    bln = compare::compare_two_subs_optimal(
                                        &im_int_s, Some(&im_y_s), &ne_spf, Some(&ne_spf),
                                        &im_int_s, Some(&im_y_s), &im_ne_s, w, h, 0, w as i32 - 1, p,
                                    );
                                }
                            }
                            if bln {
                                bf = pbf;
                                bt = pbt;
                            }
                        }
                    }

                    if finded_prev == 1 && bf != pbf {
                        let sp_isa_wc = im_int_sp.iter().filter(|&&v| v == 255).count();
                        let sp_ne_wc = im_ne_sp.iter().filter(|&&v| v == 255).count();
                        let sp_y_wc = im_y_sp.iter().filter(|&&v| v != 0).count();
                        let r = filter::analize_for_sub_presence(&im_ne_sp, &mut im_int_sp, &im_y_sp, w, h, p);
                        trace!(fn_, bf, pbf, bt, pet, sp_isa_wc, sp_ne_wc, sp_y_wc, r, "段尾 SP 保存: analize_for_sub_presence");
                        if r == 1 {
                            save_keyframe(&im_fsp, &im_int_sp, pbt, pet);
                        }
                    }

                    if ef - bf + 1 >= dl as i32 {
                        if bf != pbf {
                            trace!(fn_, bf, ef, bt, et, "段结束: 保存段 (bf!=pbf)");
                            if filter::analize_for_sub_presence(&im_ne_s, &mut im_int_s, &im_y_s, w, h, p) == 1 {
                                save_keyframe(&im_fs, &im_int_s, bt, et);
                            }
                        } else {
                            if filter::analize_for_sub_presence(&im_ne_sp, &mut im_int_sp, &im_y_sp, w, h, p) == 1 {
                                save_keyframe(&im_fsp, &im_int_sp, bt, et);
                            }
                        }
                    }
                }

                finded_prev = 0;
                bf = -2;
                trace!(fn_, "段结束: 已保存/跳过, bf 重置");

                if fn_ > ef {
                    if fn_ - fn_start >= dl as i32 {
                        found_sub = 0;
                        fn_start = fn_;
                    }
                }
            }
        }

        if found_sub != 0 {
            prev_im_ne = get_frame(&cache, fn_)?.ne.clone();
            // 进度回调：已解码帧数 / 总帧数（cache.len() 为真实已解码帧数）。
            on_progress(cache.len() as u64, total);
            fn_ += 1;
            // 流式推进：确保 [fn_+1, fn_+FORWARD] 已解码（追踪阶段 fn_ 单调递增）。
            cache.advance_to(fn_ + FORWARD)?;
            if fn_ >= cache.len() as i32 {
                // EOF：保存当前进行中的字幕段（否则末尾段丢失）。
                if bf != -2 {
                    let last = cache.len().saturating_sub(1) as i32;
                    if let Some(f) = try_frame(&cache, last) {
                        et = f.pos;
                    }
                    if last - bf + 1 >= p.dl as i32 {
                        let mut im_int = im_int_s.clone();
                        let mut im_y = im_y_s.clone();
                        if filter::analize_for_sub_presence(&im_ne_s, &mut im_int, &mut im_y, w, h, p) == 1 {
                            save_keyframe(&im_fs, &im_int, bt, et);
                        }
                    }
                    bf = -2;
                }
                break;
            }
        }
    }

    // 循环结束后的收尾：若仍有 finded_prev 段，保存（对齐 C++ 末尾段）。
    if finded_prev == 1 {
        if filter::analize_for_sub_presence(&im_ne_sp, &mut im_int_sp, &im_y_sp, w, h, p) == 1 {
            save_keyframe(&im_fsp, &im_int_sp, pbt, pet);
        }
    }

    Ok(keyframes)
}

/// flat BGR → `Array3`（H×W×3）。
fn flat_bgr_to_array3(bgr: &[u8], w: usize, h: usize) -> ndarray::Array3<u8> {
    let mut arr = ndarray::Array3::<u8>::zeros((h, w, 3));
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            arr[[y, x, 0]] = bgr[i];
            arr[[y, x, 1]] = bgr[i + 1];
            arr[[y, x, 2]] = bgr[i + 2];
        }
    }
    arr
}

#[cfg(test)]
mod probe {
    use super::*;
    use std::time::Instant;

    /// 纯 transform 流水线吞吐（不跑状态机）：测解码+transform 的端到端能力上限。
    /// 默认忽略，`cargo test -p subtitle_finder --release -- --ignored pipe` 手动跑。
    #[test]
    #[ignore]
    fn pipe_throughput() {
        let path = std::path::Path::new(
            "/home/aa/repos/ai_ls/vision-lab/tests/bench/subtitle_ocr/ref/狗/2/video_source.mp4",
        );
        let p = crate::params::Params::default();
        let mut ts = TransformStream::open(path, &p, None).unwrap();
        let t0 = Instant::now();
        let mut n = 0usize;
        while let Some((idx, _fd)) = ts.recv_frame().unwrap() {
            n += 1;
            if n % 1017 == 0 {
                eprintln!(
                    "pipe[{:?}]: {n} frames in {:.2}s (last idx {idx})",
                    std::thread::current().name(),
                    t0.elapsed().as_secs_f64()
                );
            }
        }
        eprintln!(
            "pipe total: {n} frames in {:.2}s = {:.2}ms/frame",
            t0.elapsed().as_secs_f64(),
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }
}
