// OpenCV 主版本探测：OpenCV 5 给 `warp_perspective` 增加了第 8 参
// `AlgorithmHint`，4.x 没有该参数（且 4.x 默认行为即 accurate，与 5 的
// ALGO_HINT_ACCURATE 等价）。opencv crate 只给自身发 `ocvrs_opencv_branch_5`
// cfg，下游拿不到，这里从头文件重新解析主版本并给本 crate 发同名 cfg，
// 供 src/pipeline.rs 按 cfg 分流。
//
// 版本头路径与 opencv crate 的 environment probe 同源（OPENCV_INCLUDE_PATHS），
// CI（官方 prebuilt 5.0.0）与本机（4.10 prebuilt）都走这个 env。
fn main() {
    println!("cargo::rerun-if-env-changed=OPENCV_INCLUDE_PATHS");
    let Some(major) = detect_major_version() else {
        return;
    };
    println!("cargo::rustc-check-cfg=cfg(ocvrs_opencv_branch_5)");
    if major >= 5 {
        println!("cargo::rustc-cfg=ocvrs_opencv_branch_5");
    }
}

/// 从 OpenCV 版本头读主版本（`#define CV_VERSION_MAJOR <n>`）。
fn detect_major_version() -> Option<u32> {
    let include = std::env::var("OPENCV_INCLUDE_PATHS").ok()?;
    let version_hpp = std::path::Path::new(&include).join("opencv2/core/version.hpp");
    let content = std::fs::read_to_string(&version_hpp).ok()?;
    content.lines().find_map(|line| {
        let value = line.trim().strip_prefix("#define CV_VERSION_MAJOR")?;
        value.trim().parse().ok()
    })
}
