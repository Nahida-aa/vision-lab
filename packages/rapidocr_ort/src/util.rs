//! 通用工具（图像加载等），供库与二进制复用。

use anyhow::{Context, Result};
use ndarray::Array3;
use std::path::Path;

/// 读图为 HWC u8 的 `Array3`，交回 **BGR** 通道序。
///
/// 为什么是 BGR：PP-OCR 模型按 `cv2.imread` / `cv::imread`（BGR）训练，喂 RGB 会让
/// 彩色文字出现漏检/误识。`image::open` 给的是 RGB，故这里 `swap(0,2)` 转成 BGR。
/// 这是整个仓库（cpp `cv::imread`、subtitle_ocr 生产路径、本包测试）统一的约定，
/// `detect` 输入也按 BGR 处理，上层无需再自行转换。
pub fn load_image(path: &Path) -> Result<Array3<u8>> {
    let img = image::open(path)
        .with_context(|| format!("读取图片失败: {}", path.display()))?
        .to_rgb8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut data = img.into_raw();
    // RGB→BGR：image crate 给 RGB，模型要 BGR（对齐 cpp cv::imread）。
    for px in data.chunks_mut(3) {
        px.swap(0, 2);
    }
    Array3::from_shape_vec((h, w, 3), data).context("图像数据重塑失败（维度不匹配）")
}