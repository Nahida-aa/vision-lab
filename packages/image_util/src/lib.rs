//! 图像工具：不含业务语义的纯图像转换/处理辅助。
//!
//! 与具体业务（OCR、颜色分析、操作注入）解耦——谁需要「抓来的 RGBA 图转 RGB」、
//! 「降采样」「裁剪」这类通用操作，就依赖本 crate，而不是把这些helper散落在
//! `ocr_agent` / `screen_operator` 里。
//!
//! 当前提供：
//! - [`rgba_to_rgb`]：抓图后端（capturer）给的 `RgbaImage` → `RgbImage`（丢 alpha）。

use image::{RgbImage, RgbaImage};

/// 把 capturer 抓来的 `RgbaImage` 转成 `RgbImage`（丢弃 alpha）。
///
/// OCR / 颜色分析 / 计算缩放都不需要 alpha 通道，转掉省内存也省下游分支。
pub fn rgba_to_rgb(img: &RgbaImage) -> RgbImage {
    image::DynamicImage::ImageRgba8(img.clone()).to_rgb8()
}

/// 从 RGB 图裁出矩形（手写像素拷贝，不依赖 image 的 crop API 版本差异）。
pub fn crop_rgb(img: &RgbImage, x: u32, y: u32, w: u32, h: u32) -> RgbImage {
    let mut out = RgbImage::new(w, h);
    for yy in 0..h {
        for xx in 0..w {
            *out.get_pixel_mut(xx, yy) = *img.get_pixel(x + xx, y + yy);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn crops_sub_region() {
        let mut img = RgbImage::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                *img.get_pixel_mut(x, y) = Rgb([(x * 10) as u8, (y * 10) as u8, 0]);
            }
        }
        let out = crop_rgb(&img, 1, 2, 2, 2);
        assert_eq!(out.dimensions(), (2, 2));
        assert_eq!(*out.get_pixel(0, 0), Rgb([10, 20, 0]));
        assert_eq!(*out.get_pixel(1, 1), Rgb([20, 30, 0]));
    }
}
