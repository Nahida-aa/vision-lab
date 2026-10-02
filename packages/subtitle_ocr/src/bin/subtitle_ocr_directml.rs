//! 专用 bin：`subtitle_ocr_directml <image>` 或 `subtitle_ocr_directml --dir <dir> ...`
//!
//! 字幕 OCR 的 **DirectML（GPU）执行后端专用 CLI**，与主 bin `subtitle_ocr`
//! （CPU）**不共用旗标**：后端差异在二进制层面分开，而不是挂在主 bin 的参数上
//! （bin 名带 `subtitle_ocr_` 前缀：避免泛化的 `directml` 与将来其他包的 GPU
//! 版撞名）。两者旗标集一致、执行流程共用（见 `crate::cli`），仅溯源 meta 不同
//! （engine `ort-rust-directml` / device `Directml`）。
//!
//! 构建与运行前提（见 Cargo.toml / rapidocr_ort [features] 注释）：
//! - 本 bin 无条件编译（普通 cargo build 即有产物）；但**实际执行** DirectML
//!   需以 `ep-directml` feature 构建，缺 feature 的产物在运行时建引擎一步
//!   硬报错（rapidocr_ort ep.rs），不会静默回退 CPU；
//! - VS2019 工具链下用 `--no-default-features` 走 ort 动态链接（运行时的
//!   `onnxruntime.dll` 需含 DirectML EP，如 MS 官方 DirectML nuget）；
//! - 多适配器机器用 `ORT_EP_DEVICE_ID` 指定 GPU 序号（0 未必是独显，本机
//!   实测 0 = Todesk 虚拟适配器、1 = RTX 3060，详见
//!   docs/benchmark-cpu-vs-gpu-windows.md）。
//!
//! 输出与主 bin 同形状（JSON 数组：`text` / `text_confidence` / `boxes` /
//! `timestamp`），仅执行设备不同。

use anyhow::Result;
use clap::Parser;
use subtitle_ocr::cli::{self, RunArgs};
use subtitle_ocr::util::BadNameAction;
use subtitle_ocr::{ExecutionBackend, OcrDevice};

#[derive(Parser, Debug)]
#[command(
    name = "subtitle_ocr_directml",
    about = "字幕 OCR（DirectML/GPU 执行后端专用；CPU 走主 bin subtitle_ocr）"
)]
struct Cli {
    /// 模型套件：v3 / v4 / v6-tiny / v6-medium
    #[arg(long, value_enum, default_value_t = rapidocr_ort::ModelProfile::V4)]
    model: rapidocr_ort::ModelProfile,

    /// 模型目录（默认仓库根 data/models/rapidocr，可经 RAPIDOCR_MODEL_DIR 覆盖）
    #[arg(long, env = "RAPIDOCR_MODEL_DIR", default_value = rapidocr_ort::DEFAULT_MODEL_DIR)]
    model_dir: String,

    /// 输入图片路径（单图模式，不携带时间戳，输出 timestampMs=0；与 --dir 互斥）
    image: Option<String>,

    /// 批量模式：输入图片目录（jpg/jpeg/png/bmp，按文件名排序逐张识别，与 <image> 互斥）。
    ///
    /// 文件名须为 `ms` 或 `ms_ms` 形式，编码该图对应的时刻（毫秒），可前置多余 0：
    /// - `001234.png`        → 单时刻 1234
    /// - `001234_001250.png` → 双时刻 [1234, 1250]（同一张图仅识别一次，产出两个结果）
    /// 不符合格式时按 `--on-bad-name` 处理（`skip` 跳过 / `error` 报错）。
    #[arg(long)]
    dir: Option<String>,

    /// 批量模式（`--dir`）下，文件名不符合 `ms` / `ms_ms` 时间格式时的处理：
    /// `error` 直接报错终止（默认，避免静默丢帧）；`skip` 跳过并警告。
    #[arg(long, value_enum, default_value_t = BadNameAction::Error)]
    on_bad_name: BadNameAction,

    /// 识别置信度下限（对应 cpp 的 text_score / 下游 --text-confidence-threshold，默认 0.5）
    #[arg(long)]
    text_confidence_threshold: Option<f32>,

    /// 仅保留画面底部比例区间的字幕框（cpp --subtitle-only）
    #[arg(long)]
    subtitle_only: bool,

    /// 关闭重叠框 NMS 去重（cpp --no-nms）
    #[arg(long)]
    no_nms: bool,

    /// 关闭 bottom_only：对整帧做 OCR（cpp 默认开启，故本包默认开启，此 flag 取反）
    #[arg(long)]
    full_frame: bool,

    /// 用 cpp 同款的透视矫正裁剪（warpPerspective）替代轴对齐包围盒。
    /// 与 det 几何 minAreaRect 耦合使用（实验对齐 cpp 用）。
    #[arg(long)]
    warp_crop: bool,

    /// 推理线程数（预留；当前 OcrEngine 固定 4，与 cpp 默认一致）
    #[arg(long)]
    threads: Option<usize>,

    /// 输出文件路径（完整文件名，由调用方决定，如 `asr_ocr_frames.json`）：写入
    /// `OcrFramesResult` 结构（各帧结果 + 溯源 meta），便于对接 LocalDub 的
    /// `asr_ocr_frames.json` / `sf_ocr_frames.json` 等。指定后结果仅落盘，不再向
    /// stdout 打印逐帧 JSON 数组（避免与文件重复刷屏）；不指定时仅向 stdout 打印。
    #[arg(long)]
    out: Option<String>,
}

fn main() -> Result<()> {
    cli::init_tracing();
    let c = Cli::parse();

    cli::run(RunArgs {
        model: c.model,
        model_dir: c.model_dir,
        image: c.image,
        dir: c.dir,
        on_bad_name: c.on_bad_name,
        text_confidence_threshold: c.text_confidence_threshold,
        subtitle_only: c.subtitle_only,
        no_nms: c.no_nms,
        full_frame: c.full_frame,
        warp_crop: c.warp_crop,
        out: c.out,
        execution_backend: ExecutionBackend::DirectML,
        engine: "ort-rust-directml",
        device: OcrDevice::Directml,
    })
}
