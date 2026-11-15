# subtitle_finder

> ⚠️ **本文是移植初期的设计稿，部分内容已过时（尤其「视频解码方案」「输入/输出契约」
> 「段尾 PTS 决策」「帧内并行」）。** 按 `AGENTS.md` 约定，文档与代码不一致时**以源码
> `///` 注释为准**。
> - 当前实现状态、已验证/已否决的优化结论、待验证方向 → [`docs/DECISIONS.md`](docs/DECISIONS.md)
> - 性能优化域（基线/口径/工具/实测） → [`docs/perf/`](docs/perf/README.md)
> - 正确性对齐待办 → [`todo.md`](todo.md)
>
> 下文保留历史决策记录（含被推翻的结论），不删除，以便追溯「为什么这么设计」。

吃视频 → 输出字幕关键帧图 + 时间轴。**复刻 VideoSubFinder 的「筛选」管线**（传统 CV，
不做 OCR），把 `rapidocr_ort` 作为下游识别器，对应官方
`VideoSubFinder → RapidVideOCR` 的分工。

**本包只负责"找到字幕变化的关键帧 + 时间轴"，不做识别。**

## 为什么（动机）

- 现有 `subtitle_ocr` 是**纯单帧 OCR**，抽帧在 bench（测试驱动）。
- bench 抽帧用 fps=2（341 帧），有 500ms 采样偏移。
- VideoSubFinder 逐帧（30fps）处理 + 传统 CV 筛选，只在字幕变化时输出关键帧——
  时间无偏移、且大幅减少要 OCR 的帧数。
- 完美复刻它的管线，需要一个吃视频、输出关键帧图的包，与识别解耦。

## 输入 / 输出契约

> ⚠️ **已过时**：文件名格式、`Keyframe` 字段、落盘位置都已变。以 `lib.rs` / `output.rs`
> 源码为准，见下方「实际实现」块。

```
输入：视频文件（mp4 等，ffmpeg 可解码）
输出：
  - 关键帧图目录（每张 = 一个字幕段的一张代表帧）
  - 每张文件名带 起止时间（对齐 VideoSubFinder：`{h}_{m}_{s}_{ms}__{h}_{m}_{s}_{ms}`）
  - 或结构化 JSON：{ 关键帧路径, start_ms, end_ms, text_img }
```

> ~~**当前实现**：`find_keyframes` 返回 `Vec<Keyframe>{ start_ms, end_ms, frame: Array3<u8> }`。~~
> ~~CLI（`main.rs`）把代表帧存为 `{start_ms}_{end_ms}.png`…JSON 输出（keyframes.json）已实现。~~

> **实际实现（以源码为准）**：
> - `find_keyframes(video, &Params) -> Vec<Keyframe>`（`lib.rs:42`），`Keyframe` 含
>   `start_ms / end_ms / frame: Array3<u8> / mask: Array2<u8>`（`lib.rs:23-33`）。
> - 落盘统一在 `output.rs::write_artifacts`，产物为 `frames/`（原始帧）、`mask/`（去背景
>   掩码）、`timeline.txt`、`keyframes.json` 四类；文件名 `{start_ms}_{end_ms}.png`
>   （`output.rs:59-68`），即 subtitle_ocr `--dir` 的 `ms_ms` 约定，可直接喂下游 OCR。
> - `main.rs` 只做参数解析 / 路径解析 / 进度条（`main.rs:13-14`），不负责落盘。

下游：`rapidocr_ort` 对关键帧图 rec，得到字幕文本（对应 RapidVideOCR）。

> **对齐 C++ 的经验与结果**（段边界对齐的根因、验证方法、坑）见
> `docs/cpp-alignment-notes.md`。

## 核心算法（复刻自 VideoSubFinder `SSAlgorithms.cpp`）

### 1. 逐帧解码（30fps）
VideoSubFinder 用 ffmpeg/OpenCV 逐帧 `OneStep()` 读入内存处理，不落盘。
**复刻：逐帧解码，每帧作为 `Array3<u8>`（BGR）喂给后续。**

### 2. DL=6 滑动窗口 + 相邻帧交集（去噪）
`FastSearchSubtitles` 用 `ddl = DL/2 = 3` 步进，预取 DL 帧进缓冲区。
对相邻两组帧（`fn+ddl-1` 与 `fn+2ddl-1`）做 `IntersectTwoImages`（逐像素交集），
去掉单帧闪烁/噪声，只留稳定字幕像素。
**复刻：滑动窗口 + 逐像素交集（可用 `geometry::imgproc` 的 SIMD 加速）。**

### 3. AnalyseImage（水平投影检测文字）
按 `g_segh=3` 高的水平条带，统计每列白色像素密度，用 `g_tp=0.3`（文字占比）、
`g_mtpl=0.022`（最小文字长度）判断该帧是否有文字行。
**复刻：水平条带投影 + 阈值判定。**

### 4. FilterTransformedImage（判断"有字幕"）
`AnalizeImageForSubPresence` → `FilterTransformedImage`：找连通域图元（文字块），
按尺寸/密度过滤，判断是否有字幕。
**策略：用 OpenCV `findContours`（仓库已依赖）替代手写连通域（CMyClosedFigure），
保留过滤规则（尺寸/密度阈值）。**

### 5. CompareTwoSubs / CompareTwoSubsOptimal（跨帧比较，判断字幕是否变化）
对两帧（与各自 ILA 交集后）逐像素比较差异比例，用 `g_veple=0.30`/`g_ilaple=0.30`
阈值判断字幕内容是否变化。
**复刻：逐像素差异比例 + 阈值。**

### 6. FastSearchSubtitles 状态机（时间轴核心）
用 `bf/ef`（起止帧）、`bt/et`（起止时间）、`DL`、`g_max_dl_down=20`/`g_max_dl_up=40`
跟踪字幕段：字幕出现（bf）、持续、变化（新字幕）、消失（ef）。只有**字幕内容变化**
才输出关键帧。
**复刻：状态机，这是"时间无偏移"的关键，必须精确对齐。**

## 关键参数（默认值，需对齐 VideoSubFinder）

> ⚠️ **不完整**：下表只列了 8 个状态机主参数，实际 `params.rs` 有 20+ 字段
> （`roi_y / scd / segw / msegc / min_h / mthr / mnthr / btd / to / mpn / mpd / mpned / msh` 等），
> 其中 `mpn=50`/`mnthr=0.3` 等是 filter/compare 的关键阈值。完整清单与默认值以
> `params.rs` 为准。另：`params.rs:23` 的 `roi_y` 目前**只有定义、无消费方**（死参数，未接线）。

| 参数 | 值 | 含义 |
| --- | --- | --- |
| `DL` | 6 | 字幕帧长度（滑动窗口） |
| `segh` | 3 | 水平条带高度 |
| `tp` | 0.3 | 文字占比阈值 |
| `mtpl` | 0.022 | 最小文字长度（百分比） |
| `veple` | 0.30 | 跨帧文字差异阈值 |
| `ilaple` | 0.30 | ILA 差异阈值 |
| `max_dl_down` | 20 | 字幕最短持续（帧数） |
| `max_dl_up` | 40 | 字幕最长持续（帧数） |

## 模块划分（Rust）

> ⚠️ **已过时**：`frame.rs` 解码方式变了；漏了 `output.rs`。以 `src/` 源码为准。

```
packages/subtitle_finder/
  src/
    lib.rs          # 对外 API：find_keyframes(video, &Params) -> Vec<Keyframe>
    frame.rs        # 逐帧解码（opencv::videoio::VideoCapture，非 ffmpeg-next）
    imgops.rs       # 基础像素算子（交集/色差/颜色滤波/BGR→YUV/阈值化/卷积等）
    preprocess.rs   # AnalyseImage 投影 / intersect
    filter.rs       # FilterTransformedImage（自写 BFS 连通域替代）
    compare.rs      # CompareTwoSubs / CompareTwoSubsOptimal（跨帧比较）
    state.rs        # FastSearchSubtitles 状态机（时间轴）+ FrameCache/TransformStream
    params.rs       # 全局参数（对齐上表，实际字段更多）
    output.rs       # write_artifacts：帧图/掩码/timeline/json 落盘
    main.rs         # CLI（可选 --profile 剖析）
```

## 复用策略

> ⚠️ **已过时 + 前后矛盾**：视频解码/图像读取实际是 `opencv`；连通域本文 §4 写
> 「用 OpenCV findContours 替代」、本表写「自写 BFS」——**实际是自写 8 邻接 BFS**
> （`filter.rs:40`），两条 OpenCV 方案都未采用。以 `Cargo.toml` / `filter.rs` 为准。

| 算子 | 来源 |
| --- | --- |
| 逐像素交集 / 投影 / Sobel / 卷积 / 阈值 | `geometry::imgproc`（SIMD，文件夹模块） |
| 连通域（文字块） | 自写 BFS 8 邻接（`filter.rs`，替代 CMyClosedFigure） |
| 图像读取 | 逐帧解码直接产出 BGR（`opencv::videoio::VideoCapture`） |
| 视频解码 | `opencv`（bt709 BGR，`frame.rs`；此前 ffmpeg-next 因 bt601 已弃用） |
| 形态学膨胀 / BGR→YUV | `opencv::imgproc::dilate` / `cvtColor`（对齐 C++） |
| 识别 | `rapidocr_ort`（下游，不在本包） |

## 视频解码方案（~~待你决策~~ → 已定案，结论已被推翻）

> ✅ **最终结论（以源码为准）**：改用 **`opencv::videoio::VideoCapture`** 逐帧解码，
> **不是** `ffmpeg-next`。根因：`ffmpeg-next` 的 swscale 默认输出 **bt601** BGR，而
> C++/OpenCV 输出 **bt709**（差 ±1-8），输入色彩差异让 `get_im_ff` 阈值边缘像素判定
> 不同 → 幽灵带 → 字幕段丢失/过度切分。见 `frame.rs:3-9`、`Cargo.toml:29-31`、
> `todo.md` 中 `08b5307` / `4443773`。
> 配合 `imgops::bgr_to_yuv` 用 `cv::cvtColor`，彻底对齐 C++ 输入。
> 本节的「两方案对比」与「建议方案 1」是移植初期的历史记录，**不再适用**。

<details>
<summary>历史记录（已废弃，保留供追溯）</summary>

仓库没有 Rust 视频解码库（bench 用外部 ffmpeg 命令抽帧）。逐帧 30fps 解码两个选择：

1. **`ffmpeg-next` crate**：真正逐帧解码，性能好，但引入 FFmpeg 系统依赖 + 绑定。
2. **外部 ffmpeg 命令**：复用 bench 的抽帧方式，但只能按 fps 抽帧、有进程启动开销，
   且难做到"逐帧"。

建议方案 1（`ffmpeg-next`），才能做到 30fps 逐帧复刻 VideoSubFinder 的时间行为。
若你不想引 FFmpeg 依赖，需接受抽帧粒度与 VideoSubFinder 不完全一致（时间偏移回来）。

> 已采用方案 1：`frame.rs` 用 `ffmpeg-next` 逐帧解码（回调模式），配合 `state.rs` 的
> `FrameCache` 顺序缓存每帧转换产物。

</details>

## 性能基准与优化（重要）

### 参照系：不是对齐 OpenCV，是对齐 C++ 编译方式

本包的目标是复刻 VideoSubFinder（它内部用 OpenCV）。**性能参照是「C++ 编译器如何编译
同样的算子」，不是 OpenCV 的函数调用**——因为 VideoSubFinder 的核心算子（`ImprovedSobelMEdge`
等）是自定义变体，OpenCV 没有直接对应函数。C++ 参照用 `g++ -O3 -march=native` 编译
逐位一致的朴素循环（见 `tools/perf_compare/`）。

### 最重要的优化：`.cargo/config.toml` 开 `-C target-cpu=native`

**这是性价比最高的一步。** 它让 LLVM 对**所有标量循环**自动向量化（等价 g++ 对朴素
循环的自动向量化）。实测 subtitle_finder 全流程 **~55ms/帧 → ~36ms/帧**（~34%），
连没专门手写 SIMD 的算子（bgr2yuv 274→101ms、连通域 filter 1415→1176ms）都大幅改善。

**注意**：`-C target-cpu=native` 产物**不跨机器可移植**（换机器需重编）。若需可移植，
改 `.cargo/config.toml` 为 `-C target-feature=+avx2`（AVX2 是现代 x86-64 常见）。

### 手写 SIMD 算子（在 `geometry::imgproc`，文件夹模块）

| 算子 | 位置 | 说明 |
| --- | --- | --- |
| `sobel_m/n/h_edge` | `geometry::imgproc/sobel.rs` | 自定义 Sobel 变体，AVX2 |
| `aply_ess` / `aply_ecp` | `geometry::imgproc/conv.rs` | 5×5 卷积 |
| `apply_moderate_threshold` / `zero_below_threshold` | `conv.rs` | 阈值化 |
| `resize_bilinear_hwc` / `normalize_chw` | `resize.rs` / `normalize.rs` | 供 rapidocr |

**经验**：开 `native` 后，手写 AVX2 对简单核（sobel_n/h、aply_ess）收益很小或略负
（LLVM 自动向量化已足够）。只 `sobel_m` 的 16×i16 版（权重用 shift+add 替代 `mullo_epi32`）
仍 ~2× 优于标量，值得保留。

### 与 C++ 对比（720p，release，`tools/perf_compare/bench.sh` min-of-N）

> ⚠️ 单次微基准波动可达 ~50%（系统负载/频率），单次测量会误导。用 `bench.sh`（多次采样
> 取最小值）得到可信对比。

| 算子 | C++ -O3 native | Rust（取较快） | Rust/C++ |
| --- | --- | --- | --- |
| sobel_m | 0.246 ms | 0.21 ms | **0.85×（Rust 快）** |
| sobel_n | 0.101 ms | 0.19 ms | ~1.9× 慢（真实差距） |
| sobel_h | 0.103 ms | 0.20 ms | ~1.9× 慢（真实差距） |
| aply_ess | 0.684 ms | 0.99 ms | ~1.45× 慢 |
| aply_ecp | 4.441 ms | 3.39 ms | **0.76×（Rust 快）** |

**结论（min-of-N）**：sobel_m / aply_ecp Rust 其实**快于** C++（之前单次测量的"慢"是方差
假象）；sobel_n/h 稳定慢 ~1.9× —— 这是 **Rust `#[target_feature]` 跨 crate 不内联**的平台
限制，不是算法差距（见下方"已知坑"）。同 crate 内联的 3-load AVX-512 可达 C++ 水平。

### 已知坑

- **C++ 基准必须用 `volatile g_sink` 累加结果**，否则 g++ `-O3` 死代码消除无副作用的循环
  （`tools/perf_compare/perf_compare.cpp` 已处理）。
- **AVX2 在 debug（`-O0`）构建下比标量慢**（load/widen 开销放大），必须用 release 测速。
- u16 版 `load8` 须用 `_mm_loadu_si128`(16B=8 u16)，`_mm_loadl_epi64`(8B) 只加载 4 个 u16。
- AVX2 无整数除法：8/16 个 i32/i16 累加值提取后标量 `/div`。
- **`#[target_feature]` 函数跨 crate 不内联**（Rust issue #145574：`#[inline(always)]` 与
  `#[target_feature]` 冲突，nightly 同禁）。运行时 `is_x86_feature_detected` 分派让 LLVM 无法
  静态确定调用方 feature，故跨 crate 即使开 thin LTO 也不内联。sobel_n/h 的 3-load AVX-512
  同 crate 达 C++ 水平（~0.11ms），但 subtitle_finder 跨 crate 调用仍 ~1.8× 慢。这是平台
  约束，非算法问题。若想彻底解决：把 Sobel 内联进调用方，或等 Rust 支持跨 crate 内联。

### 并行化 GetImFF / GetImNE / GetImHE（最大的一步）

> ⚠️ **现状已变**：帧内 3 线程并行**默认关闭**（`imgops.rs` `SF_PARALLEL` / `SF_THR`
> 环境变量才开，源码注释说明帧内再切线程收益为负/抢内存带宽）；当前主力是
> **帧间并行**（`state.rs::TransformStream`：解码线程 + N worker + reorder）。
> 本节记录的是「曾经试过」的历史结果，**不等于当前默认配置**。
> 当前基线与分阶段耗时见 `docs/perf/state-machine-perf-analysis.md` §7.3（~14.4ms/帧，
> 单 worker，1017 帧）。

C++ 的 `GetTransformedImage` 里这 3 个算子用 `run_in_parallel`，我们之前**顺序执行**。
改成 `std::thread::scope` 3 线程并行（共享只读 Y/U/V）后：
- im_ff + im_ne_he 从串行 ~3.7s → 并行 ~1.3s（~2.8× 快）。
- subtitle_finder 端到端 **~55ms/帧 → ~22ms/帧**（优化 62%），输出 10 关键帧不变。

> 并行时 `get_im_ff` 传 `prof=None`（不记录子计时），thr_ms/im_ne_he_ms 并入 im_ff_ms。

**剩余热点**（并行后）：filter(连通域 BFS) 与 im_ff(并行) 各 ~40%。连通域 BFS 是
8 邻接逐像素串行，难进一步并行/SIMD，收益递减。

### 已知局限

- **长字幕 `has_text` 检测不稳定（段起始时间错乱）**：✅ **2026-08-11 已修**（`26dab27`）。
  根因：`second_filtration` 的 Center 偏移块移除 `lb[0]` 时漏了 C++ 的段数组左移一位
  （IPAlgorithms.cpp:2128-2133 `lb[i]=lb[i+1]`），使后续 mpd/mpned 循环看到的段数组/
  S/段边界与 C++ 不一致，长字幕（如"你开窍后获得了它的斩击能力"）has_text 抖动，段
  start 偏晚（实测 20033ms）。补左移后段 start 20033ms → 17533ms，段数 38。
  顺带澄清两个之前的旧误解：(1) `g_text_alignment` 默认是 `Center`（IPAlgorithms.cpp:170），
  不是 Any——之前"Any-skip"方向与 C++ 不一致，本包应为 Center（5c10a58 已切到 Center，
  段数 7→4 对齐 C++）；(2) sobel N-edge `up_l` 系数实为 10（C++ line 985
  `val=3*val1+10*val2`），不是 7——Rust 的 10 正确。

### 决策：段尾 PTS —— 对齐 C++（`PosForward[offset]-1`）vs 真实末帧 PTS 的取舍（`5384414`）

> ✅ **已定案，结论与本节「倾向回退」相反方向已落地**：当前用**真实末帧 PTS**
> `frame(fn+offset-1).pos`（`f31825d`），不跟随 C++ 的 `PosForward[offset]-1`。
> `todo.md`「已修复（全部对齐 C++）」与 `docs/perf/state-machine-perf-analysis.md` §8.4
> 均确认：段尾 1 帧差是 **C++ 侧 off-by-one**，Rust 不修、也不跟随。
> 本节是 2026-08 的决策过程记录（当时「待用户定夺」），**当前实现已按真实末帧 PTS**，
> 下面的「现状（5384414 已切到 C++ 语义）」不再成立。

C++ 段尾 `et = PosForward[offset] - 1`（SSAlgorithms.cpp:1815 / 1669）：取「新句首帧
PTS - 1ms」，是个近似——落在新旧交界处，**并不指向旧句最后真正可见的那一帧**。

对比两种实现（30fps 下差 ~1 帧 / 32ms）：

| 语义 | 段尾值 | 性质 |
| --- | --- | --- |
| C++ `PosForward[offset]-1` | `frame(fn+offset).pos - 1` | **与 C++ 输出逐字节一致**；代价是段尾虚推 ~32ms，落在字幕已消失之后那帧附近 |
| 真实末帧 PTS | `frame(fn+offset-1).pos` | **逻辑上更准**：真指旧字幕最后出现的帧；代价是与 C++ 差 ~32ms（那是 C++ 的近似误差） |

⚠️ **"更准"是错误表述，已更正**：改 C++ 语义**不是**更准，只是**更贴 C++ 输出**
（实测边界差从 32ms→1ms）。它给每段尾引入 ~32ms 虚推延迟。若目标为时间准确，真实末帧
PTS 才对；若目标为与 C++ 输出逐帧一致，则用 C++ 语义。

**现状（`5384414` 已切到 C++ 语义）**：段数由 `ef`（帧索引）决定、与 `et` PTS 无关，
故 52833 误保存段仍在（需另治）。此改动是否回退到真实末帧 PTS，待用户定夺——**倾向
回退**（真实末帧 PTS 逻辑更准，且该改动未解决 52833）。
