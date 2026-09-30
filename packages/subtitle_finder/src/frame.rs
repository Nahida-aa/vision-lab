//! 逐帧解码（视频 → 帧流），复刻 VideoSubFinder 的 30fps 逐帧。
//!
//! 用 `opencv::videoio::VideoCapture` 解码视频（与 C++ `VideoSubFinder` 的 OpenCV
//! 后端一致），逐帧产出 BGR `Array3<u8>`（H×W×3），供预处理（交集/投影）使用。
//!
//! ⚠️ 之前用 `ffmpeg-next` 解码，其 swscale 默认输出 **bt601** BGR，而 C++/OpenCV
//! 输出 **bt709** BGR（差 ±1-8）。该输入色彩差异让 `get_im_ff` 阈值边缘像素判定不同
//! → 幽灵带 → 字幕段丢失/过度切分。改用 OpenCV VideoCapture 后输入与 C++ 完全一致。
//! （配合 `imgops::bgr_to_yuv` 用 `cvtColor`，彻底对齐。）
//!
//! 提供两种用法：
//! - [`FrameStepper`]：**流式逐帧**（`next()` 按需拉帧，可暂停/续解）。
//! - [`for_each_frame`]：一次性全量回调解码（基于 `FrameStepper`，保持兼容）。

use anyhow::{Context, Result};

/// 流式逐帧解码器：持有 OpenCV `VideoCapture`，`next()` 按需拉一帧。
pub struct FrameStepper {
    cap: opencv::videoio::VideoCapture,
    total_duration_ms: i64,
    total_frames: i64,
    fps: f64,
    decoded_count: i64, // 已产出帧数（用于无 PTS 兜底估算）
    w: usize,           // 首帧确定的宽；0 = 未读
    h: usize,           // 首帧确定的高；0 = 未读
}

impl FrameStepper {
    /// 打开视频，准备流式解码。
    pub fn open(video: &std::path::Path) -> Result<Self> {
        use opencv::prelude::*;
        let path = video.to_string_lossy().to_string();
        let mut cap = opencv::videoio::VideoCapture::from_file(&path, opencv::videoio::CAP_ANY)
            .context("OpenCV 打开视频失败")?;
        if !cap.is_opened().context("is_opened 失败")? {
            anyhow::bail!("视频无法打开");
        }

        let fps = cap
            .get(opencv::videoio::CAP_PROP_FPS)
            .context("取 FPS 失败")?;
        let total_frames = cap
            .get(opencv::videoio::CAP_PROP_FRAME_COUNT)
            .context("取帧数失败")? as i64;
        let total_duration_ms = if fps > 0.0 {
            (total_frames as f64 / fps * 1000.0).round() as i64
        } else {
            0
        };

        Ok(Self {
            cap,
            total_duration_ms,
            total_frames,
            fps,
            decoded_count: 0,
            w: 0,
            h: 0,
        })
    }

    /// 当前帧分辨率 `(宽, 高)`。`next()` 被调用过、产出首帧后有效；否则 `(0, 0)`。
    pub fn dim(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    /// 视频总时长（毫秒）。
    pub fn total_duration_ms(&self) -> i64 {
        self.total_duration_ms
    }

    /// 视频总帧数。0 表示未知。
    pub fn total_frames(&self) -> i64 {
        self.total_frames
    }

    /// 拉下一帧（连续 BGR，行优先 H×W×3），EOF 返回 `None`。
    ///
    /// 返回 `(flat_bgr, pts_ms)`：`flat_bgr` 直接取自 `Mat.data_bytes()` 的行优先
    /// BGR（与 C++/OpenCV 完全一致，零中间拷贝）；`pts_ms` 为当前帧时间戳（毫秒），
    /// 用 `CAP_PROP_POS_MSEC`（OpenCV 后端即帧的真实呈现时间）。POS_MSEC 不可靠时
    /// 兜底用帧号 × 1000/fps。
    pub fn next(&mut self) -> Result<Option<(Vec<u8>, i64)>> {
        use opencv::prelude::*;
        let mut mat = opencv::core::Mat::default();
        if !self.cap.read(&mut mat).context("read 失败")? {
            return Ok(None); // EOF
        }
        let h = mat.rows() as usize;
        let w = mat.cols() as usize;
        if self.w == 0 {
            self.w = w;
            self.h = h;
        }
        let channels = mat.channels() as usize;
        if channels != 3 {
            anyhow::bail!("OpenCV 帧不是 3 通道 BGR（channels={}）", channels);
        }
        // OpenCV read() 输出连续 BGR，直接拷贝成 flat（行优先 H×W×3，无中间 Array3）。
        let data = mat.data_bytes().context("取 Mat 数据失败")?;
        let flat = data.to_vec();

        // PTS：优先 POS_MSEC（与 C++/OpenCV 语义一致），不可靠则用帧号估算。
        let pos_msec = self
            .cap
            .get(opencv::videoio::CAP_PROP_POS_MSEC)
            .context("取 POS_MSEC 失败")? as i64;
        let pts_ms = if pos_msec > 0 {
            pos_msec
        } else if self.fps > 0.0 {
            (self.decoded_count as f64 * 1000.0 / self.fps).round() as i64
        } else {
            self.decoded_count * 1000 / 30
        };
        self.decoded_count += 1;
        Ok(Some((flat, pts_ms)))
    }
}

/// 一次性全量回调解码（基于 `FrameStepper`）。`f` 返回 `false` 可提前停止。
///
/// `f` 收到 `(flat_bgr, pts_ms)`：行优先 BGR（H×W×3，0-255）与该帧真实
/// 呈现时间戳（毫秒）。
pub fn for_each_frame(
    video: &std::path::Path,
    mut f: impl FnMut(Vec<u8>, i64) -> Result<bool>,
) -> Result<()> {
    let mut stepper = FrameStepper::open(video)?;
    while let Some((flat, pts_ms)) = stepper.next()? {
        if !f(flat, pts_ms)? {
            break;
        }
    }
    Ok(())
}

/// 解码 channel 的缓冲帧数（有界背压：解码最多超前这么多帧）。
const PIPELINE_BOUND: usize = 32;

/// 流水线解码器：后台线程持续解码（`FrameStepper::next`），通过有界 channel 交给
/// 消费方。让 `cap.read` 解码与逐帧 transform 重叠执行——对齐 C++ 的解码超前线程
/// （C++ `g_threads` 解码到若干帧前），消除串行 decode→transform 的等待。
pub struct FramePipeline {
    rx: std::sync::mpsc::Receiver<Result<Option<(Vec<u8>, i64)>>>,
    w: usize,
    h: usize,
    total_duration_ms: i64,
    total_frames: i64,
    /// 首帧（`open()` 里同步解码以第一时间确定分辨率）；`recv` 时先吐它。
    first: Option<(Vec<u8>, i64)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FramePipeline {
    /// 打开视频并启动后台解码线程。
    ///
    /// 首帧在 `open()` 里同步解码（用于立即确定分辨率 `(w,h)`），后续帧由后台线程
    /// 持续解码、经有界 channel 押给消费方（背压让解码最多超前 `PIPELINE_BOUND` 帧）。
    pub fn open(video: &std::path::Path) -> Result<Self> {
        use anyhow::Context as _;
        let mut stepper = FrameStepper::open(video)?;
        let (total_duration_ms, total_frames) = (
            stepper.total_duration_ms(),
            stepper.total_frames(),
        );
        // 同步解首帧：既确定 dim，又作为 `recv` 的第一帧（保持帧序一致）。
        let first = match stepper.next().context("解码首帧失败")? {
            Some(f) => Some(f),
            None => None,
        };
        let (w, h) = stepper.dim();
        let (tx, rx) = std::sync::mpsc::sync_channel(PIPELINE_BOUND);
        let handle = std::thread::spawn(move || {
            let send = |item: Result<Option<(Vec<u8>, i64)>>| -> bool {
                tx.send(item).is_ok() // false ⇒ 消费方已 drop，停止
            };
            loop {
                let item = stepper.next();
                let done = match item {
                    Ok(Some((flat, pts))) => {
                        if !send(Ok(Some((flat, pts)))) {
                            break; // 消费方已 drop
                        }
                        continue; // 已送走，继续解下一帧
                    }
                    other => other, // EOF(Ok(None)) 或错误(Err) → 送哨兵后退出
                };
                if !send(done) {
                    break;
                }
                break;
            }
        });
        Ok(Self {
            rx,
            w,
            h,
            total_duration_ms,
            total_frames,
            first,
            handle: Some(handle),
        })
    }

    /// 当前帧分辨率 `(宽, 高)`；`open()` 后即可用。
    pub fn dim(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    /// 拉下一帧（阻塞）。`Ok(None)` 表示 EOF；`Err` 为解码错误。
    pub fn recv(&mut self) -> Result<Option<(Vec<u8>, i64)>> {
        if let Some(f) = self.first.take() {
            return Ok(Some(f));
        }
        match self.rx.recv() {
            Ok(Ok(Some(f))) => Ok(Some(f)),
            Ok(Ok(None)) => Ok(None),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None), // channel 断开（后台线程结束）→ EOF
        }
    }

    pub fn total_duration_ms(&self) -> i64 {
        self.total_duration_ms
    }
    pub fn total_frames(&self) -> i64 {
        self.total_frames
    }
}

impl Drop for FramePipeline {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
