# subtitle_finder 性能优化（文档域入口）

> **本文是 `docs/perf/` 的总索引 + 测量方法论。** 动手做性能优化前先读本文，
> 尤其「§2 测量口径纪律」——本包历史上多次因口径错误得出相反结论。
>
> 权威性：源码 `///` > 本文 / `DECISIONS.md` > 本目录下的实测记录 > `DESIGN.md`（已过时）。
> 当前实现状态、已验证/已否决结论、待验证方向见 [`../DECISIONS.md`](../DECISIONS.md)。

## 目录

| 文件 | 定位 |
| --- | --- |
| [`README.md`](README.md)（本文） | 性能域入口：基线快照、测量口径、工具清单、坑、验证流程 |
| [`state-machine-perf-analysis.md`](state-machine-perf-analysis.md) | **全部实测数据与逐次实验记录**（性能基线、分阶段拆解、A/B、微基准、已落地/已放弃方案） |

> `DESIGN.md` §「性能基准与优化」保存了移植初期的性能记录（target-cpu=native、
> 手写 SIMD、与 C++ min-of-N 对比、已知坑），**部分现状已变**（帧内并行默认关闭），
> 读时以 `DECISIONS.md §1/§4` 和源码为准。
> 仓库级共享工具 `tools/perf_compare/`（C++ 逐位一致朴素循环参照 + `bench.sh`）
> 不在本目录，原地保留，见其自带 `README.md`。

---

## 一、当前基线快照（引用前先确认口径）

| 指标 | 值 | 口径 / 出处 |
| --- | --- | --- |
| Rust 算法耗时 | **~14.4 ms/帧** | 单 worker、1017 帧 / 1280×720 / 30fps、本机，`--profile`，2026-11-02（`state-machine-perf-analysis.md` §7.3） |
| 较 2026-08 基线 | 19.9 → 14.4 ms（**提速 ~28%**） | 同上；相对同基线 C++ 20.5ms 快约 30% |
| 每帧变换占比 | `filter(连通域) ~60%` > `im_ff ~24%` > `bgr_to_yuv ~10%` > `color_filtration ~6%` | 隔离 `perfbench`，§6.3 |
| 状态机 compare | ~0.5 ms/帧（2.5%）；差异帧 5 ms 仅在段边界 | §6.2 |
| 并行模型 | 帧间并行（解码线程 + N worker + reorder）；帧内并行**默认关闭** | `state.rs` / `imgops.rs:16-40` |

> ⚠️ 16 workers 的 `--profile` 数值是**各 worker 累计 CPU 时间**，不是墙钟，
> 不可与单 worker 直接比较（§7.3 注）。

---

## 二、测量口径纪律（最容易踩的坑）

1. **绝不用「墙钟 ÷ 总帧数」当算法耗时。**
   墙钟大头是**解码**，且 C++/Rust 两侧口径不同（C++ 墙钟含自身打开/解码开销）。
   `state-machine-perf-analysis.md` §7.2 曾用此口径得出「Rust 比 C++ 慢」，**已作废**；
   正确口径见 §7.4。
2. **整视频 A/B 在本机高负载下不可靠。**
   §6.1：分配优化 A/B 得 34.3s vs 34.5s（0.6%，在噪声内），据此下结论会误判。
   判断「某优化是否真有用」用 **§3 的单函数微基准**（隔离、负载稳健）。
3. **微基准波动可达 ~50%**（系统负载/频率）。单次测量会误导，用
   `tools/perf_compare/bench.sh` 多次采样取 **min-of-N**（`DESIGN.md` §与 C++ 对比）。
4. **C++ 基准必须用 `volatile g_sink` 累加结果**，否则 `g++ -O3` 会把无副作用的循环
   死代码消除（`tools/perf_compare/perf_compare.cpp` 已处理）。
5. **AVX2 在 debug（`-O0`）比标量慢**，必须 release 测。
6. 对外说「快 C++ 30-40%」必须带口径（单 worker / 1017 帧 / 本机 / `--profile`），
   不能只报百分比。

---

## 三、工具清单（按用途选，别用错）

| 工具 | 用途 | 跑法 | 适用 |
| --- | --- | --- | --- |
| `--profile` | 真实视频分阶段累计耗时（color_filtration / yuv / im_ff / filter / 状态机） | `just profile <video>` 或 `cargo run -p subtitle_finder --release -- --profile <video>` | 定位真实视频里哪阶段占大头 |
| `perfbench` | 单函数隔离微基准（合成 1280×720 字幕帧，多次取均值） | `cargo run -p subtitle_finder --release --bin perfbench` | **判定某个优化是否真有收益**（决定性） |
| `tools/perf_compare/bench.sh` | C++ vs Rust 算子逐位对比，min-of-N | 见其 `README.md` | 算子级（Sobel / 卷积 / ECP）跨语言对比 |
| `just run-nosave` | 纯算法计时（`--output timeline`，不落盘图片） | `just run-nosave <video>` | 排除落盘 I/O 干扰 |
| `ab_dump` / `ab_compare.py` | 逐阶段 A/B 指纹对比（**正确性**，非性能） | 见 `../cpp-alignment-notes.md` §三 | 任何改动后的输出一致性校验 |

> `perfbench` 与 `--profile` 互补：前者隔离单函数、回答「这个改动本身快了多少」；
> 后者回答「真实视频里这阶段占多少」。**只有后者显示占比大、前者也确认收益，才值得做。**

---

## 四、已确认的结论（避免重复实验）

**热点**：`filter` 是绝对热点（占变换 ~60%），其次是 `im_ff`（§6.3、§7）。
但 ⚠️ **别把 filter 整体等同于「连通域 BFS」**：`state-machine-perf-analysis.md §6.4`
的 filter 内部拆解显示最大头是 **`dilate(NE) ~5.8ms / 45%`**，连通域相关的
`filter_by_not_intersected_figures 2.9ms + clear_small_symbols 2.4ms ≈ 5.3ms / 40%`，
且 §6.4 结论「dilate 已接近最优，勿再微优化」。优化 filter 仍是降每帧耗时的主方向，
但要先看清子函数占比，别默认「换连通域算法就能砍半」。
连通域为何自写 BFS、是否对比过 findContours → [`../DECISIONS.md §5`](../DECISIONS.md)（结论：**无性能对比数据**）。

**已落地且验证过的优化**（输出 `timeline.txt` 逐字节一致，§8.3）：诊断统计改
`tracing::enabled!(DEBUG)` 守卫、`intersect_y_images_masked`、`y_in_range` 判定核、
交集按位与 + 帧展开、`AnalyseImage` 行扫描、检测阶段去 clone。

**已否决 / 放弃**（详见 `../DECISIONS.md §4`，此处只列与性能直接相关的）：
- 状态机分配 churn / 去 clone / 复用缓冲：**墙钟不可测，不必做**（§6.1、§7）。
- `compare_two_subs` 多趟扫描融合成单趟：**无稳定收益甚至更慢**，已回退（§8.3，
  ⚠️ 原始计时日志已丢失，引用前须重跑）。
- 帧内 3/5 线程并行 GetImFF/NE/HE 作为默认：抢内存带宽，**默认关闭**（`SF_PARALLEL`）。
- `u16 y 图只在 mask 内计算`：改变语义（背景像素参与跨帧 ILA），段起点漂移，已弃。

> ⚠️ **给「复用大 buffer / 减少分配」类优化降预期**：§6.1 已实测证明分配优化对墙钟
> 影响在噪声内。这类改动只当代码质量改进，别当提速手段。

**待验证方向的可行性**（完整清单见 [`../DECISIONS.md §5`](../DECISIONS.md)）：
- 实现层「不改输出」的（诊断统计守卫、`sm_begin` 剖析开关）**可行、低风险**，但按 §6.1
  收益量级小，先跑 `perfbench` 再决定值不值得做。
- 算法层（换连通域算法、接 `roi_y` 做 ROI 预筛）虽瞄准最大热点 `filter`，但会动
  `has_text`/段边界，**在当前「逐帧对齐 C++」目标下不可行**；须先确认目标能否放宽，
  并重建 A/B 基线（`../cpp-alignment-notes.md §三`）。

---

## 五、平台层已知坑（`DESIGN.md` §已知坑，仍有效）

- `-C target-cpu=native` 是性价比最高的一步（55→36 ms/帧，对所有标量循环自动向量化），
  但产物**不跨机器可移植**；需可移植改用 `-C target-feature=+avx2`。
- **Rust `#[target_feature]` 函数跨 crate 不内联**（issue #145574）：`sobel_n/h` 稳定慢
  ~1.9× 于 C++ 是平台限制，不是算法差距。同 crate 内联的 AVX-512 可达 C++ 水平。
- 开 `native` 后，手写 AVX2 对简单核收益很小或略负；仅 `sobel_m` 16×i16 版仍 ~2× 优于标量。
- u16 版 `load8` 须用 `_mm_loadu_si128`(16B=8 u16)；AVX2 无整数除法，累加值提取后标量 `/div`。

---

## 六、改动性能相关代码的验证流程

1. **先立基线**：当前 `--profile` 分阶段耗时 + `perfbench` 目标函数数值（同机器、同负载、
   多次取 min）。
2. **实现层改动**（声称语义不变）→ 必须过正确性校验：`timeline.txt` 逐字节相同 /
   `ab_compare.py` 逐阶段指纹一致（§8.3 的 6 项都做了逐字节校验）。
3. **算法层改动**（会动 `has_text` / 段边界）→ 先确认目标能否从「逐帧对齐 C++」放宽到
   「时间轴/文本结果一致」；用 A/B 框架喂同一帧 BGR 逐阶段对比。
4. **再测收益**：`perfbench`（决定性） + `--profile`（真实占比）。整视频 A/B 只作参考。
5. **记录**：把结论（做了/否决、日期、口径、验证方式）写进
   `state-machine-perf-analysis.md` 新章节，并更新 `../DECISIONS.md §3/§4/§5`。

---

## 七、维护约定

- 新的性能实验记录 → 追加到 `state-machine-perf-analysis.md`（按日期开新章节，保留历史，
  包括被推翻的结论，像 §7.2 那样显式标「已作废」）。
- 当前基线 / 已验证 / 已否决 / 待验证的**状态** → 维护 `../DECISIONS.md`，本文只放
  「最新基线快照 + 口径 + 工具 + 坑 + 流程」。
- 本文 §1 基线与源码/实测不一致时，**先改本文 §1**，再考虑给 `DESIGN.md` 打过时标注。
