# 连通域性能对比（待办实验的设计与结论占位）

> **状态：待做（未实测）。** 本文件是 `todo.md` 中「[性能调研] 连通域性能对比」待办的
> 落地位置，也对应 [`../DECISIONS.md §5`](../DECISIONS.md)「连通域决策背景」。
>
> **为什么要做**：当前连通域是自写 8 邻接 BFS，但「它是否比 OpenCV `findContours` /
> C++ `CMyClosedFigure` 快（或慢）」**从无数据**。`DESIGN.md:75` 的 findContours 方案
> 从未实现，`perfbench.rs` 也无连通域专项基准。在补出对比数据前，任何「换连通域算法
> 能显著提速」的说法都是推测，不该推进。
>
> **归属**：性能优化域 `docs/perf/`。实验做完把结论回填本文件，并同步更新
> `../DECISIONS.md §3/§4/§5` 与 `README.md` 维护约定。

---

## 一、问题定义

- 当前实现：`filter.rs:40` `connected_components`，8 邻接、FIFO(VecDeque) BFS，返回
  `Vec<Figure>{ points, min_x/max_x/min_y/max_y }`。
- 语义硬约束（`filter.rs:39`）：与 C++ `SearchClosedFigures(combine_diagonal_points=true)`
  **等价**——8 邻接、需保留每个域的完整 `points` 集合（供 `filter_by_not_intersected_figures`
  / `clear_image_from_small_symbols` 消费），不只是轮廓或外接框。
- 现状里 filter 的真正占比（`state-machine-perf-analysis.md §6.4`）：
  `dilate(NE) ~5.8ms / 45%` ＞ 连通域相关 `~5.3ms / 40%` ＞ 其余。
  **结论：连通域不是 filter 的最大头，别默认「换它就能砍半」。**

## 二、待对比的实现

| 实现 | 来源 | 关注点 |
| --- | --- | --- |
| A. 自写 8 邻接 BFS（现状基线） | `filter.rs:40` | FIFO VecDeque；每像素 `%w`/`/w` 整数除法；每次 `vec![false; w*h]` |
| B. OpenCV `findContours` | `opencv::imgproc`（仓库已依赖，见 `rapidocr_ort/src/det.rs:88` 用法） | 只给轮廓点；`RETR_EXTERNAL`/`CHAIN_APPROX_NONE` 能否还原 8 邻接 `points` 集合？ |
| C. OpenCV `connectedComponents` / `connectedComponentsWithStats` | `opencv::imgproc` | 直接给 label 图 + stats；8 邻接（connectivity=8）；`points` 集合需从 label 反推 |
| D. two-pass + union-find（手写） | 待实现 | 行扫描、只在相邻行判连通；避免 BFS 逐像素 `%/÷` |

> B/C 可先做「等价性探针」：若 `points`/连通关系无法与 A 逐位一致，则该实现直接淘汰，
> 不必测速。`connectedComponents` 的 8 邻接定义要核对是否与 C++ 的
> `combine_diagonal_points=true` 一致（对角线合并规则）。

## 三、实验设计（遵循 `README.md §2/§3` 口径纪律）

1. **隔离基准**：在 `src/bin/perfbench.rs` 加一个连通域子基准（合成 1280×720 二值帧，
   含若干文字块 + 噪声小连通域，模拟 `ImSF`/`ImTF`），`release`、`black_box`、预热、
   多次取 min-of-N。绝不拿整视频墙钟 ÷ 帧数下结论。
2. **等价性校验（先于测速）**：对同一输入，比对各实现的
   - 连通域个数；
   - 每个域的 `points` 集合（排序后逐元素）；
   - 边界框 `min_x/max_x/min_y/max_y`。
   任一不一致即记「语义不等价」，不进入性能排名。
3. **测速**：各实现「每调用耗时」，并换算到「占 filter / 占整帧」的量级
   （对照 §6.4 的 5.3ms / 14ms 基线），判断差异是否值得动算法。
4. **端到端校验（若考虑替换）**：替换后必须过 `timeline.txt` 逐字节一致 +
   A/B 逐阶段指纹（`../cpp-alignment-notes.md §三`）。当前「逐帧对齐 C++」目标下，
   换算法属破坏语义，**须先确认目标能否放宽到「时间轴/文本一致」**。

## 四、结论（待填）

> ⚠️ 下面是占位，实验前**不要填任何数字**，避免又变成「无来源的既有结论」。

| 项 | 结果 |
| --- | --- |
| 各实现是否与 A 逐位等价 | 待测 |
| 各实现每调用耗时 | 待测 |
| 是否存在比 A 更快且等价的实现 | 待测 |
| 是否建议替换 / 是否需放宽对齐目标 | 待测 |
| 实验日期 / 机器 / 视频或合成帧口径 | 待测 |

## 五、已知背景（避免重复考证）

- findContours 从未在 subtitle_finder 落地：骨架 `838815b` 是 `todo!("用 OpenCV findContours")`，
  首次实现 `903e427` 直接写 BFS；`git log -S'findContours' -- '*.rs'` 无本包调用记录。
- 源码里唯一的性能相关注释是 `filter.rs:44-45`：FIFO BFS 按层展开缓存友好，原 LIFO 栈
  在 8 邻接下跳跃、命中率低——这是 **BFS 内部队列**对比，不是与 findContours/C++ 对比。
- `state-machine-perf-analysis.md §6.4` 已结论：`dilate` 的 scatter 实现已接近最优，
  可分离两趟 / gather 均慢 3×，勿再微优化。
- 「分配优化墙钟不可测」（`state-machine-perf-analysis.md §6.1`）：若结论落在「给 BFS
  复用 `visited`/队列缓冲」，预期收益极低，只当代码质量改进。
