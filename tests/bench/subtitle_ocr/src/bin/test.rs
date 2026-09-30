//! subtitle_ocr 三实现（cpp / py / rust）的灰盒正确性测试。
//!
//! 用法：
//!   cargo run -p bench_subtitle_ocr --bin test [--impl cpp|py|rust]
//!
//! 行为：直接读 tests/.test-frames 下的帧，跑对应实现并校验输出 JSON 结构。
//! 各实现输出形状不同，校验逻辑分开：
//! - cpp：`--dir` 批量，每项 `file` + `segments[]`（段内 text/confidence/box）；
//! - rust：逐帧单图模式，每项是 `FrameResult`——帧级 `text`/`confidence`
//!   + `boxes[]`（框内 text/text_confidence/box）；不走 `--dir`，因为该模式
//!   要求文件名为 `ms`/`ms_ms`，而 .test-frames 是 `frame_0000260.jpg` 命名。
//! py 未装依赖时优雅跳过（退出非 0）。

use std::path::PathBuf;
use std::process::Command;

use bench_subtitle_ocr::repo_root;

const IMPLS: &[&str] = &["cpp", "py", "rust"];

/// 帧源目录（仓库内既存的 tests/.test-frames，不再复制到包内临时目录）。
fn frames_dir() -> PathBuf {
    repo_root().join("tests").join(".test-frames")
}

fn models_dir() -> PathBuf {
    repo_root().join("data").join("models").join("rapidocr")
}

/// 统计帧目录下图片文件数。
fn count_frames() -> usize {
    let dir = frames_dir();
    assert!(dir.exists(), "帧源目录不存在: {}", dir.display());
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| {
            n.ends_with(".jpg")
                || n.ends_with(".jpeg")
                || n.ends_with(".png")
                || n.ends_with(".bmp")
        })
        .collect();
    names.len()
}

/// 校验 cpp --dir 输出的 JSON 结构（数组，每项含 file + 帧级 text + boxes[]）。
///
/// cpp 与 rust 的框数组现已统一为 `boxes`（框内 `text_confidence` / `box`），
/// 差别只剩 cpp 多一个 `file` 字段、rust 多一个帧级 `confidence`。
fn check_cpp_output(stdout: &str) {
    let v: serde_json::Value = serde_json::from_str(stdout).expect("cpp 输出不是合法 JSON");
    let arr = v.as_array().expect("cpp 输出应为 JSON 数组");
    let n = count_frames();
    assert_eq!(arr.len(), n, "应处理 {n} 帧，实际 {}", arr.len());
    for item in arr {
        assert!(item.get("file").is_some(), "缺 file 字段");
        let _text = item
            .get("text")
            .and_then(|t| t.as_str())
            .expect("帧级 text 应为字符串");
        let boxes = item
            .get("boxes")
            .and_then(|b| b.as_array())
            .expect("boxes 应为数组");
        for b in boxes {
            let _text = b
                .get("text")
                .and_then(|t| t.as_str())
                .expect("box.text 应为字符串");
            let conf = b
                .get("text_confidence")
                .and_then(|c| c.as_f64())
                .expect("box.text_confidence 应为数字");
            assert!((0.0..=1.0).contains(&conf), "confidence 越界: {conf}");
            let pts = b
                .get("box")
                .and_then(|x| x.as_array())
                .expect("box 应为数组");
            if !pts.is_empty() {
                assert_eq!(pts.len(), 4, "box 应为 4 点");
            }
        }
    }
}

fn run_cpp() -> bool {
    let bin = repo_root()
        .join("packages")
        .join("subtitle_ocr_cpp")
        .join("build")
        .join("subtitle_ocr_ort_cpp");
    if !bin.exists() {
        eprintln!(
            "[cpp] 二进制不存在: {}，先构建 packages/subtitle_ocr_cpp",
            bin.display()
        );
        return false;
    }
    let md = models_dir();
    let out = Command::new(&bin)
        .arg("--dir")
        .arg(frames_dir())
        .arg("0.5")
        .arg("--no-nms")
        .env("OCR_MODELS_DIR", &md)
        .env("OCR_KEYS_PATH", md.join("ppocr_keys.json"))
        .output()
        .expect("spawn cpp 失败");
    if !out.status.success() {
        eprintln!(
            "[cpp] 退出码非零:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return false;
    }
    check_cpp_output(&String::from_utf8_lossy(&out.stdout));
    println!("[cpp] 正确性测试通过（3 帧，JSON 结构校验）");
    true
}

fn run_py() -> bool {
    let script = repo_root()
        .join("packages")
        .join("subtitle_ocr_py")
        .join("main.py");
    if !script.exists() {
        eprintln!("[py] main.py 不存在，跳过");
        return false;
    }
    let frames = std::fs::read_dir(frames_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".jpg") || n.ends_with(".png"))
        .collect::<Vec<_>>();
    for f in frames {
        let out = Command::new("python3")
            .arg(&script)
            .arg(frames_dir().join(&f))
            .arg("--subtitle-only")
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let v: serde_json::Value =
                    serde_json::from_slice(&o.stdout).expect("py 输出非法 JSON");
                assert!(v.get("lines").is_some(), "py 输出缺 lines");
            }
            _ => {
                eprintln!("[py] 调用失败（可能未装 rapidocr_onnxruntime）");
                return false;
            }
        }
    }
    println!("[py] 正确性测试通过");
    true
}

/// 校验 rust 输出的 JSON 结构（数组，每项一个 `FrameResult`）。
///
/// 与 cpp 形状**不同**（见 [`check_cpp_output`]）：rust 的 `FrameResult` 已把
/// 多框聚合成帧级 `text` / `confidence`，明细放 `boxes[]`（框内字段是 `box`
/// 与 `text_confidence`），且不含 `file` / `segments`。
fn check_rust_output(stdout: &str, expect_len: usize) {
    let v: serde_json::Value = serde_json::from_str(stdout).expect("rust 输出不是合法 JSON");
    let arr = v.as_array().expect("rust 输出应为 JSON 数组");
    assert_eq!(
        arr.len(),
        expect_len,
        "应输出 {expect_len} 项，实际 {}",
        arr.len()
    );
    for item in arr {
        let _text = item
            .get("text")
            .and_then(|t| t.as_str())
            .expect("帧级 text 应为字符串");
        let conf = item
            .get("confidence")
            .and_then(|c| c.as_f64())
            .expect("帧级 confidence 应为数字");
        assert!((0.0..=1.0).contains(&conf), "confidence 越界: {conf}");
        assert!(item.get("timestamp").is_some(), "缺 timestamp 字段");
        let boxes = item
            .get("boxes")
            .and_then(|b| b.as_array())
            .expect("boxes 应为数组");
        for b in boxes {
            let _t = b
                .get("text")
                .and_then(|t| t.as_str())
                .expect("box.text 应为字符串");
            let bc = b
                .get("text_confidence")
                .and_then(|c| c.as_f64())
                .expect("box.text_confidence 应为数字");
            assert!((0.0..=1.0).contains(&bc), "box confidence 越界: {bc}");
            let pts = b
                .get("bbox")
                .and_then(|x| x.as_array())
                .expect("box.bbox 应为数组");
            assert_eq!(pts.len(), 4, "box 应为 4 点");
        }
    }
}

fn run_rust() -> bool {
    // 与 cpp 不同，rust 二进制产出在 workspace 根 target/（非包内 target/）。
    let bin = repo_root()
        .join("target")
        .join("release")
        .join("subtitle_ocr");
    if !bin.exists() {
        eprintln!(
            "[rust] 二进制不存在: {}，先 cargo build --release -p subtitle_ocr",
            bin.display()
        );
        return false;
    }
    // 不用 --dir：该模式要求文件名是 ms / ms_ms（编码时刻），而 .test-frames 下是
    // frame_0000260.jpg 这种命名，会被 --on-bad-name skip 全部跳过、输出空数组。
    // 这里逐帧走单图模式（timestamp 恒为 0），只校验结构。
    let mut frames: Vec<PathBuf> = std::fs::read_dir(frames_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("jpg" | "jpeg" | "png" | "bmp")
            )
        })
        .collect();
    frames.sort();
    assert_eq!(frames.len(), count_frames(), "帧数统计不一致");

    for f in &frames {
        // rust CLI 用 --model-dir 传模型目录（cpp 走 OCR_MODELS_DIR 环境变量）。
        let out = Command::new(&bin)
            .arg(f)
            .arg("--model-dir")
            .arg(models_dir())
            .output()
            .expect("spawn rust 失败");
        if !out.status.success() {
            eprintln!(
                "[rust] 退出码非零 ({}):\n{}",
                f.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            return false;
        }
        // 单图模式输出单元素数组。
        check_rust_output(&String::from_utf8_lossy(&out.stdout), 1);
    }
    println!(
        "[rust] 正确性测试通过（{} 帧，JSON 结构校验）",
        frames.len()
    );
    true
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut impl_ = "cpp".to_string();
    for (i, a) in args.iter().enumerate() {
        if a == "--impl" {
            if let Some(v) = args.get(i + 1) {
                impl_ = v.clone();
            }
        }
    }
    let ok = match impl_.as_str() {
        "cpp" => run_cpp(),
        "py" => run_py(),
        "rust" => run_rust(),
        other => {
            eprintln!("未知实现: {other}（可选: {}）", IMPLS.join(" | "));
            std::process::exit(2);
        }
    };
    if !ok {
        eprintln!("[{impl_}] 正确性测试未通过 / 未实现");
        std::process::exit(1);
    }
}
