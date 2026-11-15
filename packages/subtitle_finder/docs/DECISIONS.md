# subtitle_finder 决策台账（当前状态 / 已验证 / 已否决 / 待验证）

> **本文用途**：收口「哪些是历史、哪些是当前、哪些已试过被否、哪些还没验证」，
> 供后续会话（人或 AI）在动手优化/对齐前先读，避免把过时的 `DESIGN.md` 当现状、
> 或重复踩已否决的坑。
>
> **权威性顺序（冲突时以上为准）**：
> 1. **源码 `///` 注释 + 代码本身**（`AGENTS.md` 约定，时效性最强）
> 2. `todo.md`（正确性对齐进度）、本文（决策状态）
> 3. [`docs/perf/`](perf/README.md)（性能优化域：测量口径 / 工具 / 基线 / 实测，
>    实测细目在 [`perf/state-machine-perf-analysis.md`](perf/state-machine-perf-analysis.md)）
> 4. `DESIGN.md`（移植初期设计稿，**多处已过时**，见 §6）
> 5. `README.md`（架构图，基于源码生成）
>
> 改动任何算法前先跑 §7 的验证方法；改实现层（不改输出）也要过一致性校验。

---

## 一、当前实现状态快照（以源码为准）

| 主题 | 当前事实 | 出处 |
| --- | --- | --- |
| 视频解码 | `opencv::videoio::VideoCapture`，逐帧 bt709 BGR | `frame.rs:3-9,19`、`Cargo.toml:29-31` |
| BGR→YUV | `cv::cvtColor(COLOR_BGR2YUV)` | `imgops.rs:846` |
| 形态学膨胀 | `cv::dilate`（3×3，iters=6） | `imgops.rs:212-240` |
| 连通域 | 自写 8 邻接 BFS（**非** OpenCV findContours） | `filter.rs:40` |
| 文本对齐 | `Center`（非 Any） | `filter.rs:11-14`、`todo.md` |
| 并行模型 | **帧间并行**：解码线程 + N worker + reorder + `FrameCache` 滑动窗口 | `state.rs::TransformStream` / `FrameCache` |
| 帧内并行 | **默认关闭**（`SF_PARALLEL=1` / `SF_THR=1` 才开） | `imgops.rs:16-40,766` |
| worker 数 | 默认 `min(4, cpu/3)`，`SF_NWORKERS` 覆盖 | `state.rs` |
| 段尾 PTS | **真实末帧 PTS** `frame(fn+offset-1).pos`（不跟随 C++ off-by-one） | `todo.md:134`、`docs/perf/state-machine-perf-analysis.md:430-435` |
| 对外 API | `find_keyframes(video, &Params) -> Vec<Keyframe>` | `lib.rs:42` |
| `Keyframe` | `start_ms / end_ms / frame: Array3<u8> / mask: Array2<u8>` | `lib.rs:23-33` |
| 落盘 | `output.rs::write_artifacts`：`frames/` `mask/` `timeline.txt` `keyframes.json` | `output.rs` |
| 文件名 | `{start_ms}_{end_ms}.png`（喂下游 OCR 的 `ms_ms` 约定） | `output.rs:59-68` |
| 参数全集 | 20+ 字段，**DESIGN 表只列了 8 个** | `params.rs` |
| `roi_y` | `params.rs:23` 有定义 + `Default::None`，**无任何消费方**（死参数，未接线） | `params.rs` |

---

## 二、已验证的性能结论（别再重新论证）

来自 `docs/perf/state-machine-perf-analysis.md`，按时间：

- **基线**：Rust 单 worker 算法耗时 **~14.4 ms/帧**（1017 帧 / 1280×720 / 30fps，本机，
  2026-11-02 `--profile` 口径，§7.3）。较 2026-08 基线 19.9ms 提速 ~28%。
- **口径纪律**：墙钟 ÷ 总帧数**错误**（把解码开销算进算法耗时，§7.2 / §7.4 作废结论）。
  引用「快 C++ 30-40%」必须带口径（单 worker / 1017 帧 / 本机）。
  16 workers 的 `--profile` 数值是各 worker **累计 CPU 时间**，非墙钟，不可直接比。
- **每帧变换分阶段**（§6.3，隔离 perfbench）：`filter(连通域) ~14ms / 60%` >
  `im_ff ~5.6ms / 24%` > `bgr_to_yuv ~2.4ms` > `color_filtration ~1.4ms`。
  **filter 是绝对热点**，优化它才显著降每帧耗时。
- **状态机 compare 每帧 ~0.5ms（2.5%）**；差异帧 5ms 只在段边界（§6.2）。
  状态机是顺序依赖，多线程收益有限（C++ 12 线程仅 1.12×，§7）。
- **分配 churn / 状态机去拷贝不是墙钟瓶颈**：`get_intersect_images` 引用化实测
  34.3s→34.5s（0.6%，在噪声内，§6.1）。§7 结论明确「第 2/3/4 项优化**不必做**」。

---

## 三、已落地且验证过的优化（别重复做）

`docs/perf/state-machine-perf-analysis.md` §8.3（2026-11-03，输出 `timeline.txt` 逐字节一致）：

1. `compare.rs` val3 诊断全帧白点统计改 `tracing::enabled!(DEBUG)` 守卫（原无条件扫
   4–6 遍 0.92M 像素，实测占主线程 ~3ms/帧）。
2. `IntersectYImages` 改 `intersect_y_images_masked`（mask 外清零不改输出，已对拍）。
3. 抽出 `y_in_range` 无分支判定核，单图/多图/掩码三处共用，保证逐位一致。
4. `intersect_images_range` 二值交集改按位与 + 4/5 帧展开（LLVM 可向量化）。
5. `AnalyseImage` 改行优先连续扫描 + 只为密度最大带建段表（0.47→0.41ms/帧）。
6. 检测阶段 `f1/f2` 只借用不 clone（每帧省 ~6.5MB）。

`DESIGN.md` 另记录已做：`-C target-cpu=native`（55→36ms/帧）、`geometry::imgproc`
AVX2 手写算子、OpenCV 解码/dilate/cvtColor 对齐。

---

## 四、已否决 / 放弃的方案（重点：防重复踩坑）

| 方案 | 结论 | 出处 |
| --- | --- | --- |
| 状态机分配 churn（去 clone / 复用缓冲 / compare 去拷贝） | **墙钟不可测，不必做**（§6.1、§7） | `docs/perf/state-machine-perf-analysis.md` |
| u16 `y` 图「只在 mask 内计算」 | **改变语义**：`y` 是「亮度+255」非前景掩码，背景像素参与跨帧 ILA 比较；实测第 3 段起点 1233→1266 | §8.3 放弃方案 |
| `compare_two_subs` 单趟融合（多趟全帧扫描合并） | **无稳定收益甚至更慢**（1017ms→1592ms，换数据可反转），已回退多趟；⚠️ 原始计时日志已丢失，引用前须重跑 | §8.3 放弃方案 |
| 帧内 3/5 线程并行 GetImFF/NE/HE 作为默认 | 实测抢内存带宽、收益为负/甚微，**默认关闭**（`SF_PARALLEL`） | `imgops.rs:16-40`、`DESIGN.md` §并行化 顶部标注 |
| `ffmpeg-next` 解码 | 已弃用：bt601 vs bt709 → 幽灵带 → 段丢失 | `frame.rs:6-9`、`todo.md 08b5307` |
| 跟随 C++ `PosForward[offset]-1` 段尾 | 已弃用：那是 C++ off-by-one；Rust 用真实末帧 PTS | `todo.md:134`、§8.4 |
| `g_text_alignment=Any` / sobel `up_l=7` / Any-skip 清理 | 基于错误前提（实际是 Center / 系数 10），**无效或回归** | `todo.md:143-144` |
| 手写 scatter 膨胀（替代 cv::dilate） | iters=6 时 ~19ms/帧，OpenCV 分离核快一个量级 | `imgops.rs:212-217` |

---

## 五、待验证方向（尚未实测，动手前先读本节）

> 这些都**只是推测**，落地前必须用 §7 方法证伪/证实；改动会破坏「逐帧对齐 C++」的
> 属算法层，需先确认目标是否可从「逐帧一致」放宽到「时间轴/文本结果一致」。

- **算法层**：
  - `filter` 连通域换 two-pass + union-find / 限定到 LB/LE 文字带区域，替代全图 8 邻接
    BFS（当前最大热点）。需保证 `Figure` 边界框逐位一致。
  - 把 `params.rs:23` 已存在但未接线的 `roi_y` 接线，做廉价字幕 ROI 预筛，跳过无字幕帧
    的完整 transform（当前仅 `color_filtration` 的 `n==0` 早退，`imgops.rs:742`）。
  - 状态机减少段边界处 `compare_two_subs_optimal` 的重复调用。
- **实现层**（不改输出，优先级低于已否决项的警示）：
  - `filter.rs:637-678`、`imgops.rs:797-798`、`state.rs:651-652` 仍有**无条件 `let`**
    的全帧/带区白点统计（结果只喂 `trace!`/`debug!`）。`compare.rs` 已用 `diag` 守卫修过
    同类问题，这几处是**遗漏/回归**，可照 `compare.rs:280-284` 范式加守卫（零算法风险）。
  - `imgops.rs:88` `sm_begin()` 的 `Instant::now()` + `thread_local` Drop 在热路径无条件
    执行，profiling 默认关闭时数据没人读；可用 static 开关完全消除（纯剖析税）。
  - `find_and_apply_local_thresholding` 每次 `vec![0i32; 45056]`（180KB）、`get_im_ff`
    Thr1/Thr2 各一次 = 每帧 ~360KB calloc；`cmoe`/`visited` 等大 buffer 可按 worker
    thread_local 复用。**注意**：§6.1 已证明「分配优化墙钟不可测」，此项预期收益很低，
    仅作为代码质量改进，别当提速手段。
  - `get_transformed_image` 返回 9 元素 tuple，worker 丢弃了 `im_ff/im_sf/lb/le/n`
    却仍完整计算；`im_y` u8 又被全帧转 u16（`state.rs:314`）。可只产出最终需要的
    FrameData 字段。

---

## 六、过时文档索引（读这些时先回到本文）

| 文档 | 过时点 | 正确处理 |
| --- | --- | --- |
| `DESIGN.md` §视频解码方案 | 整节写 `ffmpeg-next` /「待你决策」/「已采用方案 1」 | 已加「已定案 + 结论被推翻」标注，历史折进 `<details>` |
| `DESIGN.md` §输入/输出契约 | 文件名 `{h}_{m}_{s}_{ms}__...`、`Keyframe` 少 `mask`、`main.rs` 落盘、只有帧图+JSON | 已加「实际实现（以源码为准）」块 |
| `DESIGN.md` §模块划分 / §复用策略 | `frame.rs` 写 ffmpeg-next；漏 `output.rs`；findContours 前后矛盾 | 已就地订正 + 标注 |
| `DESIGN.md` §关键参数表 | 只列 8 个参数 | 已标「不完整」，指向 `params.rs` |
| `DESIGN.md` §并行化 GetImFF | 把「曾经试过帧内并行」写成默认做法 | 已标「默认关闭，主力是帧间并行」 |
| `DESIGN.md` §段尾 PTS 决策 | 停在「倾向回退 / 待用户定夺」 | 已标「已定案 = 真实末帧 PTS」 |
| `DESIGN.md` §已知局限 | 「has_text 已修」易被误读为「功能全对齐」 | 需配合 `todo.md` 看后续新待办（56600 末尾段、39466 误段、「走吧」缺失等） |
| `docs/project_sf_39466_fix.md` | 空占位文件（0 字节） | 无内容，忽略 |

---

## 七、验证方法（改动前后必做）

来自 `docs/cpp-alignment-notes.md` §三、`docs/perf/state-machine-perf-analysis.md` §6.2/§8.4：

1. **正确性（首要目标：逐帧对齐 C++）**
   - A/B 框架：`src/bin/ab_dump.rs` + `ab_dump.cpp` + `ab_compare.py`，**喂同一帧 BGR**
     给两边算法逐阶段对比（不要用 C++ CLI 分支的 `FastSearchSubtitles` dump 对比，不可靠）。
   - 输出校验：`timeline.txt` 逐字节相同；段数相同、段首差 ≤1ms、段尾差 ≤1 帧
     （段尾那 1 帧是 C++ off-by-one，不修，§8.4）。
   - 用 `subtitle_ocr` 验证具体帧有无字幕。
2. **性能**
   - 单函数微基准：`cargo run -p subtitle_finder --release --bin perfbench`（隔离、负载稳健）。
   - 整视频 A/B 受负载干扰不可靠（§6.1），别拿它下结论；用 `--profile` 分阶段耗时 +
     单函数基准。
   - 微基准波动可达 ~50%，`tools/perf_compare/bench.sh` 多次采样取 min-of-N。
3. **实现层改动**：即使声称「语义不变」，也要过第 1 步的 `timeline.txt` 对拍
   （§8.3 全部 6 项都做了逐字节校验）。

---

## 八、维护约定

- 新做一次优化 / 否决一个方案 → 更新 §3 或 §4，并注明日期、口径、验证方式。
- 发现源码与本文 / `DESIGN.md` 不一致 → **先改本文的 §1 状态快照**，再考虑给
  `DESIGN.md` 打「已过时」标注（保留历史，不删）。
- `DESIGN.md` 不再追加新决策，只作为历史设计稿保留；新决策写本文。
