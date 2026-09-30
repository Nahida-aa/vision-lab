//! 几何工具：由顶点算坐标值域 / 几何中心（纯函数，无模型依赖，可单测）。
//!
//! 目前包含：
//! - [`points_range`]：一批二维点的 x/y 值域（不算质心）。
//! - `polygon_metrics`：单框四点的 x/y 值域 + 几何中心。
//!
//! 两函数求值口径必须保持一致（同一套 `glam::Vec2` 按分量 min/max SSE fold），
//! **故紧邻放置、便于对照**；改一处值域算法时另一处要同步。

use glam::Vec2;

/// 对一批二维点算 `x_range` / `y_range`（按分量 `min`/`max`，底层 SSE），**不算质心**。
///
/// 单遍 `fold`（每个识别框都会调用，故避免多次扫描），无 `mut`、无堆分配。供多框聚合
/// （subtitle_ocr 的 `aggregate_boxes`）使用。值域口径对齐 cpp/ts 的 `polygonToXyRange`。
///
/// 注：单框场景（`polygon_metrics`）刻意不复用本函数，而是在自身一遍 `fold` 里顺带
/// 算质心——若复用此处会多扫一遍点。两函数求值口径必须保持一致，故请让它们**紧邻放置**，
/// 便于对照（改一处值域算法时另一处要同步）。
pub fn points_range<I: Iterator<Item = Vec2>>(pts: I) -> ([f32; 2], [f32; 2]) {
    let (min_xy, max_xy) = pts.fold(
        (Vec2::splat(f32::INFINITY), Vec2::splat(f32::NEG_INFINITY)),
        |(min_xy, max_xy), p| (min_xy.min(p), max_xy.max(p)),
    );
    ([min_xy.x, max_xy.x], [min_xy.y, max_xy.y])
}

/// 由四边形顶点算 `x_range` / `y_range` 与几何中心（四点平均）。
///
/// 值域口径对齐 cpp/ts 的 `polygonToXyRange`（`min/max` 各分量），中心用于点击回灌。
/// 单遍 `fold` 同时算 min/max/sum（不复用 `points_range`，否则为质心多扫一遍点），
/// 无堆分配、无 `mut`。
///
/// 借 `glam::Vec2` 的按分量运算把 x/y 两路同构运算合并成宽向量指令，输入变大时也能
/// 真正跑出向量化收益。固定 4 点由 LLVM 完全展开，fold 与手写 `for` 生成等价机器码。
///
/// 求值口径须与 `points_range` 保持一致——两函数紧邻放置便于对照。
pub(crate) fn polygon_metrics(polygon: &[Vec2; 4]) -> ([f32; 2], [f32; 2], [f32; 2]) {
    let (min_xy, max_xy, sum) = polygon.iter().fold(
        (
            Vec2::splat(f32::INFINITY),
            Vec2::splat(f32::NEG_INFINITY),
            Vec2::ZERO,
        ),
        |(min_xy, max_xy, sum), &p| (min_xy.min(p), max_xy.max(p), sum + p),
    );
    (
        [min_xy.x, max_xy.x],
        [min_xy.y, max_xy.y],
        [sum.x / 4.0, sum.y / 4.0],
    )
}
