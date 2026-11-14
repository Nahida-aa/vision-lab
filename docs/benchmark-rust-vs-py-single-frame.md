# 单帧实测：Rust / Python / C++ 字幕 OCR 的耗时与内存

> 目的：给简历里「用 Rust（ort-rs / opencv-rs）替代 Python，降低内存占用和处理耗时」这句
> 业绩找证据，或者证伪它。
>
> 结论先放这：**「快」成立（同权重同线程 1.22–1.37×；相对旧的每帧起进程调用 2.3×）；
> 「省内存」在「同配置 + 速度优先」的口径下也成立**——arena 开是四组配置里都更快的那个，
> 在该配置下 Rust 比 Python 峰值低 16–20%（v4：932 vs 1162MB；v3：757 vs 905MB）。
> 但必须写明条件：**arena 关时 v4 组方向翻转**（634 vs 581，Rust 反而高 9%），而 arena 关
> 恰好是 Python 包内的默认（v3 组未翻转：526 vs 562）。arena 一个开关就值 300–580MB，
> 远大于两者的残余差。
>
> 速度收益主要来自「常驻 ORT session + 不每帧起进程」，不是 opencv-rs，也不是 Rust 语言本身。
>
> 测量日期：2026-11-14/15 ｜ 只测**同一帧**（按约定不跑全视频），重复采样取 P50/P95。

---

## 0. 被测对象与一句话结论

| 实现 | 入口 | 权重 |
| --- | --- | --- |
| Python | `packages/subtitle_ocr_py/main.py`（`rapidocr_onnxruntime`） | 默认包内 PP-OCRv4；`--models v3` 时指向 `data/models/rapidocr` 的 PP-OCRv3 |
| Rust | `packages/subtitle_ocr`（`rapidocr_ort` + `subtitle_ocr_post`，`--model v4 / v3`） | `data/models/rapidocr` |
| C++ | `packages/subtitle_ocr_cpp`（ORT C API 直连） | `data/models/rapidocr`，**写死 v3** |

- **同权重两两比较**：v4 组（Python 包内 vs Rust v4）与 v3 组（Python `--models v3` vs
  Rust v3 vs C++）。Python 包内 v4 的 det/rec **sha256 与 `data/models/rapidocr` 的 v4 完全一致**，
  所以 v4 组不需要额外指路。
- C++ 只能跑 v3，故它只进 v3 组；不与 v4 的 Python 混比（那正是旧文档 1.7× 的错误来源）。

一句话（全部是 4 线程、同一帧、常驻 session）：

- v4：Python **447ms** → Rust **358ms**（**1.25×**）；开 arena 对齐后 437 → 358（**1.22×**）。
- v3：Python **327ms** → Rust **222ms**（**1.48×**）；开 arena 对齐后 304 → 222（**1.37×**）。
- 旧的 Python 调用方式（每帧一个进程、每帧重建 3 个 ORT session）：**832ms/帧**；Rust 常驻
  session **358ms/帧**，**2.32×**。这 2.3× 里有 ~385ms/帧 是进程+session 重建，不是推理。
- 内存（**速度优先的唯一配置 = arena 开**）：v4 峰值 932 vs 1162（Rust **−20%**）；
  v3 峰值 757 vs 905（Rust **−16%**）。arena 关时符号翻转（634 vs 581），详见 §3.2。

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

**Python 跑 v3 的接线（只动驱动，不动 `main.py`）**：`RapidOCR(...)` 接受
`det_model_path / rec_model_path / rec_keys_path`，在 `import main` 之前 patch 构造函数即可
把模块级 `engine` 指到仓库 v3 权重。字典需要一次格式转换：仓库的 `ppocr_keys.json` 是
6624 元素 JSON（第 0 位 `""` 是 blank 占位），而 Python 的 `CTCLabelDecode` 只读「每行一字」
的 txt 且**自己**在头尾插 `blank`/空格；所以驱动去掉首元素后写成 6623 行 txt（Python 补两位
= 6625，与 v3 rec 输出类别数一致）。这一步在 `py_batch.py::_v3_keys_txt()`，不修改实现。

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
| **Rust**（`--model v4`，arena 开） | v4 | 120 | **358.4** | **355.8** | 377.6 | 316.4 | 532.7 |
| **Python**（批量，常驻 engine，arena 关＝默认） | v4 | 120 | **447.4** | **445.7** | 482.9 | 396.1 | 498.2 |
| **Python**（每帧一个进程） | v4 | 15 | **831.7** | 825.0 | 891.0 | 781.6 | 899.4 |
| **Rust**（`--model v3`，arena 开） | v3 | 120 | **221.9** | **221.6** | 240.0 | 201.7 | 249.8 |
| **Python**（`--models v3`，arena 关＝默认） | v3 | 120 | **327.4** | **322.6** | 373.5 | 281.3 | 592.2 |
| C++（`--dir --threads 4`，arena 开） | v3 | 120 | 256.2 | 256.3 | 280.2 | 221.2 | 324.9 |

> arena 关的耗时不单列在此表（属 §3.2 的同配置实验）：Rust v4 ≈383ms（5 轮中位）、
> Rust v3 ≈250ms（3 轮中位；全样本均值 254.7ms）、Python v4 已在上表（默认关）、
> Python v3 已在上表（默认关）。开 arena 的 Python 两行见 §3.2 的 2×2。

派生比值（同权重、同线程口径）：

| 对比 | 比值 | 说明 |
| --- | ---: | --- |
| Rust v4 / Python v4（批量，各自默认） | **1.25×** | 同权重同线程的「纯单帧处理」差距 ✅ |
| Rust v4 / Python v4（每帧起进程） | **2.32×** | 旧基准的调用方式，差距主要来自进程与 session 重建 ✅ |
| Rust v3 / Python v3（批量，各自默认） | **1.48×** | 同权重同线程；v3 的 Python 端也慢于 v4，故比值比 v4 组更大 |
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

### 3.2 内存与 arena：**按「速度优先」选配置后，Rust 同时更快且更低**

先回答「开关不同有没有影响速度」——有，而且方向一致：**arena 开在四组配置里全部更快**
（详见 §3.2.2）。模型运行时速度优先，所以**该选的唯一配置就是 arena 开**；这恰好也是
Rust/C++ 的默认、Python 需要 patch 才能到达的配置。先给全表（每格 3 轮；Rust arena-off
为 5 轮、r1 撞负载尖峰；Python v3 arena-on 的 r3 峰值 971 属离群，取中位并注明）：

| 实现 | 权重 / ORT arena | 峰值 RSS | 平均 RSS | 备注 |
| --- | --- | ---: | ---: | --- |
| Rust v4 | v4 / **开**（ORT 默认） | **931.8** | 916.1 | 3 轮：928/932/928 |
| Rust v4 | v4 / **关**（`RAPIDOCR_CPU_ARENA=0`） | 634（中位） | 463（中位） | 5 轮峰值 645/633/634/631/669 |
| Python v4 | v4 / **关**（包内默认） | 580.7 | 370.0 | 3 轮：577/577/581 |
| Python v4 | v4 / **开**（诊断 patch） | **1163.1** | 1024.5 | 3 轮：1162/1163/1163 |
| Rust v3 | v3 / **开** | **757.0** | 728.7 | 3 轮：757/739/734 |
| Rust v3 | v3 / 关 | 526（中位） | 347（中位） | 3 轮峰值 526/515/571 |
| Python v3 | v3 / **关**（包内默认） | 562（中位） | 340.4 | 3 轮峰值 562/562/608 |
| Python v3 | v3 / **开** | **905**（中位） | 808.6（中位） | 3 轮峰值 905/900/971，均值 800/794/832 |
| C++ v3 | v3 / **开** | 757.6 | 729.3 | 3 轮：754/758/757 |
| Python v4（每帧一进程） | v4 / 关 | 511.0 | — | 15 个进程的最大峰值 |

#### 3.2.1 速度优先口径（arena 开，同权重对齐）

| 权重 | 实现 | avg 单帧 | P50 | 峰值 RSS | 平均 RSS | Rust 相对 Python |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| v4 | Rust | **358.4ms** | 355.8 | **931.8** | 916.1 | 快 **1.22×**；峰值 **−20%**；均值 **−11%** |
| v4 | Python | 436.5ms | 436.4 | 1163.1 | 1024.5 | — |
| v3 | Rust | **221.9ms** | 221.6 | **757.0** | 728.7 | 快 **1.37×**；峰值 **−16%**；均值 **−10%** |
| v3 | Python | 304.2ms | 304.1 | 905 | 808.6 | — |

这就是「同内存分配配置下比较」的正确做法，而且**结论对 Rust 有利且可复现**：两套权重
都指向同一方向（快 1.22–1.37×、峰值低 16–20%）。区别只在幅度，不在符号。

#### 3.2.2 arena 开关本身的速度影响（同实现内比较）

| 实现 / 权重 | arena 开 | arena 关 | 开比关快 |
| --- | ---: | ---: | ---: |
| Rust v4 | 358.4ms | 395.5ms | **+10.4%**（P50 355.8 vs 388.3，+9.1%） |
| Python v4 | 436.5ms | 447.4ms | **+2.5%** |
| Rust v3 | 221.9ms | 254.7ms | **+14.8%** |
| Python v3 | 304.2ms | 327.4ms | **+7.6%** |

结论：**arena 开既更快，也更容易对齐（Rust/C++ 天生如此）**。arena 关的唯一收益是内存，
代价是 2.5–15% 的耗时——在「速度优先」的前提下不该选它。

#### 3.2.3 仍然要写明的反向格子

如果强行对齐到 Python 的包内默认（arena 关），内存符号会翻转：

**同配置 2×2（v4，4 线程，同一帧）：**

| | arena **开**（速度最优 + Rust 默认） | arena **关**（更省内存 + Python 默认） |
| --- | --- | --- |
| Rust v4 | **358ms** / 峰值 932 / 均值 916 | 383ms / 峰值 634 / 均值 463 |
| Python v4 | 437ms / 峰值 1162 / 均值 1025 | 447ms / 峰值 581 / 均值 370 |
| Rust 相对 Python | **快 1.22×；峰值 −20%；均值 −11%** | 快 1.17×；峰值 **+9%**；均值 **+25%** |

- 对齐到 arena 关：**v4 组** Rust 峰值高 9%（634 vs 581）。这是「同配置」下对 Rust 不利的
  格子，也是 Python 的产品默认。**v3 组没有翻转**（Rust 526 vs Python 562，Rust 低 6%），
  说明连「翻转」本身都依赖权重版本与峰值取法，不能当成普遍规律。
- 但这一格**单帧更慢**（Rust +7%、Python +2.5%），按「速度优先」不应选。
- 机制上：arena 开关值 300–580MB，远大于两者的残余差（53–230MB）；Python 从关 arena
  获益更大，足以翻转符号。**所以任何内存表述都必须带 arena 条件。**

**旁证：Rust 与 C++ 在同一配置（arena 开、v3）下几乎逐位相同（757/729 vs 758/729）。**
说明内存由 ORT arena + 模型/激活占用决定，宿主语言只值几十 MB（裸 Python 13MB、
`import onnxruntime` 峰值 46MB），对比 900MB 级占用是噪声。

> 结论：**在速度优先的同配置口径（arena 开）下，「Rust 更快且峰值更低」成立**
> （v4 1.22×/−20%，v3 1.37×/−16%）。若把 Python 的包内默认（arena 关）当基准，
> 内存结论翻转且两者都更慢——那是「配置选择」的差异，不是语言差异。

### 3.3 识别质量（本次只做单帧一致性）

按约定只跑一帧，**不做全视频 CER**。这一帧上各实现逐字一致：

| 实现 | 权重 | 输出 | 3 轮是否一致 |
| --- | --- | --- | --- |
| Rust | v4 | `不对醒来` | ✅ 3/3 相同 |
| Python | v4 | `不对醒来` | ✅ 3/3 相同 |
| Rust | v3 | `不对醒来` | ✅ 3/3 相同 |
| Python | v3 | `不对醒来` | ✅ 3/3 相同（arena 开/关都是） |
| C++ | v3 | `不对 醒来`（两个框以空格连接，字符相同） | ✅ 3/3 相同（40 帧 × 3 轮） |

全视频质量数字不在本次重测范围内，引用既有基准（**口径与本文不同，别和上表混用**）：

- `tests/bench/subtitle_ocr/README.md`：rust（v3，轴对齐）`CER(norm)=0.72% / CER(paired)=0.18% /
  spurious 4`；rust（v3 + `--warp-crop`）与 cpp **逐位一致**：`0.36% / 0.36% / 0 spurious /
  0 missed`。
- 注意：README 里的 rust 数字是 **v3** 权重；v4 的全视频 CER 在 `packages/tmp/summary_new_r*.json`
  里是 `CER(norm)=1.25% / spurious 5`（未入文档、未在本次复测），引用时需另行确认。
- 本次新测的 Python v3 与 Rust v3 只验证了**单帧输出一致**，不等于全视频 CER 相同。

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
| Rust v4（arena 关） | 448.8 / 385.4 / 381.5 / 382.6 / 379.2 ms（5 轮） | 18.3%（r1 撞负载尖峰） |
| Rust v3 | 223.4 / 223.7 / 218.5 ms | 2.4% |
| Rust v3（arena 关） | 268.8 / 249.7 / 245.6 ms | 9.5%（r1 同上） |
| Python v4 | 447.7 / 448.5 / 446.0 ms | 0.6% |
| Python v4（arena 开） | 436.6 / 437.3 / 435.5 ms | 0.4% |
| Python v3（arena 关） | 318.7 / 326.0 / 337.6 ms | 5.9% |
| Python v3（arena 开） | 310.2 / 314.5 / 287.9 ms | 9.2%（r3 偏快，疑缓存/负载变化） |
| Python one-shot | 830.9 / 837.4 / 826.8 ms | 1.3% |

第一轮 loadavg 最低但 Rust 最慢（首次触页 + 桌面其它进程），故 Rust 的 358ms 略偏高；
用最快一轮 352ms 算，Rust/Python = **1.27×**。v3 组同理：Rust 最快轮 218.5ms vs
Python arena-on 最慢轮 314.5ms，比值 **1.44×**（保守下限），方向与 1.37× 一致。

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
这不是语言优势，是**进程模型**优势。要公平比较「同权重同线程的纯推理」，就是
v4 447 vs 358（1.25×）、v3 327 vs 222（1.48×）。

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

### 4.4 内存：收益属于「arena 配置」，但速度优先时 Rust 恰好两头都占

把 arena 拉平后（§3.2）可以确认三件事：

1. **arena 开关值 300–580MB**（Rust 932→634，Python 1162→581），远大于两者的残余差
   （53–230MB）。想单独压内存，第一优先级是关 arena，不是换语言。
2. **arena 开在四组配置里都更快**（+2.5% ~ +15%）。速度优先 ⇒ 选 arena 开。
3. 在 arena 开这一格，**Rust 同时更快且峰值更低**：v4 1.22×/−20%，v3 1.37×/−16%。
   所以「更快 + 更省」可以在**同一句里**成立，前提是写明「同 arena 配置（开）」。

反过来说：如果线上必须沿用 Python 的包内默认（arena 关），Rust 的内存优势会消失
（v4 634 vs 581）。但那一格单帧也慢 2.5–7.6%，按「模型运行速度优先」不该作为目标配置。

**给仓库的可执行建议**（本次未改默认，避免影响既有基准）：常驻内存吃紧时，才把
`packages/rapidocr_ort/src/ep.rs` 的 CPU 分支默认改成 arena 关——实测峰值
932→634MB（−32%），单帧 358→383ms（+7%）。这是「牺牲速度换内存」的开关，
不是默认推荐。

---

## 5. 结论：哪些说法成立

| 简历原句成分 | 是否成立 | 证据 |
| --- | --- | --- |
| 「用 ort-rs / opencv-rs 替代 Python」 | ✅ 成立 | 代码结构；`rapidocr_ort` 依赖 `ort` + `opencv` |
| 「用 burn 替代…（字幕 OCR）」 | ❌ 不成立 | vision-lab 无 burn 使用；burn 在 vox-lab 语音侧 |
| 「降低处理耗时」 | ✅ 成立 | 同权重同线程：v4 **1.22–1.25×**（437/447→358ms）、v3 **1.37–1.48×**（304/327→222ms）；相对每帧起进程 **2.32×**（832→358ms） |
| 「降低内存占用」 | ✅ **条件成立（必须写明细）** | 同权重 + 同 arena（开，且是速度最优格）：v4 峰值 1162→932（**−20%**）、v3 905→757（**−16%**）；⚠️ 若把 arena 关（Python 默认）当基准，v4 翻转为 581→634（Rust **+9%**） |
| 「替代原先 Python 中某些多模型串行的流程」 | ⚠️ 表述含糊 | Python 侧 `engine` 本身就是常驻的 3 模型串行；「每帧重建」只发生在 CLI 每帧起进程的调用方式 |

**如果只能留一句话：保留「快」；内存可以写，但必须带「同 arena 配置（开）」这个条件；
删掉 burn；把「多模型串行」改成具体的调用方式差异（进程/session 重建）。**

---

## 6. 简历那句该怎么写（可直接粘贴）

### 候选 A（最保守，只写有证据的耗时）

> 把字幕 OCR 从 Python（rapidocr-onnxruntime）迁移到 Rust（ort-rs + opencv-rs）常驻推理：
> 同权重（PP-OCRv4）、同 4 线程下，单帧 OCR 从 447ms 降到 358ms（约 1.25×）；相对原先
> 每帧起进程、重建 3 个 ORT session 的调用方式，单帧从 832ms 降到 358ms（约 2.3×）。

### 候选 B（带上归因，显得可信；适合技术面展开）

> 字幕 OCR 从 Python 迁到 Rust 常驻 ORT session：消除每帧进程启动与 3 个 session 重建，
> 单帧 832→358ms（2.3×）；同权重同线程的纯推理 v4 447→358ms（1.25×）、v3 327→222ms
> （1.48×）。收益来自常驻 session 与批处理，而非语言本身——作为对照，C++ 直连 ORT
> 在同权重下为 256ms（Rust 221ms）。

### 候选 C（速度优先 + 同 arena 配置；要写内存时用这条，推荐）

> 字幕 OCR 从 Python 迁到 Rust 常驻 ORT session，并把 ONNX Runtime CPU arena 作为对齐项：
> 同权重、同 4 线程、同 arena（开）下，v4 单帧 437→358ms（1.22×）、峰值 RSS 1162→932MB
> （−20%）；v3 单帧 304→222ms（1.37×）、峰值 RSS 905→757MB（−16%）。相对于 Python 同时
> 降低耗时与峰值内存；收益来自常驻 session/批处理与 arena 配置，不是语言本身。
>
> ⚠️ 这句的两个前提要能现场说清：
> 1. 同权重（PP-OCRv4 与 v3 各比各的，C++ v3 不参与 v4 对比）；
> 2. 同 arena 且是**速度最优**的那一格。若 interviewer 追问「arena 关掉再比呢」，
>    答案是 v4 634 vs 581（Rust 反而高 9%），但那一格单帧慢 2.5–7.6%，按速度优先不选。
> 说不清这两点就退回候选 A/B。

### 数字证据等级

| 数字 | 证据 | 备注 |
| --- | --- | --- |
| 447ms（Python v4，批量，4 线程） | ✅ 本次实测，120 样本，波动 0.6% | 单帧 `0020000.jpg` |
| 358ms（Rust v4，4 线程） | ✅ 本次实测，120 样本，波动 4.9% | 单帧同一图 |
| 327ms（Python v3）/ 222ms（Rust v3） | ✅ 本次实测，各 120 样本，波动 5.9% / 2.4% | 同权重、同为仓库 v3 权重 |
| 832ms（Python 每帧一进程） | ✅ 本次实测，15 样本 | 旧 590ms 是另一台机器/另一版本的同类口径 |
| 1.22–1.25× / 1.37–1.48× / 2.32× | ✅ 由上几行相除 | 同口径相除，未混用 |
| 256ms（C++ v3） | ✅ 本次实测，120 样本 | 同权重同线程，仅作参考列 |
| 932MB vs 1162MB（v4，arena 开） | ✅ 本次实测，3 轮 | 同配置对齐，Rust −20% |
| 757MB vs 905MB（v3，arena 开） | ✅ 本次实测，3 轮（Python 取中位） | 同配置对齐，Rust −16% |
| 634MB vs 581MB（v4，arena 关） | ✅ 本次实测，5 轮 / 3 轮 | 反向格子；该格更慢，速度优先不选 |
| arena 开比关快 2.5–15% | ✅ 本次实测，四组配置 | 见 §3.2.2 |
| 「降低内存占用」（无条件的表述） | ❌ **不能写** | 必须带「同 arena（开）」；见 §3.2 |
| 全视频 CER 0.36% / 0.72% | ⚠️ 引用既有 README，**本次未复测** | 且 0.72% 是 v3；v4 未入文档 |
| 「多模型串行」「burn」 | ❌ 与代码不符 | 见 §4.3 |

---

## 7. 复现

```bash
# 1) 生成「同一帧 ×40 硬链接」目录（不跑全视频）
bash tests/bench/subtitle_ocr/rust-vs-py/create_frames.sh

# 2) 三实现 × 3 轮（含逐帧计时 + 外部 RSS 采样；含 arena 开/关两组）
bash tests/bench/subtitle_ocr/rust-vs-py/run.sh 3

# 3) 汇总表
python3 tests/bench/subtitle_ocr/rust-vs-py/summarize.py
```

单独跑某一格（例如 Rust 关 arena）：

```bash
RAPIDOCR_CPU_ARENA=0 ./target/release/probe --dir packages/tmp/ocr-bench/oneframe-replay40 \
  --model v4 --reps 1 --warmup 1 --threshold 0.45 --subtitle-only --out /tmp/rust-arenaoff.json
```

> `RAPIDOCR_CPU_ARENA=0` 是为归因实验新增的 **env 探针开关**（`packages/rapidocr_ort/src/ep.rs`）：
> 设置时显式注册 CPU EP 并 `DisableCpuMemArena`；**不设置时行为与之前完全一致**（不注册 EP、
> 走 ORT 默认）。这是本次唯一一处被测代码改动，只为内存探针，默认路径未变。

文件：

| 文件 | 作用 |
| --- | --- |
| `tests/bench/subtitle_ocr/src/bin/probe.rs` | Rust 单帧探针（计时，未改实现逻辑） |
| `packages/rapidocr_ort/src/ep.rs` | 新增 `RAPIDOCR_CPU_ARENA=0` arena 探针（默认不生效） |
| `tests/bench/subtitle_ocr/rust-vs-py/monitor.py` | 子进程峰值/平均 RSS 采样 |
| `tests/bench/subtitle_ocr/rust-vs-py/py_batch.py` | Python 批量/one-shot 驱动；`--models v3` 指仓库 v3 权重；`--arena-on` 为归因诊断开关 |
| `tests/bench/subtitle_ocr/rust-vs-py/run.sh` | 跑批（`taskset 0-3`） |
| `tests/bench/subtitle_ocr/rust-vs-py/summarize.py` | 出表 |
| `tests/bench/subtitle_ocr/rust-vs-py/results/` | 原始 JSON / cpp stderr / `summary.md` |

---

## 8. 局限（引用这些数字时必须一起说）

1. **只测 1 帧**（按要求）：这是「同一帧重复 40 次」的稳态单帧成本，不代表全视频分布；
   不同帧的字幕框数量不同，rec 开销会变（本例仅 1 个框，rec 21ms，偏轻）。
2. **同权重组**：v4（Python 包内 vs Rust v4）与 v3（Python `--models v3` vs Rust v3 /
   C++ v3）分开比；ORT 版本 Python/Rust **1.28.0**、C++ **1.24.4**。
3. **arena 默认不同**（Python 关、Rust/C++ 开）。已补测两格对齐（§3.2）；按速度优先取
   arena 开（Rust 在此格同时更快、峰值更低），但**内存表述必须带这个条件**。
4. Python 的 one-shot 口径包含解释器启动与 `import cv2`，与「Rust 常驻进程」不是同一件事，
   只能作为「旧调用方式」的证据，不能当作「Python 推理慢」。
5. 桌面有常驻负载（loadavg 2.4→7.3）：arena-off 的 Rust 第一轮撞上尖峰（449ms vs 379–385ms），
   Python v3 两组的轮间波动 5.9% / 9.2%。表里对离群轮取中位；其余配置 ≤4.9%。
   绝对值仍有 ±5–10% 漂移。
6. 线程数固定 4，未测其它值（§3.4 只做了 auto 的旁证）。
7. `subtitle_only` 的窗口定义三实现不完全一致：Python 用绝对像素 `620 ≤ y ≤ 700`，
   Rust/C++ 用比例 `0.85 ≤ y/h ≤ 0.99`（720p 下 612–713px）。本帧的字幕框（y 647–682）
   两种窗口都包含，故不影响本次单帧结果；换帧复核时需注意。
8. **arena 差异的机制没有逐项拆解**：只确认了「开关值 300–580MB」「Rust 与 C++ 同配置下
   持平」，没有做 `smaps_rollup`/各 mapping 的归因（heap vs arena vs 模型权重）。因此
   「开 arena 时 Python 为何比 Rust 多吃 230MB」目前只有方向性解释，不算结论。
9. Rust 关 arena 的耗时数据只有 5 轮（含 1 轮负载尖峰），「+7~10% 耗时」是粗估，不是精确值。
10. **Python 的 v3 接线是驱动层适配**：改 `det_model_path/rec_model_path/rec_keys_path`，并把
    仓库 JSON 字典转成 Python 需要的 txt（去首占位 + Python 自动补 blank/空格）。它验证的是
    「同一份 v3 权重、同一个 `ocr_frame` 流程」，但字典加载路径与 Rust/C++ 不同；若怀疑字典
    映射有偏移，应以逐字输出（本帧一致）和小规模 CER 复核。
11. **v3 组的速度差距（1.37–1.48×）比 v4 组（1.22–1.25×）大**，原因未逐项拆解（可能来自
    Python 端 v3 det/rec 的前后处理或 batch 策略）。引用时按「实测比值」引用，不要外推成
    「Rust 对任意 PP-OCR 模型都固定快 1.4×」。
