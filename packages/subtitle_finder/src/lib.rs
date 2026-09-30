//! 吃视频 → 输出字幕关键帧图 + 时间轴（复刻 VideoSubFinder 的筛选管线）。
//!
//! 本包只做「找到字幕变化的关键帧 + 时间轴」，不做 OCR 识别（那是
//! `rapidocr_ort` 下游的事），对应官方 `VideoSubFinder → RapidVideOCR` 分工。
//!
//! 设计见 `DESIGN.md`。核心是 `FastSearchSubtitles` 状态机（DL=6 滑动窗口 +
//! 相邻帧交集去噪 + 水平投影检测文字 + 跨帧比较判断字幕变化），精确复刻
//! VideoSubFinder 的 30fps 逐帧时间行为，避免抽样帧的时间偏移。

pub mod compare;
pub mod filter;
pub mod frame;
pub mod imgops;
pub mod output;
pub mod params;
pub mod preprocess;
pub mod state;

use anyhow::Result;

/// 一个字幕关键帧：一段字幕的一张代表帧 + 时间轴。
#[derive(Debug, Clone)]
pub struct Keyframe {
    /// 字幕段起始时间（毫秒）。
    pub start_ms: u64,
    /// 字幕段结束时间（毫秒）。
    pub end_ms: u64,
    /// 代表帧的 BGR 图像（H×W×3，0-255，含背景）。
    pub frame: ndarray::Array3<u8>,
    /// 去背景字幕前景 mask（H×W，255=字幕文字，0=背景）。
    /// 对应 VideoSubFinder 的 ImISA（交叠前景图），供 OCR 识别用。
    pub mask: ndarray::Array2<u8>,
}

/// 对视频逐帧筛选，输出字幕关键帧 + 时间轴。
///
/// `video` 为视频路径；`params` 为筛选参数（对齐 VideoSubFinder 默认值，
/// 见 `params::Params::default()`）。
///
/// 拿到 [`Vec<Keyframe>`] 后，可用 [`output::write_artifacts`] 落盘（帧图 /
/// 掩码 / timeline / json），或用 `Keyframe.frame` / `.mask` 自行处理。
pub fn find_keyframes(video: &std::path::Path, params: &params::Params) -> Result<Vec<Keyframe>> {
    state::find_keyframes(video, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_defaults_match_videosubfinder() {
        let p = params::Params::default();
        assert_eq!(p.dl, 6);
        assert_eq!(p.segh, 3);
        assert!((p.tp - 0.3).abs() < 1e-6);
        assert!((p.mtpl - 0.022).abs() < 1e-6);
        assert!((p.veple - 0.30).abs() < 1e-6);
        assert!((p.ilaple - 0.30).abs() < 1e-6);
        assert_eq!(p.max_dl_down, 20);
        assert_eq!(p.max_dl_up, 40);
    }

    /// 集成测试：用 bench 的真实视频解码前几帧，验证 30fps 解码管线 + 分辨率正确。
    /// 依赖 tests/bench/subtitle_ocr/ref/video_source.mp4 存在（不入库，需自备）。
    #[test]
    #[ignore] // 依赖真实视频文件，手动跑
    fn decodes_video_frames() {
        let video = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests/bench/subtitle_ocr/ref/video_source.mp4");
        assert!(video.exists(), "缺少测试视频: {}", video.display());

        let mut count = 0usize;
        frame::for_each_frame(&video, |_flat, _pts_ms| {
            count += 1;
            // 只解前 30 帧验证管线，避免全量慢。
            Ok(count < 30)
        })
        .expect("解码失败");

        // 用 stepper 单独验证分辨率（for_each_frame 回调不携带维度）。
        let stepper = frame::FrameStepper::open(&video).expect("打开失败");
        let mut count2 = 0usize;
        let mut dims = None;
        let mut st = stepper;
        while let Some((_flat, _pts)) = st.next().expect("解码失败") {
            count2 += 1;
            if count2 == 1 {
                dims = Some(st.dim());
            }
            if count2 >= 2 {
                break;
            }
        }

        assert_eq!(dims, Some((1280, 720)), "720p 分辨率 (w,h)");
        assert_eq!(count, 30);
    }

    /// 集成测试：跑完整状态机，验证能输出关键帧（非空、时间轴有序）。
    /// 依赖 tests/bench/subtitle_ocr/ref/video_source.mp4 存在（不入库，需自备）。
    #[test]
    #[ignore] // 全量解码 5100 帧较慢，手动跑
    fn runs_state_machine_produces_keyframes() {
        let video = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests/bench/subtitle_ocr/ref/video_source.mp4");
        assert!(video.exists(), "缺少测试视频: {}", video.display());

        let kfs = find_keyframes(&video, &params::Params::default()).expect("find_keyframes 失败");
        eprintln!("subtitle_finder 输出 {} 个关键帧", kfs.len());
        // 时间轴应非递减。
        for w in kfs.windows(2) {
            assert!(w[0].start_ms <= w[1].start_ms, "时间轴应有序");
        }
        // 至少应有一些关键帧。
        assert!(!kfs.is_empty(), "应找到字幕关键帧");
    }
}
