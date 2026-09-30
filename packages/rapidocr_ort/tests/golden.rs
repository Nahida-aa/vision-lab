//! Golden / 回归测试：对已知样本图跑整条 OCR pipeline，断言识别文字与置信度。
//!
//! 目的：锁死本次 debug 修复的三个根因——cls 0.9 旋转阈值、PP-OCRv4 字典
//! （blank@0 + 6623 表 + space@end 的 6625 格式）、rec 前处理——不让它们
//! 在后续改动中悄悄回归。
//!
//! 模型权重较大、加载较慢，故默认 `#[ignore]`，CI/本地手动用
//! `cargo test -p rapidocr_ort --test golden -- --ignored` 跑。

use std::path::PathBuf;

use rapidocr_ort::{DEFAULT_MODEL_DIR, ModelProfile, OcrEngine};

/// 仓库根：`packages/rapidocr_ort` 的上两级。
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .parent()
        .expect("parent")
        .to_path_buf()
}

fn engine_v4() -> OcrEngine {
    let model_dir = repo_root().join(DEFAULT_MODEL_DIR);
    OcrEngine::from_profile(ModelProfile::V4, &model_dir)
        .expect("加载 v4 引擎失败")
        .with_warp_crop(false)
}

/// 复现 "不过" 误识别根因：cls 输出 [0.347, 0.652] 的弱倒置信号，
/// 旧版按 0.5 阈值（out[1] > out[0]）把正立字幕误旋 180°，rec 错识成
/// "RL"/"FL"。修复后（cls 阈值 0.9）应正确读出 "不过"，置信度 > 0.99。
#[test]
#[ignore = "加载模型权重较重，手动运行：cargo test -p rapidocr_ort --test golden -- --ignored"]
fn v4_bu_guo_frame_reads_correctly() {
    let img_path = repo_root()
        .join("tmp/大/09/sf_ocr_pre/frames/23900_24300.png");
    let img = rapidocr_ort::load_image(&img_path).expect("读取 golden 图失败");
    let mut engine = engine_v4();
    let results = engine.detect(&img).expect("OCR 推理失败");

    assert!(
        !results.is_empty(),
        "golden 图应至少识别到一个文字框，实际为空（cls/det 可能回归）"
    );

    // 期望整帧主文字为 "不过"（可能夹带少量其他框，故检查是否包含该串）。
    let joined: String = results.iter().map(|r| r.text.as_str()).collect();
    assert!(
        joined.contains("不过"),
        "golden 图主文字应为 \"不过\"，实际识别结果：{:?}",
        results.iter().map(|r| &r.text).collect::<Vec<_>>()
    );

    // 主框（置信度最高）的置信度必须足够高，排除 "RL"/"FL" 之类低置信误读。
    let max_tc = results
        .iter()
        .map(|r| r.text_confidence)
        .fold(0.0_f64, |acc: f64, c: f32| acc.max(c as f64));
    assert!(
        max_tc > 0.99,
        "golden 图主框置信度应 > 0.99，实际 {max_tc}",
    );
}
