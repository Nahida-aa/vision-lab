// OpenCV 主版本探测：OpenCV 5 给 `warp_perspective` 增加了第 8 参
// `AlgorithmHint`，4.x 没有该参数（且 4.x 默认行为即 accurate，与 5 的
// ALGO_HINT_ACCURATE 等价）。opencv crate 只给自身发 `ocvrs_opencv_branch_5`
// cfg，且无 links 键（无 DEP_* 元数据可传播），下游拿不到——这里重新解析
// 主版本并给本 crate 发同名 cfg，供 src/pipeline.rs 按 cfg 分流。
//
// 探测顺序（覆盖 opencv crate 实际会用的发现方式）：
//   1. `OPENCV_INCLUDE_PATHS`（`;` / `:` 分隔的多路径；Windows env-probe 与
//      CI release-windows.yml 走这条）
//   2. `pkg-config --cflags opencv5` / `opencv4`（Linux 系统安装走这条，
//      解析输出里的 `-I` 路径）
//
// 全部失败时**按 5 处理并发 cargo 警告**，不硬失败：本仓 CI 与 Linux 本机
// 均为 5，main 的无条件 8 参调用本就要求 5——默认 5 与其行为一致；4.x 环境
// （如 VS2019 + 4.10 prebuilt）必须显式设 OPENCV_INCLUDE_PATHS。警告是刻意的：
// 宁可吵，也不静默选错分支给出难懂的 E0061。
fn main() {
    println!("cargo::rerun-if-env-changed=OPENCV_INCLUDE_PATHS");
    match detect_major_version() {
        Some(major) => emit_cfg(major),
        None => {
            println!(
                "cargo::warning=无法探测 OpenCV 主版本（OPENCV_INCLUDE_PATHS 未设置或找不到版本头，\
                 pkg-config opencv5/opencv4 亦失败），按 OpenCV 5 处理；\
                 若实际为 4.x，请设置 OPENCV_INCLUDE_PATHS 指向其 include 目录"
            );
            emit_cfg(5);
        }
    }
}

fn emit_cfg(major: u32) {
    println!("cargo::rustc-check-cfg=cfg(ocvrs_opencv_branch_5)");
    if major >= 5 {
        println!("cargo::rustc-cfg=ocvrs_opencv_branch_5");
    }
}

/// 依次尝试各探测来源，取第一个能读到版本头的 include 目录。
fn detect_major_version() -> Option<u32> {
    env_include_paths()
        .into_iter()
        .chain(pkg_config_include_paths())
        .find_map(read_major_version)
}

/// `OPENCV_INCLUDE_PATHS`：`;` / `:` 分隔的目录列表（与 opencv crate 同名 env 对齐）。
fn env_include_paths() -> Vec<String> {
    match std::env::var("OPENCV_INCLUDE_PATHS") {
        Ok(raw) => raw
            .split([';', ':'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// pkg-config 回退（Linux 系统安装）：解析 `--cflags` 输出里的 `-I` 路径。
fn pkg_config_include_paths() -> Vec<String> {
    for lib in ["opencv5", "opencv4"] {
        let Ok(output) = std::process::Command::new("pkg-config")
            .arg("--cflags")
            .arg(lib)
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let paths: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|f| f.strip_prefix("-I").map(str::to_string))
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    Vec::new()
}

/// 从 include 目录读 `opencv2/core/version.hpp` 的 `#define CV_VERSION_MAJOR <n>`。
fn read_major_version(include: String) -> Option<u32> {
    let version_hpp = std::path::Path::new(&include).join("opencv2/core/version.hpp");
    let content = std::fs::read_to_string(&version_hpp).ok()?;
    content.lines().find_map(|line| {
        let value = line.trim().strip_prefix("#define CV_VERSION_MAJOR")?;
        value.trim().parse().ok()
    })
}
