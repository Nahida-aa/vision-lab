# 单帧实测：Rust / Python / C++ 字幕 OCR 的耗时与内存

> 目的：给简历里「用 Rust（ort-rs / opencv-rs）替代 Python，降低内存占用和处理耗时」这句
> 业绩找证据，或者证伪它。
>
> 结论先放这：**「快」成立（同权重同线程约 1.25×，相对旧的每帧起进程调用约 2.3×）；
> 「省内存」不成立（按各自默认配置，Rust 峰值 932MB > Python 581MB）。**
> 速度收益主要来自「常驻 ORT session + 不每帧起进程」，不是 opencv-rs，也不是 Rust 语言本身。
>
> 测量日期：2026-11-14 ｜ 只测**同一帧**（按约定不跑全视频），重复采样取 P50/P95。

---

## 0. 被测对象与一句话结论

| 实现 | 入口 | 权重 |
| --- | --- | --- |
| Python | `packages/subtitle_ocr_py/main.py`（`rapidocr_onnxruntime`） | 包内自带 PP-OCRv4 det/rec（**sha256 与 `data/models/rapidocr` 的 v4 完全一致**） |
| Rust | `packages/subtitle_ocr`（`rapidocr_ort` + `subtitle_ocr_post`，`--model v4 / v3`） | `data/models/rapidocr` |
| C++ | `packages/subtitle_ocr_cpp`（ORT C API 直连） | `data/models/rapidocr`，**写死 v3** |

- **Rust vs Python 的公平对比 = 双方都用 v4**（哈希已验证同一份权重）。
- C++ 只能跑 v3，故它只作「同权重同线程下 Rust 是否打得过 C++」的对照，不与 Python 混比。

一句话：

- 同权重（v4）、同 4 线程、单帧纯 OCR：Python **447ms** → Rust **358ms**，**1.25×**。
- 旧的 Python 调用方式（每帧一个进程、每帧重建 3 个 ORT session）：**832ms/帧**；Rust 常驻
  session **358ms/帧**，**2.32×**。这 2.3× 里有 ~385ms/帧 是进程+session 重建，不是推理。
- 内存：Rust v4 峰值 **932MB** / 均值 916MB；Python v4 默认峰值 **581MB** / 均值 370MB。
  **Rust 反而更高**。同 ORT 配置（都开 arena）才是 932 vs 1162（Rust 低 20%），但那是
  Python 被改成非默认配置后的结果。

---

## 1. 实验条件

| 项 | 值 |
| --- | --- |
| CPU | AMD Ryzen 7 H 255 w/ Radeon 780M（8 物理核 / 16 逻辑核，`lscpu`） |
| GPU | 无独显；Radeon 780M 集显**未使用**，三个实现都是 `CPUExecutionProvider` |
| 内存 | 27.8 GB |
| OS / 内核 | Linux x86_64，KDE Wayland 桌面 |
| CPU 绑定 | `taskset -c 0-3`（固定 4 个 CPU；`nproc` 16） |
| ORT 线程 | 4（cpp `--threads 4`；python `intra_op_num_threads=4`；rust 走 ORT 默认 = 可见物理核 4） |
| 视频 | `tests/bench/subtitle_ocr/ref/video_source.mp4`，1280×720，30fps，170.06s，sha256 `34431682…76908`（与 README 记录一致） |
| 帧 | **1 张**：`0020000.jpg`（t=20.0s，1280×720，sha256 `da96093b…`），由 bench 的 2fps 抽帧产出 |
| 采样方式 | 该帧硬链接成 40 个文件名（同一 inode），供「常驻 session 的批量进程」产生 40 个逐帧样本 |
| OCR 参数 | `text_score/text_confidence_threshold=0.45`，`subtitle_only`，`bottom_only`（下 40% ROI） |
| 预热 | rust 预热 1 个完整 pass（40 帧）；python 批量预热 2 帧；cpp 无显式预热（首帧冷启动 +25ms，摊到 40 帧约 +0.6ms） |
| 连跑 | 每个配置 3 轮（python one-shot 每轮 5 次），逐帧样本合并后取 P50/P95 |
| 负载 | 跑前 loadavg 2.4 → 结束后 7.3（桌面有 Chrome/Zed/QQ 常驻）；**逐轮均值波动见 §3.5**，最大 4.9% |
| ORT 版本 | Python 1.28.0；Rust `ort 2.0.0-rc.13` 内置 **rel-1.28.0**（二进制内含 `ORT Build Info: rel-1.28.0`）；**C++ 1.24.4**（构建目录里的 `libonnxruntime.so.1`） |

> 注意：Rust 与 Python 的 ORT 是**同一版本 1.28.0**，两者对比不受 ORT 版本影响。
> 只有 C++（1.24.4）落后一个大版本，故 C++ 只作参考列。

---

## 2. 口径（这次必须写死，之前踩过坑）

| 口径 | 定义 | 本文是否使用 |
| --- | --- | --- |
| **单帧处理耗时**（本表主指标） | 进程内逐帧计时：读图解码 + det 推理 + det 后处理 + cls + rec 推理 + 聚合。**不含**进程启动、模型/session 加载、抽帧 | ✅ 主表 |
| 引擎初始化 | `engine_init_ms` = 建 3 个 ORT session + 载权重，只发生一次 | ✅ 单列 |
| 每帧墙钟（`--dir` 的 wall ÷ 帧数） | 批量进程总墙钟 ÷ 帧数，含模型加载摊薄、含 stdout 输出 | ❌ 不用（正是旧文档 355ms/590ms 的来源，两者口径并不完全一致） |
| 端到端含解码 | 进程启动 → 抽帧 → OCR → 退出 | ❌ 不用（抽帧属驱动层） |

具体到每个实现，逐帧数字是这样取的：

- **cpp**：它把 `detMs / postMs / recMs / imageLoadMs` 逐帧打到 stderr。本文的逐帧耗时 =
  `imageLoadMs + detMs + postMs + recMs`，**剔除 `modelLoadMs`**（该字段被每帧重复写一遍，
  所以它的 `totalMs` 会虚高 ~123ms/帧，不能直接用）。
- **rust**：新增探针 `tests/bench/subtitle_ocr/src/bin/probe.rs`，对 `subtitle_ocr::ocr_entry`
  调用前后 `Instant::now()`（该函数正是 CLI `--dir` 的单帧流程）。**未改被测实现逻辑**。
- **python**：`ocr_frame()`（`main.py` 原函数）调用前后 `perf_counter`，含 `cv2.imread`；
  同时记录它内部自报的 `inference_ms`（仅 `engine(roi)`）。两者差 ~4ms，即读图成本。

内存口径：

- **峰值 RSS**：`monitor.py` 用 `wait4`/`getrusage(RUSAGE_CHILDREN).ru_maxrss`（内核记账的
  精确峰值，含启动与模型加载）。
- **平均 RSS**：外部每 20ms 读 `/proc/<pid>/status` 的 `VmRSS` 求均值（含启动爬升段）。
- 不用进程内 `tracemalloc`（只统计 Python 堆，无法与 Rust/C++ 横比）。

---

## 3. 结果

> 原始数据：`tests/bench/subtitle_ocr/rust-vs-py/results/`（`summary.md` 由 `summarize.py` 生成）。

### 3.1 单帧处理耗时（ms，4 线程，同一帧）

| 实现 | 权重 | 样本 n | avg | P50 | P95 | min | max |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| **Rust**（`--model v4`） | v4 | 120 | **358.4** | **355.8** | 377.6 | 316.4 | 532.7 |
| **Python**（批量，常驻 engine） | v4 | 120 | **447.4** | **445.7** | 482.9 | 396.1 | 498.2 |
| **Python**（每帧一个进程） | v4 | 15 | **831.7** | 825.0 | 891.0 | 781.6 | 899.4 |
| Rust（`--model v3`） | v3 | 120 | 221.9 | 221.6 | 240.0 | 201.7 | 249.8 |
| C++（`--dir --threads 4`） | v3 | 120 | 256.2 | 256.3 | 280.2 | 221.2 | 324.9 |

派生比值（同权重、同线程口径）：

| 对比 | 比值 | 说明 |
| --- | ---: | --- |
| Rust v4 / Python v4（批量） | **1.25×** | 同权重同线程的「纯单帧处理」差距 ✅ |
| Rust v4 / Python v4（每帧起进程） | **2.32×** | 旧基准的调用方式，差距主要来自进程与 session 重建 ✅ |
| Rust v3 / C++ v3 | 0.87×（Rust 快 13%） | 同权重同线程，Rust 并不劣于 C++ |
| C++ v3 / Python v4 | 1.75× | **不同权重**，不能当 Rust 的业绩引用（旧文档的 1.7× 属于这一栏） |

引擎初始化（只发生一次）：

| 实现 | 初始化耗时 |
| --- | ---: |
| Rust v4 | 140–150ms |
| Rust v3 | 126–127ms |
| Python `import main`（含建 3 session） | 134–152ms（另加解释器启动 ~12ms、`import cv2` 90–130ms） |
| C++ 模型加载 | 122ms |

**三个实现的 session 初始化成本其实差不多（~130ms）。** 差距不在「Rust 建 session 更快」，
而在 Python 的旧调用路径**每帧都重付这 ~130ms，外加解释器启动和 `import cv2`**。

### 3.2 内存（MB）

| 实现 | 权重 / ORT 配置 | 峰值 RSS | 各轮峰值 | 平均 RSS | 线程峰值 |
| --- | --- | ---: | --- | ---: | ---: |
| Rust v4 | v4 / arena 开（ORT 默认） | **931.8** | 928, 932, 928 | 916.1 | 25 |
| Python v4（默认） | v4 / **arena 关**（包内硬编码） | **580.7** | 577, 577, 581 | 370.0 | 19 |
| Python v4（诊断：开 arena） | v4 / arena 开 | **1162.0** | 1162 | 1023.8 | 19 |
| Python v4（每帧一个进程） | v4 / arena 关 | 511.0 | 511, 511, 510 | —（进程太短） | — |
| Rust v3 | v3 / arena 开 | 757.0 | 757, 739, 734 | 728.7 | 25 |
| C++ v3 | v3 / arena 开 | 757.6 | 754, 758, 757 | 729.3 | 13 |

读法：

1. **按各自默认配置：Rust 峰值 932MB > Python 581MB。** 「降低内存占用」不成立。
   Python 默认更省，是因为 `rapidocr_onnxruntime` 在 `OrtInferSession` 里写死了
   `enable_cpu_mem_arena = False`（`utils/infer_engine.py`），而 Rust/C++ 用 ORT 默认（开）。
2. **把 arena 拉平（都开）：Rust 932MB vs Python 1162MB，Rust 低 ~20%。** 这才是
   「同配置下的语言/实现差异」，量级是 20%，不是数倍。
3. **Rust v3 与 C++ v3 的峰值/均值几乎逐位相同（757/729 vs 758/729）。** 同一份 ORT
   配置下，Rust 与 C++ 的内存开销没有差别 —— 说明**内存由 ORT arena + 模型/激活占用决定，
   和宿主语言无关**。Python 运行时全部算上（裸解释器 13MB、`import onnxruntime` 峰值 46MB）
   也只有几十 MB，对比 900MB 级的 ORT 占用是噪声。
4. Python 的「平均 370MB < 峰值 581MB」说明关掉 arena 后内存会还给系统；Rust/C++ 开着
   arena 时 RSS 基本贴着峰值不回落。这也解释了为什么 Rust 的均值反而更高。

> 归因结论：**省内存的功劳不属于「改用 Rust」，属于 ORT 的 arena 配置。** 想真的省，
> 应该关掉 Rust 侧的 CPU arena（本仓库当前没这个开关），而不是重写语言。

### 3.3 识别质量（本次只做单帧一致性）

按约定只跑一帧，**不做全视频 CER**。这一帧上三实现逐字一致：

| 实现 | 输出 | 3 轮是否一致 |
| --- | --- | --- |
| Rust v4 | `不对醒来` | ✅ 3/3 相同 |
| Python v4 | `不对醒来` | ✅ 3/3 相同 |
| C++ v3 | `不对 醒来`（两个框以空格连接，字符相同） | ✅ 3/3 相同（40 帧 × 3 轮） |

全视频质量数字不在本次重测范围内，引用既有基准（**口径与本文不同，别和上表混用**）：

- `tests/bench/subtitle_ocr/README.md`：rust（v3，轴对齐）`CER(norm)=0.72% / CER(paired)=0.18% /
  spurious 4`；rust（v3 + `--warp-crop`）与 cpp **逐位一致**：`0.36% / 0.36% / 0 spurious /
  0 missed`。
- 注意：README 里的 rust 数字是 **v3** 权重；本文同 Python 对比用的是 **v4**。v4 的全视频
  CER 在 `packages/tmp/summary_new_r*.json` 里是 `CER(norm)=1.25% / spurious 5`（未入文档、
  未在本次复测），引用时需另行确认。

### 3.4 线程数（只作为口径补充）

| 配置 | 线程峰值 | avg/帧 |
| --- | ---: | ---: |
| Rust v4，`taskset 0-3`（4 线程） | 25 | 358.4ms |
| Rust v4，不绑定（ORT 默认 8 物理核） | 37 | 366.8ms |
| Python v4，4 线程 | 19 | 447.4ms |
| Python v4，`intra=-1`（不绑定） | 67 | 427.9ms |

线程加多并没有收益（Rust 反而慢 2%，Python 只快 4%），与 `justfile` 里
「16 线程反而更慢、rec 劣化 5 倍」的既有结论方向一致。**故主表统一固定 4 线程。**

### 3.5 波动

| 配置 | 三轮均值 | 轮间波动 |
| --- | --- | ---: |
| Rust v4 | 369.4 / 353.3 / 352.3 ms | 4.9% |
| Rust v3 | 223.4 / 223.7 / 218.5 ms | 2.4% |
| Python v4 | 447.7 / 448.5 / 446.0 ms | 0.6% |
| Python one-shot | 830.9 / 837.4 / 826.8 ms | 1.3% |

第一轮 loadavg 最低但 Rust 最慢（首次触页 + 桌面其它进程），故 Rust 的 358ms 略偏高；
用最快一轮 352ms 算，Rust/Python = **1.27×**。结论方向不变。

---

## 4. 归因：1.7× / 2.3× 到底是谁的功劳

### 4.1 「Python 每帧重建 ORT session」是真的吗？

**分两种调用方式：**

- **在一个进程里跑**：`main.py` 顶层 `engine = RapidOCR()` 只建一次，det/cls/rec 三个 session
  常驻。这时 Python 没有「每帧重建」，单帧 447ms。
- **每帧起一个进程**（旧基准 `bench` 的调用方式，也是 `subtitle_ocr_py` 作为 CLI 的用法）：
  **每帧重建 3 个 session**，外加解释器启动、`import cv2`、无预热。单帧 832ms。

差值 **832 − 447 = 385ms/帧**，与「`import main` ~340ms + 冷启动首次推理比预热后慢 ~50ms」
基本对得上。**旧文档 1.7× 的差距，主因就是这个调用方式差异，不是 Python 推理慢 1.7 倍。**

Rust/C++ 天然是「一个进程、session 常驻、批量跑目录」，所以在这个对比里占便宜。
这不是语言优势，是**进程模型**优势。要公平比较「同权重同线程的纯推理」，就是 447 vs 358（1.25×）。

**session 重建对内存没有影响。** 峰值 RSS 是「一个进程内看到的最大常驻集」，不跨进程累加：
Python 每帧起进程的峰值是 **511MB**，反而比常驻进程的 581MB 略低（因为每帧只跑一次、
没有预热后的缓存增长）。所以「每帧重建 session」解释的是**耗时**，解释不了「省内存」；
拿它去支撑「降低内存占用」是错的。

### 4.2 opencv-rs 的贡献

cpp 的分相实测（同一帧、4 线程）：

| 读图 | det 推理 | det 后处理（含 NMS/unclip） | rec 推理 | 模型加载（一次） |
| ---: | ---: | ---: | ---: | ---: |
| 3.8ms | 229.7ms | **1.5ms** | 21.1ms | 123ms |

后处理只占 **0.6%**。旧文档「后处理只占 8ms/帧」的结论方向一致（本机更快）。
**把后处理换成 `cv::`/opencv-rs，收益上限就是 1~8ms/帧，撑不起「降低处理耗时」这句话。**
opencv-rs 在 Rust 侧的真实价值是**数值一致性**（`cv::warpPerspective` 与 cpp 逐位一致，
把 `--warp-crop` 下的 CER 从 0.54% 拉回 0.36%、missed 1→0），不是提速。归因给 opencv-rs 是错的。

### 4.3 burn 该不该出现在这句里

- 本仓库 `Cargo.toml` 的 `[workspace.dependencies]` 里确实有一行 `burn = "0.21.0"`，但
  **没有任何成员包依赖它**，全仓库 `use burn` 命中 0 次（`grep -rn "use burn" --include=*.rs` 为空）。
- 真正用 burn 的是 **vox-lab**：`packages/voxcpm-burn`（VoxCPM 语音合成/推理）、
  `packages/demucs-burn`（Demucs 人声分离），都是 `burn = "0.20.1"` + wgpu/vulkan 后端。
- 字幕 OCR 链路的技术栈是 **ort-rs（`rapidocr_ort`）+ opencv-rs（`rapidocr_ort::det/pipeline`
  里的 `opencv::imgproc`）+ faer/geometry**，与 burn 无关。

**结论：这句把两个不同工作的技术栈串了。** 建议拆成两条业绩（字幕 OCR / 语音侧 burn 迁移），
或把 burn 从字幕 OCR 这句里删掉。

---

## 5. 结论：哪些说法成立

| 简历原句成分 | 是否成立 | 证据 |
| --- | --- | --- |
| 「用 ort-rs / opencv-rs 替代 Python」 | ✅ 成立 | 代码结构；`rapidocr_ort` 依赖 `ort` + `opencv` |
| 「用 burn 替代…（字幕 OCR）」 | ❌ 不成立 | vision-lab 无 burn 使用；burn 在 vox-lab 语音侧 |
| 「降低处理耗时」 | ✅ 部分成立 | 同权重同线程 **1.25×**（447→358ms）；相对每帧起进程 **2.32×**（832→358ms） |
| 「降低内存占用」 | ❌ 不成立（按各自默认配置） | Rust 932MB vs Python 581MB 峰值；同 ORT 配置下才是 932 vs 1162（-20%） |
| 「替代原先 Python 中某些多模型串行的流程」 | ⚠️ 表述含糊 | Python 侧 `engine` 本身就是常驻的 3 模型串行；「每帧重建」只发生在 CLI 每帧起进程的调用方式 |

**如果只能留一句话：保留「快」，删掉「省内存」，删掉 burn，并把「多模型串行」改成具体的
调用方式差异（进程/session 重建）。**

---

## 6. 简历那句该怎么写（可直接粘贴）

### 候选 A（最保守，只写有证据的耗时）

> 把字幕 OCR 从 Python（rapidocr-onnxruntime）迁移到 Rust（ort-rs + opencv-rs）常驻推理：
> 同权重（PP-OCRv4）、同 4 线程下，单帧 OCR 从 447ms 降到 358ms（约 1.25×）；相对原先
> 每帧起进程、重建 3 个 ORT session 的调用方式，单帧从 832ms 降到 358ms（约 2.3×）。

### 候选 B（带上归因，显得可信；适合技术面展开）

> 字幕 OCR 从 Python 迁到 Rust 常驻 ORT session：消除每帧进程启动与 3 个 session 重建，
> 单帧 832→358ms（2.3×）；同权重同线程的纯推理 447→358ms（1.25×）。收益来自常驻 session
> 与批处理，而非语言本身——作为对照，C++ 直连 ORT 在同权重下为 256ms（Rust 221ms）。

### 数字证据等级

| 数字 | 证据 | 备注 |
| --- | --- | --- |
| 447ms（Python v4，批量，4 线程） | ✅ 本次实测，120 样本，波动 0.6% | 单帧 `0020000.jpg` |
| 358ms（Rust v4，4 线程） | ✅ 本次实测，120 样本，波动 4.9% | 单帧同一图 |
| 832ms（Python 每帧一进程） | ✅ 本次实测，15 样本 | 旧 590ms 是另一台机器/另一版本的同类口径 |
| 1.25× / 2.32× | ✅ 由上三行相除 | 同口径相除，未混用 |
| 256ms（C++ v3）/ 221ms（Rust v3） | ✅ 本次实测，120 样本 | 同权重同线程 |
| 932MB / 581MB（Rust / Python 峰值 RSS） | ✅ 本次实测，3 轮 | 各自默认 ORT 配置 |
| 1162MB（Python 开 arena） | ✅ 本次实测（诊断配置，非产品默认） | 仅用于归因 |
| 「降低内存占用」 | ❌ **无证据支持** | 按默认配置 Rust 更高 |
| 全视频 CER 0.36% / 0.72% | ⚠️ 引用既有 README，**本次未复测** | 且 0.72% 是 v3；v4 未入文档 |
| 「多模型串行」「burn」 | ❌ 与代码不符 | 见 §4.3 |

---

## 7. 复现

```bash
# 1) 生成「同一帧 ×40 硬链接」目录（不跑全视频）
bash tests/bench/subtitle_ocr/rust-vs-py/create_frames.sh

# 2) 三实现 × 3 轮（含逐帧计时 + 外部 RSS 采样）
bash tests/bench/subtitle_ocr/rust-vs-py/run.sh 3

# 3) 汇总表
python3 tests/bench/subtitle_ocr/rust-vs-py/summarize.py
```

文件：

| 文件 | 作用 |
| --- | --- |
| `tests/bench/subtitle_ocr/src/bin/probe.rs` | Rust 单帧探针（计时，未改实现逻辑） |
| `tests/bench/subtitle_ocr/rust-vs-py/monitor.py` | 子进程峰值/平均 RSS 采样 |
| `tests/bench/subtitle_ocr/rust-vs-py/py_batch.py` | Python 批量/one-shot 驱动；`--arena-on` 为归因诊断开关 |
| `tests/bench/subtitle_ocr/rust-vs-py/run.sh` | 跑批（`taskset 0-3`） |
| `tests/bench/subtitle_ocr/rust-vs-py/summarize.py` | 出表 |
| `tests/bench/subtitle_ocr/rust-vs-py/results/` | 原始 JSON / cpp stderr / `summary.md` |

---

## 8. 局限（引用这些数字时必须一起说）

1. **只测 1 帧**（按要求）：这是「同一帧重复 40 次」的稳态单帧成本，不代表全视频分布；
   不同帧的字幕框数量不同，rec 开销会变（本例仅 1 个框，rec 21ms，偏轻）。
2. **Rust 与 Python 是 v4，C++ 是 v3**；三者 ORT 版本为 1.28 / 1.28 / **1.24.4**。
3. **arena 默认不同**（Python 关、Rust/C++ 开），这是内存结论的关键变量，不可忽略。
4. Python 的 one-shot 口径包含解释器启动与 `import cv2`，与「Rust 常驻进程」不是同一件事，
   只能作为「旧调用方式」的证据，不能当作「Python 推理慢」。
5. 桌面有常驻负载（loadavg 2.4→7.3），逐轮波动最大 4.9%；绝对值会有 ±5% 漂移。
6. 线程数固定 4，未测其它值（§3.4 只做了 auto 的旁证）。
7. `subtitle_only` 的窗口定义三实现不完全一致：Python 用绝对像素 `620 ≤ y ≤ 700`，
   Rust/C++ 用比例 `0.85 ≤ y/h ≤ 0.99`（720p 下 612–713px）。本帧的字幕框（y 647–682）
   两种窗口都包含，故不影响本次单帧结果；换帧复核时需注意。
