//! 关键帧落盘：把 [`crate::Keyframe`] 列表写成帧图 / 掩码 / 时间轴 / JSON。
//!
//! 这是「库方式使用」的出口：拿到 [`crate::find_keyframes`] 返回的
//! [`crate::Keyframe`] 后，一行 [`write_artifacts`] 就能按需落盘，无需了解
//! 目录/文件名的内部约定。CLI 与库调用方共用，文件名约定集中在此。

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::ValueEnum;

use crate::Keyframe;

/// 输出模式：控制落盘哪些产物。
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    /// 写关键帧 PNG + 掩码 PNG + timeline.txt + keyframes.json（默认）。
    Full,
    /// 只写 timeline.txt（纯算法计时，不落盘图片）。
    Timeline,
    /// 仅写原始关键帧 PNG（RGB，含背景），不写掩码 / json / timeline。
    Frames,
}

impl OutputMode {
    fn write_frames(self) -> bool {
        matches!(self, OutputMode::Full | OutputMode::Frames)
    }
    fn write_mask(self) -> bool {
        matches!(self, OutputMode::Full)
    }
    fn write_json(self) -> bool {
        matches!(self, OutputMode::Full)
    }
    fn write_timeline(self) -> bool {
        matches!(self, OutputMode::Full | OutputMode::Timeline)
    }
}

/// [`write_artifacts`] 的落盘结果摘要。
#[derive(Debug, Clone)]
pub struct WriteReport {
    /// 原始关键帧 PNG 所在目录（`frames/`）。
    pub frames_dir: PathBuf,
    /// 掩码 PNG 所在目录（`mask/`，仅 `Full` 模式有意义）。
    pub mask_dir: PathBuf,
    /// 写了多少张原始关键帧。
    pub frames_written: usize,
    /// 写了多少张掩码（`Full` 模式下 = 关键帧数）。
    pub masks_written: usize,
    /// `timeline.txt` 内容（`{start_ms},{end_ms}\n` 每行）。
    pub timeline: String,
    /// `keyframes.json` 内容（结构化列表）。
    pub json: String,
    /// 输出根目录（`frames/`、`mask/` 等子目录的父目录）。
    pub out_dir: PathBuf,
}

/// 把关键帧列表落盘，目录/文件名约定集中在此（`frames/`、`mask/` 子目录，
/// 文件名 `{start_ms}_{end_ms}.png`）。
///
/// - `out_dir` 为输出根目录，会自动创建（含子目录）。
/// - `mode` 控制写哪些产物（见 [`OutputMode`]）。
/// - `progress` 每写一张原始关键帧回调一次 `(已写张数)`，供调用方挂进度条；
///   不需要进度传 `&mut |_| {}` 即可。
///
/// 文件名 `start_ms_end_ms` 即 subtitle_ocr `--dir` 的 `ms_ms` 时间区间约定，
/// 因此 `frames/` 目录可直接喂下游 OCR。
pub fn write_artifacts(
    kfs: &[Keyframe],
    out_dir: &Path,
    mode: OutputMode,
    progress: &mut dyn FnMut(usize),
) -> Result<WriteReport> {
    let frames_dir = out_dir.join("frames");
    let mask_dir = out_dir.join("mask");
    std::fs::create_dir_all(&frames_dir)?;
    if mode.write_mask() {
        std::fs::create_dir_all(&mask_dir)?;
    }

    let mut frames_written = 0usize;
    let mut masks_written = 0usize;
    let mut timeline = String::new();
    let mut json = Vec::new();

    for kf in kfs {
        // 文件名 `start_ms_end_ms`：时间区间互不重叠，天然唯一。
        let name = format!("{}_{}", kf.start_ms, kf.end_ms);

        if mode.write_frames() {
            // 原始帧 PNG（BGR → RGB，含背景）→ frames/。
            let path = frames_dir.join(format!("{}.png", name));
            save_png(&path, &kf.frame)?;
            frames_written += 1;
        }
        if mode.write_mask() {
            // 去背景字幕前景 PNG（黑底白字，对应 VideoSubFinder 的 ISA 图）→ mask/。
            let mask_path = mask_dir.join(format!("{}.png", name));
            save_mask_png(&mask_path, &kf.mask)?;
            masks_written += 1;

            json.push(format!(
                "{{\"start_ms\":{},\"end_ms\":{},\"image\":\"{}.png\",\"mask\":\"{}.png\"}}",
                kf.start_ms, kf.end_ms, name, name
            ));
        }
        if mode.write_timeline() {
            timeline.push_str(&format!("{},{}\n", kf.start_ms, kf.end_ms));
        }
        progress(frames_written);
    }

    if mode.write_timeline() {
        std::fs::write(out_dir.join("timeline.txt"), &timeline)?;
    }
    let json_out = if mode.write_json() {
        let s = format!("[{}]\n", json.join(","));
        std::fs::write(out_dir.join("keyframes.json"), &s)?;
        s
    } else {
        String::new()
    };

    Ok(WriteReport {
        frames_dir,
        mask_dir,
        frames_written,
        masks_written,
        timeline,
        json: json_out,
        out_dir: out_dir.to_path_buf(),
    })
}

/// 把 BGR `Array3`（H×W×3）存为 PNG（转 RGB）。
pub fn save_png(path: &Path, arr: &ndarray::Array3<u8>) -> Result<()> {
    let (h, w, _) = arr.dim();
    let mut rgb = Vec::with_capacity(h * w * 3);
    for y in 0..h {
        for x in 0..w {
            // ndarray 存 BGR → PNG 要 RGB。
            rgb.push(arr[[y, x, 2]]); // R
            rgb.push(arr[[y, x, 1]]); // G
            rgb.push(arr[[y, x, 0]]); // B
        }
    }
    let img = image::RgbImage::from_raw(w as u32, h as u32, rgb)
        .ok_or_else(|| anyhow::anyhow!("构造 RgbImage 失败"))?;
    img.save(path)?;
    Ok(())
}

/// 把字幕前景 mask `Array2`（H×W，255=文字）存为 PNG（黑底白字）。
pub fn save_mask_png(path: &Path, mask: &ndarray::Array2<u8>) -> Result<()> {
    let (h, w) = mask.dim();
    // 直接作为灰度图（0=黑背景，255=白字幕）。
    let gray = image::GrayImage::from_raw(w as u32, h as u32, mask.iter().copied().collect())
        .ok_or_else(|| anyhow::anyhow!("构造 GrayImage 失败"))?;
    gray.save(path)?;
    Ok(())
}
