//! 批量模式辅助：目录扫描 + 文件名时刻解析（`ms` / `ms_ms` 约定）。
//!
//! 这一层把「磁盘上的图片目录 → 一组 [`subtitle_ocr::OcrEntry`]」的逻辑独立出来，
//! 与 CLI 参数解析、JSON 输出解耦。核心 OCR 流程见 [`subtitle_ocr::ocr_entries`]。

use anyhow::{Context, Result};
use clap::ValueEnum;
use std::path::Path;
use crate::{FrameTimes, OcrEntry};
use tracing::warn;

/// 批量模式下，文件名不符合时间格式时的处理策略。
///
/// CLI 默认 [`BadNameAction::Error`]（直接终止）；库调用方可按需选 [`BadNameAction::Warn`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BadNameAction {
    /// 跳过该文件并打印警告（不终止）。
    Warn,
    /// 直接报错终止（CLI 默认）。
    Error,
}

/// 解析文件名里的时刻：支持 `ms` 或 `ms_ms`，可前置多余 0。
///
/// 例如 `001234.png` → `Single(1234)`；`001234_001250.png` → `Range(1234, 1250)`。
/// 返回 `None` 表示文件名不符合格式（无扩展名 / 非纯数字 / 段数 >2）。
pub fn parse_name_times(stem: &str) -> Option<FrameTimes> {
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    let mut nums = Vec::with_capacity(parts.len());
    for p in parts {
        // 允许前置多余 0；空段（如 `__`）非法。
        if p.is_empty() {
            return None;
        }
        // 仅接受十进制数字（前置 0 自动被 u64 解析忽略）。
        if !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        nums.push(p.parse::<u64>().ok()?);
    }
    match nums.as_slice() {
        [t] => Some(FrameTimes::Single(*t)),
        [s, e] => Some(FrameTimes::Range(*s, *e)),
        _ => None,
    }
}

/// 列出目录下图片文件，解析文件名时刻并按数值时间排序（对齐 cpp listFrames）。
///
/// 文件名须为 `ms` 或 `ms_ms`（可前置 0）形式，否则按 `on_bad` 处理：
/// `skip` 跳过并警告，`error` 直接报错。返回的每个 [`OcrEntry`] 带解析出的时刻。
pub fn list_frames(dir: &Path, on_bad: BadNameAction) -> Result<Vec<OcrEntry>> {
    let mut entries: Vec<OcrEntry> = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for e in read.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_lowercase());
            if !matches!(ext.as_deref(), Some("jpg" | "jpeg" | "png" | "bmp")) {
                continue;
            }
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .context("文件名非 UTF-8")?;
            match parse_name_times(stem) {
                Some(times) => entries.push(OcrEntry { path: p, times }),
                None => match on_bad {
                    BadNameAction::Warn => {
                        warn!(path = %p.display(), "文件名不符合 ms/ms_ms 格式，已跳过");
                    }
                    BadNameAction::Error => {
                        anyhow::bail!("文件名不符合 ms/ms_ms 时间格式: {}", p.display());
                    }
                },
            }
        }
    }
    // 按主时刻数值排序（保证时间顺序，不受前置 0 / 字典序影响）。
    entries.sort_by_key(|e| e.times.sort_key());
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_name_times_single() {
        assert_eq!(parse_name_times("001234"), Some(FrameTimes::Single(1234)));
        assert_eq!(parse_name_times("0"), Some(FrameTimes::Single(0)));
        assert_eq!(parse_name_times("999"), Some(FrameTimes::Single(999)));
    }

    #[test]
    fn parse_name_times_range() {
        // ms_ms：两段，前置 0 不影响数值。
        assert_eq!(
            parse_name_times("001234_001250"),
            Some(FrameTimes::Range(1234, 1250))
        );
        assert_eq!(
            parse_name_times("1234_1250"),
            Some(FrameTimes::Range(1234, 1250))
        );
    }

    #[test]
    fn parse_name_times_invalid() {
        // 三段（超过 2 段）非法。
        assert_eq!(parse_name_times("132_932_0"), None);
        // 非数字 / 空段非法。
        assert_eq!(parse_name_times("abc"), None);
        assert_eq!(parse_name_times("1234_"), None);
        assert_eq!(parse_name_times("_1234"), None);
        // 带扩展名前缀（整名）不在此函数处理范围，但这里只测 stem。
        assert_eq!(parse_name_times("12.3"), None);
    }

    /// 在临时目录放若干图片，验证 list_frames 的解析/排序/双产出/skip。
    ///
    /// `tag` 须各测试唯一：cargo test 默认多线程并行，若两个测试算出同一目录，
    /// 一方的 remove_dir_all/create_dir_all 会在另一方扫描目录时抽走文件，
    /// 造成随机失败（曾用 files.len() 作后缀，bad_name_error 与 bad_name_skip
    /// 都是 2 个文件而撞名）。
    fn make_tmp_dir(tag: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("sf_ocr_list_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"dummy").unwrap();
        }
        dir
    }

    #[test]
    fn list_frames_parses_and_sorts() {
        let dir = make_tmp_dir("parses_and_sorts", &[
            "00500.png",
            "00100.png",
            "00300_00350.png", // ms_ms：双时刻
            "ignore.txt",      // 非图片，跳过
        ]);
        let entries = list_frames(&dir, BadNameAction::Warn).unwrap();
        // 按主时刻排序：100, 300(eff), 500。
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].times, FrameTimes::Single(100));
        assert_eq!(entries[1].times, FrameTimes::Range(300, 350)); // 双产出
        assert_eq!(entries[2].times, FrameTimes::Single(500));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_frames_bad_name_error() {
        let dir = make_tmp_dir("bad_name_error", &["00100_00150.png", "badname.png"]);
        let r = list_frames(&dir, BadNameAction::Error);
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_frames_bad_name_skip() {
        let dir = make_tmp_dir("bad_name_skip", &["00100_00150.png", "badname.png"]);
        let entries = list_frames(&dir, BadNameAction::Warn).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].times, FrameTimes::Range(100, 150));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
