# 单帧实测：Rust / Python / C++ 字幕 OCR 的耗时与内存

> 目的：给简历里「用 Rust（ort-rs / opencv-rs）替代 Python，降低内存占用和处理耗时」这句
> 业绩找证据，或者证伪它；同时把实际迁移链 `Python → C++ → Rust` 的每一段量化。
>
> 结论先放这：**「快」成立**——同权重同线程（都显式 4 线程）单帧 Rust 比 Python 快
> **1.21×**（v4）/ **1.33×**（v3；跨四次会话 1.18–1.25× / 1.33–1.48×）；相对旧的
> 「每帧起进程 + 重建 3 个 session」调用方式快 **2.27×**。**「省内存」只在写明条件时成立**：
> arena 开（速度最优、也是 Rust/C++ 的默认）这一格 Rust 峰值低 **20–24%**
> （v4 927 vs 1163MB；v3 736 vs 972MB）；arena 关（Python 包内默认）时 v4 组**方向翻转**
> （630 vs 577，Rust 反而高 9%），v3 组基本持平（534 vs 563）。arena 一个开关值 200–590MB，
> 大于两者在多数格子里的残余差。
>
> 速度收益主要来自「常驻 ORT session + 不每帧起进程」，不是 opencv-rs，也不是 Rust 语言本身；
> C++ 是这条链上真实的一站（本次会话 Python→C++ 1.15×、C++→Rust 1.09×、累计 1.26×），
> 不是只用来凑数的一列。
>
> 测量日期：2026-11-14/15 ｜ 只测**同一帧**（按约定不跑全视频），重复采样取 P50/P95。

---

## 0. 被测对象与一句话结论

| 实现 | 入口 | 权重 | 链接的 ORT |
| --- | --- | --- | --- |
| Python | `packages/subtitle_ocr_py/main.py`（`rapidocr_onnxruntime`） | 默认包内 PP-OCRv4；`--models v3` 时指向 `data/models/rapidocr` 的 PP-OCRv3 | venv 内 **1.28.0** |
| Rust | `packages/subtitle_ocr`（`rapidocr_ort` + `subtitle_ocr_post`，`--model v4 / v3`） | `data/models/rapidocr` | `ort 2.0.0-rc.13` 内置 **1.28.0**（静态） |
| C++ | `packages/subtitle_ocr_cpp`（ORT C API 直连） | `data/models/rapidocr`，**写死 v3** | 三个构建：**1.24.4 / 1.28.0 / 1.32.0**（`build` = 1.28.0） |

- **同权重两两比较**：v4 组（Python 包内 vs Rust v4）与 v3 组（Python `--models v3` vs
  Rust v3 vs C++）。Python 包内 v4 的 det/rec **sha256 与 `data/models/rapidocr` 的 v4 完全一致**，
  所以 v4 组不需要额外指路。
- C++ 只能跑 v3，故它只进 v3 组；不与 v4 的 Python 混比（那正是旧文档 1.7× 的错误来源）。
- **C++ 是迁移链的中间站**（§3.1、§6 候选 D）：v3 组同 arena 开，Python→C++ **1.15×**、
  C++→Rust **1.09×**，累计 **1.26×**（上一次全量会话为 1.17× / 1.14× / 1.33×）。

一句话（4 线程、同一帧、常驻 session、同一会话连跑 3 轮）：

- v4：Python **442ms** → Rust **365ms**（**1.21×**）；开 arena 对齐后 445 → 365（**1.22×**）。
- v3：Python **309ms** → C++ **254ms** → Rust **233ms**（1.22× / 1.09×，累计 **1.33×**）；
  开 arena 对齐后 **293 → 254 → 233ms**（两段 **1.15× / 1.09×**，累计 **1.26×**）。
- 旧的 Python 调用方式（每帧一个进程、每帧重建 3 个 ORT session）：**827ms/帧**；Rust 常驻
  session **365ms/帧**，**2.27×**。这 2.27× 里有 ~385ms/帧 是进程+session 重建，不是推理。
- **这 1.2–1.3× 也不是「语言快」**：三个实现调的是同一个 ORT 的同一份 C++ 内核，arena 对齐后
  det 推理差 ≤2%。差距是 Python 每帧多做的宿主侧搬运——numpy det 归一化 ~36ms（每帧 ~115MB
  临时数组）、逐框 crop/resize、det→rec 切换的首帧惩罚 ~34ms，合计 ~76ms（逐阶段拆解见 §4.6）。
- 内存（**速度优先的唯一配置 = arena 开**）：v4 峰值 **927 vs 1163**（Rust −20%）；
  v3 峰值 **736 vs 972**（Rust −24%）。arena 关时 v4 符号翻转（630 vs 577），详见 §3.3。

> 绝对值在四次独立会话里有 ±10% 级别的漂移，但同会话内比值只漂 ±3%；本文一律给
> 「本次会话值 + 跨会话范围」。简历里请用保守下限（v4 1.2×、v3 1.3×），不要用最有利的那一次。

---

## 1. 实验条件

| 项 | 值 |
| --- | --- |
| CPU | AMD Ryzen 7 H 255 w/ Radeon 780M（8 物理核 / 16 逻辑核，`lscpu`） |
| GPU | 无独显；Radeon 780M 集显**未使用**，三个实现都是 `CPUExecutionProvider` |
| 内存 | 27.8 GB |
| OS / 内核 | Linux x86_64，KDE Wayland 桌面 |
| CPU 绑定 | `taskset -c 0-3`（固定 4 个 CPU；`nproc` 16） |
| ORT 线程 | **三个实现都显式钉 4**：cpp `--threads 4`；python `intra_op_num_threads=4`；rust `RAPIDOCR_INTRA_THREADS=4`（新探针，见 §2、§3.5） |
| 视频 | `tests/bench/subtitle_ocr/ref/video_source.mp4`，1280×720，30fps，170.06s，sha256 `34431682…76908`（与 README 记录一致） |
| 帧 | **1 张**：`0020000.jpg`（t=20.0s，1280×720，sha256 `da96093b…`），由 bench 的 2fps 抽帧产出 |
| 采样方式 | 该帧硬链接成 40 个文件名（同一 inode），供「常驻 session 的批量进程」产生 40 个逐帧样本 |
| OCR 参数 | `text_score/text_confidence_threshold=0.45`，`subtitle_only`，`bottom_only`（下 40% ROI） |
| 预热 | rust 预热 1 个完整 pass（40 帧）；python 批量预热 2 帧；cpp 无显式预热（首帧冷启动 +25ms，摊到 40 帧约 +0.6ms） |
| 连跑 | 每个配置 3 轮（python one-shot 每轮 5 次），逐帧样本合并后取 P50/P95 |
| 负载 | 主表会话：跑前 loadavg 4.2 → 跑后 5.9（桌面有 Chrome/Zed/QQ 常驻，且前几轮测量自身也计入）；**逐轮波动仅 0.8–4.1%，见 §3.6** |
| ORT 版本 | Python venv **1.28.0**；Rust `ort 2.0.0-rc.13` 内置 **rel-1.28.0**；C++ 三个构建 **1.24.4 / 1.28.0 / 1.32.0**（§3.2） |

> Rust 与 Python 的 ORT 是**同一版本 1.28.0**，两者对比不受 ORT 版本影响。
> C++ 的默认 `build` 现在也是 1.28.0，所以「C++→Rust 快 1.09×（同版本 1.15×）」不再是
> 跨版本结论（§3.2）。

---

## 2. 口径（这次必须写死，之前踩过坑）

| 口径 | 定义 | 本文是否使用 |
| --- | --- | --- |
| **单帧处理耗时**（本表主指标） | 进程内逐帧计时：读图解码 + det 推理 + det 后处理 + cls + rec 推理 + 聚合。**不含**进程启动、模型/session 加载、抽帧 | ✅ 主表 |
| 引擎初始化 | `engine_init_ms` = 建 3 个 ORT session + 载权重，只发生一次 | ✅ 单列 |
| 每帧墙钟（`--dir` 的 wall ÷ 帧数） | 批量进程总墙钟 ÷ 帧数，含模型加载摊薄、含 stdout 输出 | ❌ 不用（正是旧文档 355ms/590ms 的来源，两者口径并不完全一致） |
| 端到端含解码 | 进程启动 → 抽帧 → OCR → 退出 | ❌ 不用（抽帧属驱动层） |

具体到每个实现，逐帧数字是这样取的：

- **cpp**：它把 `detMs / postMs / recMs / imageLoadMs` 逐帧打到 stderr（JSON 结果走 stdout）。
  本文的逐帧耗时 = `imageLoadMs + detMs + postMs + recMs`，**剔除 `modelLoadMs`**（该字段被
  每帧重复写一遍，所以它的 `totalMs` 会虚高 ~110ms/帧，不能直接用）。
- **rust**：探针 `tests/bench/subtitle_ocr/src/bin/probe.rs`，对 `subtitle_ocr::ocr_entry`
  调用前后 `Instant::now()`（该函数正是 CLI `--dir` 的单帧流程）。**未改被测实现逻辑**。
- **python**：`ocr_frame()`（`main.py` 原函数）调用前后 `perf_counter`，含 `cv2.imread`；
  同时记录它内部自报的 `inference_ms`（仅 `engine(roi)`）。两者差 ~4ms，即读图成本。

**线程数必须钉死（本次新增的两处探针）**：

- Rust 侧 `packages/rapidocr_ort/src/lib.rs` 新增 `RAPIDOCR_INTRA_THREADS=N`：设置时
  `SessionBuilder::with_intra_threads(N)`，**不设置时行为不变**（走 ORT 默认线程池）。
- 这条不是洁癖：同一二进制、同一帧，**不设该变量实测 638ms/帧，设 4 实测 366ms/帧**（1.74×）。
  ORT 默认全局线程池在本机（16 逻辑核）宽到 16 线程，放进 `taskset -c 0-3` 就是超订。
  旧版本「rust 走 ORT 默认（= 可见物理核 4）」的假设不成立，详见 §3.5。

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
> 主表 = 2026-11-15 11:52 那次全量会话（cpp 默认目录切到 ORT 1.28.0 之后唯一一次
> 三实现同会话测量，轮间波动 0.8–4.1%，是四次会话里最稳的）。`summary.md` 的「峰值 RSS」
> 列取各轮 **max**；本文正文取 **3 轮中位** 并列出各轮值，两者差异 ≤1%
> （例外：Rust v4 arena 关，671 vs 中位 630）。

### 3.1 单帧处理耗时（ms，4 线程，同一帧）

| 实现 | 权重 | 样本 n | avg | P50 | P95 | min | max |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| **Rust**（`--model v4`，arena 开） | v4 | 120 | **364.9** | **363.7** | 381.4 | 346.9 | 403.1 |
| **Python**（批量，常驻 engine，arena 关＝默认） | v4 | 120 | **441.7** | **441.7** | 466.6 | 402.8 | 479.4 |
| **Python**（每帧一个进程） | v4 | 15 | **826.6** | 828.0 | 849.8 | 799.4 | 862.9 |
| **Rust**（`--model v3`，arena 开） | v3 | 120 | **232.6** | **230.6** | 247.8 | 213.3 | 270.1 |
| **Python**（`--models v3`，arena 关＝默认） | v3 | 120 | **309.2** | **307.8** | 338.6 | 271.1 | 351.5 |
| **C++**（ORT 1.24.4，`--threads 4`，arena 开） | v3 | 120 | **253.9** | **253.5** | 276.6 | 227.8 | 311.2 |
| C++（ORT 1.28.0，同源码，默认构建） | v3 | 120 | 268.3 | 267.5 | 289.3 | 240.7 | 299.9 |
| C++（ORT 1.32.0，同源码） | v3 | 120 | 260.9 | 260.3 | 285.9 | 227.4 | 309.5 |

> arena 关的耗时不单列在此表（属 §3.3 的同配置实验）：Rust v4 378.3ms、Rust v3 242.3ms；
> Python 两行已在上表（包内默认关）；开 arena 的 Python 两行见 §3.3 的 2×2。

派生比值（同权重、同线程口径）：

| 对比 | 比值 | 说明 |
| --- | ---: | --- |
| Rust v4 / Python v4（批量，各自默认） | **1.21×** | 同权重同线程的「纯单帧处理」差距 ✅ |
| Rust v4 / Python v4（同 arena 开） | **1.22×**（445→365） | 速度最优格的对齐口径 |
| Rust v4 / Python v4（每帧起进程） | **2.27×** | 旧基准的调用方式，差距主要来自进程与 session 重建 ✅ |
| Rust v3 / Python v3（批量，各自默认） | **1.33×** | 同权重同线程 |
| Rust v3 / Python v3（同 arena 开） | **1.26×**（293→233） | 迁移链累计值 |
| C++ v3 / Python v3（同 arena 开） | **1.15×** | 迁移第一段：293 → 254ms（ORT 1.24.4）|
| Rust v3 / C++ v3（同 arena 开） | **1.09×** | 迁移第二段：254 → 233ms |
| C++ v3 / Python v3（同 arena 开，**同 ORT 1.28**） | 1.09× | 293→268 的版本差见 §3.2；同版本下这一段被 ORT 版本吃掉 |
| Rust v3 / C++ v3（**同 ORT 1.28**） | **1.15×** | 排除 ORT 版本后的语言/进程模型净差 |
| C++ v3 / Python v4 | 1.74× | **不同权重**，不能当迁移业绩引用（旧文档的 1.7× 属于这一栏） |

跨会话漂移（同脚本、同机器、不同时间的四次 3 轮连跑）：

| 会话 | Rust v4 / Python v4 | Rust v3 / Python v3 | 备注 |
| --- | ---: | ---: | --- |
| 上一版文档（11-14 夜） | 1.25×（447→358） | 1.48×（327→222） | rust 线程未被显式钉住（结果仍等价 4 线程） |
| 会话 A（11-15 11:05） | 1.21×（439→365） | 1.33×（306→229） | 首次全部显式 4 线程 |
| 会话 B（11-15 11:31） | 1.18×（418→353） | 1.40×（301→214） | cpp 仍是 ORT 1.24.4 默认目录 |
| **会话 C（11-15 11:52，主表）** | **1.21×（442→365）** | **1.33×（309→233）** | 三实现同会话，轮间波动最小 |
| **范围** | **1.18–1.25×** | **1.33–1.48×** | 简历用下限 1.2× / 1.3× |

引擎初始化（只发生一次）：

| 实现 | 初始化耗时 |
| --- | ---: |
| Rust v4 | 133–141ms（本会话 135–136） |
| Rust v3 | 109–117ms（本会话 109–112） |
| Python `import main`（含建 3 session） | 134–152ms（另加解释器启动 ~12ms、`import cv2` 90–130ms；早先会话实测） |
| C++ 模型加载（ORT 1.24.4 / 1.28.0 / 1.32.0） | 115 / 101 / 82ms |

**前两个实现的 session 初始化成本其实差不多（~110–140ms）。** 差距不在「Rust 建 session 更快」，
而在 Python 的旧调用路径**每帧都重付这 ~130ms，外加解释器启动和 `import cv2`**。
C++ 换 ORT 1.32 后模型加载快 29%（82 vs 115ms），这是一次性收益，量级远小于单帧耗时（§3.2）。

### 3.2 C++ 的 ORT 版本：1.24.4 / 1.28.0 / 1.32.0（同源码、只换链接库）

同一份 `ocr_pipeline.cpp`，三个独立构建目录各自 `-DORT_DIR=...` 指到 `packages/tmp/` 下解压的
ORT；每轮内三个构建**轮换顺序**（否则「先跑的吃亏」会和负载漂移混在一起）。
默认目录 `build` 现在指向 **1.28.0**（与 Rust/Python 同版本），`build-ort124` / `build-ort132`
只作实验基线。

| 构建 | 单帧 avg | P50 | P95 | 读图 | det | det 后处理 | rec | 模型加载（一次） | 峰值 RSS | 线程峰 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ORT 1.24.4 | **253.9** | 253.5 | 276.6 | 3.7 | 227.8 | 1.5 | 21.0 | 115ms | 761 | 13 |
| ORT 1.28.0 | 268.3 | 267.5 | 289.3 | 4.3 | 240.5 | 1.4 | 22.1 | 101ms | 763 | 13 |
| ORT 1.32.0 | 260.9 | 260.3 | 285.9 | 4.4 | 233.5 | 1.5 | 21.6 | **82ms** | 770 | **17** |

- **单帧吞吐没有变快**：1.28.0 比 1.24.4 慢 **+5.7%**、1.32.0 慢 **+2.8%**（另两次会话是
  +2.3~4% / +0.6~1%）。三轮内 1.28 每一轮都比同轮 1.24.4 慢（配对差 **+13~16ms**），
  方向稳定；幅度随会话在 2–6% 之间。
- **内存基本不动**：峰值 761 / 759 / 768MB（1.32 高 1.2%），均值 734 / 734 / 741MB。
- **一次性初始化确实变快**：115 → 101 → **82ms**（1.32 比 1.24.4 −29%），与 1.31/1.32 发布说明里
  「会话初始化/预打包、大图 init 加速」的方向一致；但这段只跑一次，摊到 40 帧是 0.8ms/帧。
- **1.32 会开更多线程**：线程峰值 17（1.24/1.28 均 13），在 4 CPU 的 taskset 里没有带来收益。
- 结论：**升级 ORT 不是提速手段**。要升就升到 1.32.0 拿初始化收益（以及安全修复），
  不要指望单帧变快；基准里保留 1.24.4 那一列作为迁移当时的历史基线。

### 3.3 内存与 arena：**按「速度优先」选配置后，Rust 同时更快且更低**

先回答「开关不同有没有影响速度」——四组里三组 arena 开更快（3.7–5.5%），Python v4 是
−0.7%（噪声级），详见 §3.3.2。模型运行时速度优先，所以**该选的唯一配置就是 arena 开**；
这恰好也是 Rust/C++ 的默认、Python 需要 patch 才能到达的配置。全表（每格 3 轮，列各轮峰值）：

| 实现 | 权重 / ORT arena | 峰值 RSS（中位） | 各轮峰值 | 平均 RSS | 备注 |
| --- | --- | ---: | --- | ---: | --- |
| Rust v4 | v4 / **开**（ORT 默认） | **927** | 926/927/928 | 914 | 稳定 |
| Rust v4 | v4 / **关**（`RAPIDOCR_CPU_ARENA=0`） | 630 | 671/630/630 | 463 | 轮间 6.5% |
| Python v4 | v4 / **关**（包内默认） | 577 | 577/581/577 | 372 | 稳定 |
| Python v4 | v4 / **开**（诊断 patch） | **1163** | 1163/1163/1163 | 1029 | 稳定 |
| Rust v3 | v3 / **开** | **736** | 754/735/736 | 723 | 稳定 |
| Rust v3 | v3 / 关 | 534 | 534/526/535 | 358 | 稳定 |
| Python v3 | v3 / **关**（包内默认） | 563 | 562/591/563 | 340 | 轮间 5% |
| Python v3 | v3 / **开** | **972** | 974/972/903 | 835 | 轮间 7% |
| C++ v3（1.24.4） | v3 / **开** | 761 | 761/756/761 | 734 | 稳定 |
| C++ v3（1.28.0） | v3 / **开** | 759 | 759/759/763 | 734 | 稳定 |
| C++ v3（1.32.0） | v3 / **开** | 768 | 766/770/768 | 741 | 稳定 |
| Python v4（每帧一进程） | v4 / 关 | 511 | 511/510/511 | — | 15 个进程的最大峰值 |

#### 3.3.1 速度优先口径（arena 开，同权重对齐）

| 权重 | 实现 | avg 单帧 | P50 | 峰值 RSS | 平均 RSS | Rust 相对 Python |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| v4 | Rust | **364.9ms** | 363.7 | **927** | **914** | 快 **1.22×**；峰值 **−20%**；均值 **−11%** |
| v4 | Python | 444.6ms | 443.4 | 1163 | 1029 | — |
| v3 | Rust | **232.6ms** | 230.6 | **736** | **723** | 快 **1.26×**；峰值 **−24%**；均值 **−13%** |
| v3 | Python | 293.1ms | 292.8 | 972 | 835 | — |

这就是「同内存分配配置下比较」的正确做法，而且**结论对 Rust 有利且可复现**：两套权重
都指向同一方向（快 1.22–1.26×、峰值低 20–24%）。区别只在幅度，不在符号。
Python v3 的 arena 开峰值三轮是 974/972/903，若取最有利的 903，则差距缩到 −19%（仍同向）。

#### 3.3.2 arena 开关本身的速度影响（同实现内比较）

| 实现 / 权重 | arena 开 | arena 关 | 开比关快 |
| --- | ---: | ---: | ---: |
| Rust v4 | 364.9ms | 378.3ms | **+3.7%** |
| Rust v3 | 232.6ms | 242.3ms | **+4.2%** |
| Python v4 | 444.6ms | 441.7ms | **−0.7%**（≈ 噪声） |
| Python v3 | 293.1ms | 309.2ms | **+5.5%** |

结论：**四次会话里 arena 开从未显著更慢**（本次 −0.7% ~ +5.5%，上一版会话 0.5–15%）。
所以速度优先时没有理由选 arena 关；但幅度会跨会话漂移，别把它写成一个「关键加速项」。
arena 真正的代价/收益在**内存**：200–590MB（下表），比速度效应大一个量级。

#### 3.3.3 仍然要写明的反向格子

如果强行对齐到 Python 的包内默认（arena 关），v4 的内存符号会翻转：

**同配置 2×2（v4，4 线程，同一帧）：**

| | arena **开**（速度最优 + Rust 默认） | arena **关**（更省内存 + Python 默认） |
| --- | --- | --- |
| Rust v4 | **365ms** / 峰值 927 / 均值 914 | 378ms / 峰值 630 / 均值 463 |
| Python v4 | 445ms / 峰值 1163 / 均值 1029 | 442ms / 峰值 577 / 均值 372 |
| Rust 相对 Python | **快 1.22×；峰值 −20%；均值 −11%** | 快 1.17×；峰值 **+9%**；均值 **+24%** |

- 对齐到 arena 关：**v4 组** Rust 峰值高 9%（630 vs 577），均值高 24%（463 vs 372，且
  Rust 的 arena 关峰值轮间 6.5%，取 630 已是最有利的中位）。这是「同配置」下对 Rust 不利的
  格子，也是 Python 的产品默认。**v3 组没有翻转，也没有优势**（534 vs 563 峰值，Rust 低 5%；
  均值 358 vs 340，Rust 高 5%），说明连「翻转」本身都依赖权重版本与取法，不能当普遍规律。
- 机制上：arena 开关值 **200–590MB**（Rust v4 927→630、Rust v3 736→534、Python v4
  1163→577、Python v3 972→563），在 v4 上远大于两者的残余差（约 240MB）。
  Python 从关 arena 获益更大，足以翻转符号。**所以任何内存表述都必须带 arena 条件。**

**旁证：Rust 与 C++ 在同一配置（arena 开、v3）下几乎相同（736 vs 759~761MB，均值 723 vs 734）。**
说明内存由 ORT arena + 模型/激活占用决定，宿主语言只值几十 MB（裸 Python 13MB、
`import onnxruntime` 峰值 46MB），对比 900MB 级占用是噪声。

> 结论：**在速度优先的同配置口径（arena 开）下，「Rust 更快且峰值更低」成立**
> （v4 1.22×/−20%，v3 1.26×/−24%）。若把 Python 的包内默认（arena 关）当基准，
> v4 内存结论翻转且两者都更慢——那是「配置选择」的差异，不是语言差异。

### 3.4 识别质量（本次只做单帧一致性）

按约定只跑一帧，**不做全视频 CER**。这一帧上各实现逐字一致：

| 实现 | 权重 | 输出 | 3 轮是否一致 |
| --- | --- | --- | --- |
| Rust | v4 | `不对醒来` | ✅ 3/3 相同（arena 开/关都是） |
| Python | v4 | `不对醒来` | ✅ 3/3 相同 |
| Rust | v3 | `不对醒来` | ✅ 3/3 相同（arena 开/关都是） |
| Python | v3 | `不对醒来` | ✅ 3/3 相同（arena 开/关都是） |
| C++ | v3 | `不对 醒来`（两个框以空格连接，字符相同） | ✅ 3/3 相同（40 帧 × 3 轮） |
| C++ | v3（ORT 1.28.0 / 1.32.0） | `不对 醒来` | ✅ 各 3/3 相同（见 `results/cpp-text-check.txt`） |

全视频质量数字不在本次重测范围内，引用既有基准（**口径与本文不同，别和上表混用**）：

- `tests/bench/subtitle_ocr/README.md`：rust（v3，轴对齐）`CER(norm)=0.72% / CER(paired)=0.18% /
  spurious 4`；rust（v3 + `--warp-crop`）与 cpp **逐位一致**：`0.36% / 0.36% / 0 spurious /
  0 missed`。
- 注意：README 里的 rust 数字是 **v3** 权重；v4 的全视频 CER 在 `packages/tmp/summary_new_r*.json`
  里是 `CER(norm)=1.25% / spurious 5`（未入文档、未在本次复测），引用时需另行确认。
- 本次新测的 Python v3 与 Rust v3 只验证了**单帧输出一致**，不等于全视频 CER 相同。

### 3.5 线程数：ORT 默认线程池是个坑（本次新增）

同一二进制、同一帧、同样 `taskset -c 0-3`，只改 intra-op 线程数（`results/threadpool-check.txt`）：

| 配置 | 线程峰值 | avg/帧 | 相对 4 线程 |
| --- | ---: | ---: | ---: |
| Rust v4，`RAPIDOCR_INTRA_THREADS=4`（本文口径） | 13 | **366.2ms** | — |
| Rust v4，`RAPIDOCR_INTRA_THREADS=8` | 25 | 378.9ms | +3% |
| Rust v4，不设（ORT 默认全局池） | 25 | **637.7ms** | **+74%** |
| Python v4，4 线程 | 19 | 441.7ms | — |
| Python v4，`intra=-1`（不绑定，早先一轮 40 样本） | 67 | 427.9ms | +2% |
| C++ v3，`--threads 4` | 13 | 253.9ms | — |

> Rust 三者与 Python `intra=-1` 来自同一批 `threadpool-check.txt` / 早先 `py-v4-autothreads` 实测；
> Python 4 线程与 C++ 行取自主表（§3.1），仅作对照。

**这是本次最值得写进工程结论的一条**：ORT 的默认线程池在 16 逻辑核机器上会开到 16 个
intra-op 线程，放进 4 CPU 的 cpuset 就是严重超订，单帧劣化 **1.74×**（638 vs 366ms）。
上一版文档「rust 走 ORT 默认 = 可见物理核 4」是错的——旧数据的 358ms 只是碰巧等价于 4 线程
（不同时间点时默认池的实际宽度不同），同一命令今天跑出来是 638ms。**所以三个实现都必须
显式钉线程数**，否则数字不可复现。

线程加多没有收益（Rust 8 线程慢 3%，Python 不绑定只快 2%），与 `justfile` 里
「16 线程反而更慢、rec 劣化 5 倍」的既有结论方向一致。**故主表统一固定 4 线程。**

### 3.6 波动

| 配置 | 三轮均值 | 轮间波动 |
| --- | --- | ---: |
| Rust v4 | 362.9 / 365.6 / 366.1 ms | 0.9% |
| Rust v4（arena 关） | 376.8 / 378.1 / 380.1 ms | 0.9% |
| Rust v3 | 231.2 / 237.8 / 228.9 ms | 3.9% |
| Rust v3（arena 关） | 241.1 / 242.2 / 243.6 ms | 1.1% |
| Python v4 | 438.8 / 445.0 / 441.4 ms | 1.4% |
| Python v4（arena 开） | 446.6 / 450.8 / 436.3 ms | 3.3% |
| Python v3（arena 关） | 308.2 / 313.9 / 305.5 ms | 2.8% |
| Python v3（arena 开） | 290.7 / 296.7 / 292.0 ms | 2.1% |
| Python one-shot | 824.5 / 824.4 / 831.0 ms（各 5 次） | 0.8% |
| C++ v3（ORT 1.24.4） | 251.0 / 257.5 / 253.3 ms | 2.6% |
| C++ v3（ORT 1.28.0） | 266.7 / 271.4 / 266.7 ms | 1.8% |
| C++ v3（ORT 1.32.0） | 256.6 / 267.2 / 258.8 ms | 4.1% |

主表这次会话的轮间波动只有 **0.8–4.1%**（三轮起始 loadavg 4.2 / 5.7 / 6.6，其中大部分是本次测量
自身贡献的），比 11:05 / 11:31 两次会话（9–21%，其中一次还遇到 loadavg 10.6 的尖峰）
明显更稳，所以选它当主表。上表用三轮合并的 120 个样本，P50 与单轮均值基本重合。

**结论是比值比绝对值稳**：跨会话绝对值漂 ±10%（例如 Rust v4 353–365ms、Python v4 418–442ms），
但同会话内 Rust/Python 比值只漂 ±3%。这也是简历里该用下限的原因。

---

## 4. 归因：1.2× / 2.3× 到底是谁的功劳

### 4.1 「Python 每帧重建 ORT session」是真的吗？

**分两种调用方式：**

- **在一个进程里跑**：`main.py` 顶层 `engine = RapidOCR()` 只建一次，det/cls/rec 三个 session
  常驻。这时 Python 没有「每帧重建」，单帧 442ms。
- **每帧起一个进程**（旧基准 `bench` 的调用方式，也是 `subtitle_ocr_py` 作为 CLI 的用法）：
  **每帧重建 3 个 session**，外加解释器启动、`import cv2`、无预热。单帧 827ms。

差值 **827 − 442 = 385ms/帧**，与「`import main` ~340ms + 冷启动首次推理比预热后慢 ~50ms」
基本对得上。**旧文档 1.7× 的差距，主因就是这个调用方式差异，不是 Python 推理慢 1.7 倍。**

Rust/C++ 天然是「一个进程、session 常驻、批量跑目录」，所以在这个对比里占便宜。
这不是语言优势，是**进程模型**优势。要公平比较「同权重同线程的纯推理」，就是
v4 442 vs 365（1.21×）、v3 309 vs 233（1.33×）。这 1.2–1.33× 又是怎么来的，
见 §4.6 的逐阶段拆解。

**session 重建对内存没有影响。** 峰值 RSS 是「一个进程内看到的最大常驻集」，不跨进程累加：
Python 每帧起进程的峰值是 **511MB**，反而比常驻进程的 577MB 略低（因为每帧只跑一次、
没有预热后的缓存增长）。所以「每帧重建 session」解释的是**耗时**，解释不了「省内存」；
拿它去支撑「降低内存占用」是错的。

### 4.2 opencv-rs 的贡献

cpp 的分相实测（同一帧、4 线程、ORT 1.24.4）：

| 读图 | det 推理 | det 后处理（含 NMS/unclip） | rec 推理 | 模型加载（一次） |
| ---: | ---: | ---: | ---: | ---: |
| 3.7ms | 227.8ms | **1.5ms** | 21.0ms | 115ms |

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

### 4.4 内存：收益属于「arena 配置」，但速度优先时 Rust 恰好把头也占了

把 arena 拉平后（§3.3）可以确认三件事：

1. **arena 开关值 200–590MB**（Rust v4 927→630，Python v4 1163→577；v3 两组 736→534 /
   972→563）。在 v4 上这远大于两者的残余差，想单独压内存，第一优先级是关 arena，不是换语言。
2. **arena 开在四次会话的所有配置里都没有显著更慢**（本次 −0.7% ~ +5.5%，上一版 +0.5% ~ +15%）。
   速度优先 ⇒ 选 arena 开。
3. 在 arena 开这一格，**Rust 同时更快且峰值更低**：v4 1.22×/−20%，v3 1.26×/−24%。
   所以「更快 + 更省」可以在**同一句里**成立，前提是写明「同 arena 配置（开）」。
   拆成迁移链看更清楚：v3 的峰值内存几乎全部在 Python→C++ 这一步降掉（972→761MB，
   −22%），C++→Rust 再低 3%（761→736MB，均值 734→723 基本持平）；Rust 这一步的增量是
   再快 9% 与工程性（内存安全、与仓库其余 Rust 组件同构），不是又一次大幅降内存。

反过来说：如果线上必须沿用 Python 的包内默认（arena 关），Rust 的内存优势会消失甚至翻转
（v4 630 vs 577，Rust 高 9%）。而那一格单帧也更慢 3.7%，按「模型运行速度优先」不该作为目标配置。

**给仓库的可执行建议**（本次未改默认，避免影响既有基准）：

- 常驻内存吃紧时，才把 `packages/rapidocr_ort/src/ep.rs` 的 CPU 分支默认改成 arena 关——
  实测峰值 927→630MB（−32%），单帧 365→378ms（+3.7%）。这是「省内存换 4% 速度」的开关，
  与上一版结论（+7% 耗时）方向一致、幅度变小；仍建议保留为可选开关而不是默认。
- **更重要的是把 intra-op 线程数显式设进 `rapidocr_ort`**（而不是留给 ORT 默认池）：
  默认池在本机会多开 12 个线程并把单帧从 366ms 拖到 638ms。这是比换语言、换 ORT 版本
  都更值钱的一行配置（§3.5）。

### 4.5 ORT 版本升级值不值（1.24.4 → 1.28.0 / 1.32.0）

近半年 ORT 的发版节奏（GitHub releases，截至 2026-11-15）：1.25.0/1.25.1（4 月）、
1.26.0（5 月）、1.27.0/1.27.1（6–7 月）、1.28.0–1.28.3（7–10 月）、1.29.0/1.29.1（8–9 月）、
1.30.0/1.30.1（9–10 月）、1.31.0/1.31.1（10 月）、**1.32.0（2026-11-13，最新）**。
与「CPU 单帧推理」相关的条目：

- **1.31/1.32**：CPU session 初始化与预打包（prepacking）加速、大图 init 提速；
  1.31 起有 x86 FP16 LayerNorm/RMSNorm 内核。
- **1.30**：CPU LinearAttention 优化、AVX2 LayerNorm/RMSNorm。
- **1.29**：以遥测/安全修复为主。
- **1.28**：内存安全与模型加载加固、ONNX 1.22。本仓库 Python/Rust 用的正是这一版。
- **1.32**：重新集成 ONNX 1.23.2（opset 28），MoE 相关改动与 CPU 单帧无关。

实测结论（§3.2）：**升级不会让我们的两阶段 OCR 变快**（+2.8%~+5.7%，方向稳定为略慢），
内存基本不动（+1%），唯一可测收益是**一次性模型加载 −29%**（115→82ms），
与 changelog 里「session init 加速」对得上。原因也好理解：本工作负载是 det（294×1280→
736×3264）上的大卷积，瓶颈在卷积内核与内存带宽，不在 session 初始化/图优化。

**建议**：C++ 若要动，就从 1.24.4 直接到 **1.32.0**（拿安全修复 + init 收益），
并在文档里保留「单帧无提速」的事实；不要为了速度升级，更不要把 ORT 版本差算进
「C++→Rust 快 1.09×」里——同版本 1.28.0 下这一比值是 **1.15×**，反而更大。

### 4.6 逐阶段拆解：多出来的 76ms 是什么，「是不是语言」的答案（本次新增）

§4.1 解释了 2.27× 的绝大部分（进程模型）。剩下的 1.2–1.33× 常被简化成「Rust 比 Python 快」，
但 ORT 的卷积/GEMM 是同一份预编译 C++ kernel，**Python 进程里跑的也是它**。所以先把同一帧
按阶段拆开（同一会话、4 线程、每阶段 20 次重复 × 3 轮取中位；loadavg 2.3→5.5，其中大部分是
测量自身贡献的。绝对值比 §3.1 主表低 1–3%，比值不受影响）：

| 阶段（单帧） | Python v3 | Rust v3 | 差 | Python v4 | Rust v4 | 差 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| det 预处理（resize + 归一化 + CHW） | 40.1 | **14.4** | −25.7 | 40.6 | 14.5 | −26.1 |
| det 推理 + DB 后处理 | 219.9 | 202.0 | −17.9 | 349.7 | 332.9 | −16.8 |
| cls + 裁剪/resize + rec | 41.7 | **7.9** | −33.8 | 46.2 | 12.7 | −33.5 |
| **engine/detect 合计（不含读图）** | **301.0** | **225.1** | **−75.9** | **436.9** | **361.1** | **−75.9** |

> Rust 的 `detect` 不含读图 2.6ms 与 ROI 拷贝 0.1ms；加上后整帧 227.8 / 363.8ms，与主表
> 232.6 / 364.9ms 一致。Python 的 `engine(roi)` 本身不含 `cv2.imread`（3–4ms/帧）。
> 各阶段在三组独立循环里各测 20 次，故三项差值之和（77.4 / 76.4）与合计差（75.9）差 1–2ms；
> 以合计行为准。测量方式见 §7 的新增探针。

三句话：

1. **det 推理本身几乎一样。** 把 Python 的 det 推理单独计时（输入预热、20 次平均，本次会话）：
   arena 关 217.3ms（p50 216.6）/ arena 开 **204.1ms**（p50 200.6）；Rust 的 det 推理 + 后处理
   202.0ms（后处理 ~2ms）。arena 对齐后 v4 甚至出现反向（Python 329.1 vs Rust ~331，即
   332.9 减掉 ~2ms 后处理），v3 剩 2%。**语言在这里没有可测贡献**——两边都是同一个
   `onnxruntime` 1.28.0 的 CPU EP。
   （arena 关时多出的 13ms 是 §3.3 的配置差，不是 numpy 的锅。）
2. **det 预处理是纯 numpy 搬运。** Python 40.1ms 的内部拆解（3 轮中位）：`cv2.resize` 0.6ms、
   `normalize()` **35.7ms**、`permute` 0.0ms、`expand_dims + astype` 3.0ms。`normalize` 就是
   `(img.astype("float32") * scale - mean) / std`：对 736×3264×3 的输入，每一步都新建一个
   28.8MB 的全尺寸临时数组，一帧要分配并读写 ~115MB。Rust 的 `resize_bilinear_hwc` +
   `normalize_chw` 一趟写进 CHW，14.4ms。**这一项 26ms ≈ 总差距的 34%。**
3. **cls + rec 段差 33–34ms，但不是「Rust 的 rec 推理更快」。** Python 单框 rec 孤立跑 7.8ms，
   紧跟 det 后第一次跑 **23.9ms**、第二次 15.3ms，中间空转 20ms 后回到 9.9ms；C++ 的 `recMs`
   （只包 rec `Run`）同样是 20–26ms，而 Rust 的 cls+rec 在同样「紧跟 det」的条件下是 8–13ms。
   所以这段主要是 **det→rec 切换后的首帧惩罚**（更像 ORT 线程池的 spin/唤醒，而不是缓存：
   中间空转 20ms 反而降到 9.9ms），Python 与 C++ 都付、Rust 付得少；机制未完全归因（列入 §8）。
   **这一项 ~34ms ≈ 总差距的 45%。**

**直接回答「是不是只是编程语言」**：不是——而且「语言」这个变量在模型执行里几乎不存在，
三个实现最终都调 `onnxruntime` 的同一份 C++ 推理内核。真正的差距是**每帧在 Python 侧多做的
宿主数据处理**：numpy 的 115MB 临时数组、逐框 crop/resize、det→rec 切换的首帧惩罚，合计 76ms；
Rust 侧换成一次遍历的 SIMD 预处理 + 常驻 session，于是 301 → 225ms（主表口径 309 → 233ms）。
换句话说，1.33× 的准确表述是「**去掉每帧约 76ms 的宿主侧开销**」，而不是「Rust 算得比
Python 快 1.33 倍」。2.27× 里的 385/462ms 则是进程模型（§4.1），同样与语言无关。

对简历的直接含义：这两个倍率只能挂在「每帧处理耗时 / 进程模型」上，不能写成
「Rust 推理比 Python 快 1.3 倍」，也不该把 ORT 版本、opencv-rs、burn 算进这个倍率。

---

## 5. 结论：哪些说法成立

| 简历原句成分 | 是否成立 | 证据 |
| --- | --- | --- |
| 「用 ort-rs / opencv-rs 替代 Python」 | ✅ 成立 | 代码结构；`rapidocr_ort` 依赖 `ort` + `opencv` |
| 「用 burn 替代…（字幕 OCR）」 | ❌ 不成立 | vision-lab 无 burn 使用；burn 在 vox-lab 语音侧 |
| 「降低处理耗时」 | ✅ 成立 | 同权重同线程：v4 **1.18–1.25×**（442→365ms）、v3 **1.33–1.48×**（309→233ms）；相对每帧起进程 **2.27×**（827→365ms） |
| 「先迁 C++ 再迁 Rust」 | ✅ 成立 | v3 同 arena 开：Python 293 → C++ 254（**1.15×**）→ Rust 233（**1.09×**），累计 **1.26×**；同 ORT 版本下 C++→Rust 为 **1.15×** |
| 「降低内存占用」 | ✅ **条件成立（必须写明细）** | 同权重 + 同 arena（开，且是速度最优格）：v4 峰值 1163→927（**−20%**）、v3 972→736（**−24%**）；⚠️ 若把 arena 关（Python 默认）当基准，v4 翻转为 577→630（Rust **+9%**），v3 基本持平（563 vs 534） |
| 「替代原先 Python 中某些多模型串行的流程」 | ⚠️ 表述含糊 | Python 侧 `engine` 本身就是常驻的 3 模型串行；「每帧重建」只发生在 CLI 每帧起进程的调用方式 |
| 「内存收益主要在 Python→C++ 这一段」 | ✅ 成立（且有量化） | v3 峰值 972→761（−22%），C++→Rust 761→736（−3%）；Rust 这一段换来的是再快 9%、内存安全与可维护性 |
| 「升级 ORT 到最新能提速」 | ❌ 不成立（对本题） | 1.24.4→1.28.0/1.32.0 单帧 +5.7%/+2.8%（略慢），仅一次性初始化 −29% |
| 「Rust 语言本身让推理快 1.2–1.3×」 | ❌ 不成立（本次拆解） | ORT 内核相同；差距来自 numpy 预处理 26ms + det→rec 首帧惩罚 ~34ms + det 推理侧 ~18ms（§4.6） |

**如果只能留一句话：保留「快」；内存可以写，但必须带「同 arena 配置（开）」这个条件；
删掉 burn；把「多模型串行」改成具体的调用方式差异（进程/session 重建）。**

---

## 6. 简历那句该怎么写（可直接粘贴）

### 候选 A（最保守，只写有证据的耗时）

> 把字幕 OCR 从 Python（rapidocr-onnxruntime）迁移到 Rust（ort-rs + opencv-rs）常驻推理：
> 同权重（PP-OCRv4）、同 4 线程下，单帧 OCR 从 442ms 降到 365ms（约 1.2×）；相对原先
> 每帧起进程、重建 3 个 ORT session 的调用方式，单帧从 827ms 降到 365ms（约 2.3×）。

### 候选 B（带上归因，显得可信；适合技术面展开）

> 字幕 OCR 从 Python 迁到 Rust 常驻 ORT session：消除每帧进程启动与 3 个 session 重建，
> 单帧 827→365ms（2.3×）；同权重同线程的纯推理 v4 442→365ms（1.2×）、v3 309→233ms
> （1.3×）。收益来自常驻 session 与批处理，而非语言本身——作为对照，C++ 直连 ORT
> 在同权重下为 254ms（Rust 233ms），同 ORT 版本时 Rust 仍快 1.15×。

### 候选 C（速度优先 + 同 arena 配置；要写内存时用这条，推荐）

> 字幕 OCR 从 Python 迁到 Rust 常驻 ORT session，并把 ONNX Runtime CPU arena 作为对齐项：
> 同权重、同 4 线程、同 arena（开）下，v4 单帧 445→365ms（1.22×）、峰值 RSS 1163→927MB
> （−20%）；v3 单帧 293→233ms（1.26×）、峰值 RSS 972→736MB（−24%）。相对于 Python 同时
> 降低耗时与峰值内存；收益来自常驻 session/批处理与 arena 配置，不是语言本身。
>
> ⚠️ 这句的两个前提要能现场说清：
> 1. 同权重（PP-OCRv4 与 v3 各比各的，C++ v3 不参与 v4 对比）；
> 2. 同 arena 且是**速度最优**的那一格。若 interviewer 追问「arena 关掉再比呢」，
>    答案是 v4 630 vs 577（Rust 反而高 9%），但那一格单帧也更慢 3.7%，按速度优先不选。
> 说不清这两点就退回候选 A/B。

### 候选 D（要写「先 C++ 再 Rust」的迁移链时用这条）

> 字幕 OCR 运行时迁移分两步：先把 Python（rapidocr-onnxruntime）迁到 C++（ONNX Runtime
> C API 直连），再迁到 Rust（ort-rs + opencv-rs）常驻推理。同权重 PP-OCRv3、同 4 线程、
> 同 arena（开）下：单帧 293→254ms（C++，1.15×）→233ms（Rust，再 1.09×，累计 1.26×）；
> 峰值 RSS 972→761→736MB。C++ 阶段完成主要提速与降内存，Rust 阶段在内存持平下再快 9%，
> 并改善内存安全与可维护性。v4 权重下 Python→Rust 为 445→365ms（1.22×）。
>
> ⚠️ 这条的边界：
> 1. **C++ 只支持 v3**（仓库里写死 v3 det/rec），所以「先 C++」只能放在 v3 组讲，不能和
>    v4 的 Python/Rust 数字混在一句里。
> 2. ORT 版本已不再是干扰项：C++ 默认 `build` 就是 1.28.0，**与 Rust/Python 同版本比，
>    C++→Rust 是 1.15×**（比跨版本的 1.09× 还大）。所以「Rust 比 C++ 快」站得住，
>    但仍不要外推成「Rust 语言本身快 15%」——里面含 opencv-rs 预/后处理与调用方式差异。
> 3. C++ 与 Rust 的峰值内存相差只有 3%（761 vs 736MB）：**内存收益属于 Python→C++ 这一步，
>    Rust 这一步换来的是速度、内存安全和可维护性**，不要写成 Rust 又省了一大截内存。
> 4. 两段比值跨会话有 1.15–1.17× / 1.09–1.14× 的波动，累计 1.26–1.33×；写 1.3× 是安全的。

### 数字证据等级

| 数字 | 证据 | 备注 |
| --- | --- | --- |
| 442ms（Python v4，批量，4 线程） | ✅ 本次实测，120 样本，轮间 1.4% | 单帧 `0020000.jpg` |
| 365ms（Rust v4，4 线程，显式钉线程） | ✅ 本次实测，120 样本，轮间 0.9% | 单帧同一图 |
| 309ms（Python v3，默认）/ 293ms（Python v3，arena 开） | ✅ 本次实测，各 120 样本，轮间 2.8% / 2.1% | 同权重、同为仓库 v3 权重 |
| 254ms（C++ v3，arena 开，ORT 1.24.4） | ✅ 本次实测，120 样本，轮间 2.6% | **迁移中间站**；1.28.0 为 268ms、1.32.0 为 261ms |
| 233ms（Rust v3，arena 开） | ✅ 本次实测，120 样本，轮间 3.9% | 同权重同线程 |
| 827ms（Python 每帧一进程） | ✅ 本次实测，15 样本 | 旧 590ms 是另一台机器/另一版本的同类口径 |
| 1.18–1.25× / 1.33–1.48× / 2.27× | ✅ 由上几行相除 | 同口径相除，未混用；范围来自四次独立会话 |
| 1.15×（Python→C++）/ 1.09×（C++→Rust） | ✅ 均由 293/254/233ms 相除 | 同权重、同线程、同 arena（开）；另一会话为 1.17×/1.14× |
| 1.15×（C++→Rust，**同 ORT 1.28**） | ✅ 268 vs 233ms | 排除 ORT 版本后的净差 |
| 927MB vs 1163MB（v4，arena 开） | ✅ 本次实测，3 轮 | 同配置对齐，Rust −20% |
| 736MB vs 972MB（v3，arena 开） | ✅ 本次实测，3 轮 | 同配置对齐，Rust −24%（取 Python 最优轮为 −19%） |
| 761MB（C++ v3，1.24.4）/ 759MB（1.28.0）vs 736MB（Rust v3） | ✅ 本次实测，3 轮 | 两步之间内存差 3%，Rust 未再大幅降 |
| 630MB vs 577MB（v4，arena 关） | ✅ 本次实测，3 轮 | 反向格子；该格单帧也慢 3.7% |
| arena 开比关快 −0.7~+5.5%（上一版 0.5–15%） | ✅ 本次实测，四组配置 | 见 §3.3.2；方向基本一致，幅度不稳 |
| ORT 1.24.4→1.28.0/1.32.0 单帧 +5.7%/+2.8% | ✅ 本次实测，各 120 样本 | 升级不提速；初始化 115→82ms |
| 「降低内存占用」（无条件的表述） | ❌ **不能写** | 必须带「同 arena（开）」；见 §3.3 |
| 全视频 CER 0.36% / 0.72% | ⚠️ 引用既有 README，**本次未复测** | 且 0.72% 是 v3；v4 未入文档 |
| 「多模型串行」「burn」 | ❌ 与代码不符 | 见 §4.3 |

---

## 7. 复现

```bash
# 1) 生成「同一帧 ×40 硬链接」目录（不跑全视频）
bash tests/bench/subtitle_ocr/rust-vs-py/create_frames.sh

# 2) cpp 的三个 ORT 构建（必须显式 -DORT_DIR，否则 CMake 自动挑最新的一份）
#    默认目录 build 对齐 rust/python 的 1.28.0；另两个是实验基线。
cmake -S packages/subtitle_ocr_cpp -B packages/subtitle_ocr_cpp/build \
      -DORT_DIR="$PWD/packages/tmp/onnxruntime-linux-x64-1.28.0"
cmake -S packages/subtitle_ocr_cpp -B packages/subtitle_ocr_cpp/build-ort124 \
      -DORT_DIR="$PWD/packages/tmp/onnxruntime-linux-x64-1.24.4"
cmake -S packages/subtitle_ocr_cpp -B packages/subtitle_ocr_cpp/build-ort132 \
      -DORT_DIR="$PWD/packages/tmp/onnxruntime-linux-x64-1.32.0"
cmake --build packages/subtitle_ocr_cpp/build -j8
cmake --build packages/subtitle_ocr_cpp/build-ort124 -j8
cmake --build packages/subtitle_ocr_cpp/build-ort132 -j8

# 3) 三实现 × 3 轮（含逐帧计时 + 外部 RSS 采样；含 arena 开/关、cpp 三个 ORT 版本）
bash tests/bench/subtitle_ocr/rust-vs-py/run.sh 3

# 只跑一类（例如只重测 cpp 的三个 ORT 版本）
bash tests/bench/subtitle_ocr/rust-vs-py/run.sh 3 cpp

# 4) 汇总表
python3 tests/bench/subtitle_ocr/rust-vs-py/summarize.py
```

单独跑某一格（例如 Rust 关 arena / 改线程数）：

```bash
RAPIDOCR_CPU_ARENA=0 RAPIDOCR_INTRA_THREADS=4 \
  ./target/release/probe --dir packages/tmp/ocr-bench/oneframe-replay40 \
  --model v4 --reps 1 --warmup 1 --threshold 0.45 --subtitle-only --out /tmp/rust-arenaoff.json
```

分阶段归因（§4.6，两次探针都只测、不改被测逻辑）：

```bash
# Rust：preprocess_det / detect_raw_boxes / detect / load_image / roi_copy 各自计时
cargo build --release -p bench_subtitle_ocr --bin phase_probe
taskset -c 0-3 env RAPIDOCR_INTRA_THREADS=4 ./target/release/phase_probe \
  --dir packages/tmp/ocr-bench/oneframe-replay40 --model v3 --reps 20

# Python：det_pre / det_full / engine 三段（与 Rust 同口径）
taskset -c 0-3 packages/subtitle_ocr_py/.venv/bin/python \
  tests/bench/subtitle_ocr/rust-vs-py/py_phase.py v3 20

# Python det 预处理内部拆解（resize / normalize / permute / expand）
taskset -c 0-3 packages/subtitle_ocr_py/.venv/bin/python \
  tests/bench/subtitle_ocr/rust-vs-py/py_det_pre_breakdown.py
```

> 两处 env 探针都**只影响显式设置时**，默认行为与之前一致：
> - `RAPIDOCR_CPU_ARENA=0`（`packages/rapidocr_ort/src/ep.rs`）：显式注册 CPU EP 并
>   `DisableCpuMemArena`，用于内存归因。
> - `RAPIDOCR_INTRA_THREADS=N`（`packages/rapidocr_ort/src/lib.rs`）：显式钉 intra-op 线程数，
>   用于和 Python/C++ 对齐（也是绕开 ORT 默认池超订的开关，§3.5）。
>
> 这是本次两处被测代码改动，都只为测量口径，默认路径未变。

文件：

| 文件 | 作用 |
| --- | --- |
| `tests/bench/subtitle_ocr/src/bin/probe.rs` | Rust 单帧探针（计时，未改实现逻辑） |
| `tests/bench/subtitle_ocr/src/bin/phase_probe.rs` | Rust 分阶段探针（§4.6：det 预处理 / det 推理+后处理 / 完整 detect / 读图 / ROI） |
| `tests/bench/subtitle_ocr/rust-vs-py/py_phase.py` | Python 分阶段探针（与 `phase_probe` 同口径） |
| `tests/bench/subtitle_ocr/rust-vs-py/py_det_pre_breakdown.py` | Python det 预处理的 resize/normalize/permute 拆解 |
| `tests/bench/subtitle_ocr/rust-vs-py/py_det_split.py` | Python det 的 pre/infer/post 三段（`--arena-on` 对齐 Rust） |
| `packages/rapidocr_ort/src/ep.rs` | `RAPIDOCR_CPU_ARENA=0` arena 探针（默认不生效） |
| `packages/rapidocr_ort/src/lib.rs` | `RAPIDOCR_INTRA_THREADS=N` 线程探针（默认不生效） |
| `packages/subtitle_ocr_cpp/CMakeLists.txt` | ORT 目录/版本自动探测，`-DORT_DIR` 可切任意版本 |
| `tests/bench/subtitle_ocr/rust-vs-py/monitor.py` | 子进程峰值/平均 RSS 采样 |
| `tests/bench/subtitle_ocr/rust-vs-py/py_batch.py` | Python 批量/one-shot 驱动；`--models v3` 指仓库 v3 权重；`--arena-on` 为归因诊断开关 |
| `tests/bench/subtitle_ocr/rust-vs-py/run.sh` | 跑批（`taskset 0-3`、显式 4 线程、cpp 三版本轮换顺序、可只跑一类） |
| `tests/bench/subtitle_ocr/rust-vs-py/summarize.py` | 出表 |
| `tests/bench/subtitle_ocr/rust-vs-py/results/` | 原始 JSON / cpp stdout+stderr / `summary.md` / `threadpool-check.txt` / `cpp-text-check.txt` |
| `tests/bench/subtitle_ocr/rust-vs-py/results/phase/` | §4.6 的分阶段原始数据（三轮 + det 拆分 + 预处理拆解） |

---

## 8. 局限（引用这些数字时必须一起说）

1. **只测 1 帧**（按要求）：这是「同一帧重复 40 次」的稳态单帧成本，不代表全视频分布；
   不同帧的字幕框数量不同，rec 开销会变（本例仅 1 个框，rec ~21ms，偏轻）。
2. **同权重组**：v4（Python 包内 vs Rust v4）与 v3（Python `--models v3` vs Rust v3 /
   C++ v3）分开比；ORT 版本 Python/Rust **1.28.0**，C++ 有 **1.24.4/1.28.0/1.32.0** 三份，
   跨实现比速度优先用同版本那一列（§3.2）。
3. **arena 默认不同**（Python 关、Rust/C++ 开）。已补测两格对齐（§3.3）；按速度优先取
   arena 开（Rust 在此格同时更快、峰值更低），但**内存表述必须带这个条件**。
4. Python 的 one-shot 口径包含解释器启动与 `import cv2`，与「Rust 常驻进程」不是同一件事，
   只能作为「旧调用方式」的证据，不能当作「Python 推理慢」。
5. **线程数必须显式钉**（本次教训，§3.5）：Rust 若不设 `RAPIDOCR_INTRA_THREADS`，ORT 会开
   16 线程池，在 4 CPU 内超订，单帧劣化 1.74×。旧版本没有这条约定，其 Rust 绝对值属于
   「碰巧等于 4 线程」；跨版本引用前请先跑 `results/threadpool-check.txt` 的对照。
6. **跨会话绝对值漂 ±10%**：四次会话的主表量级为 Rust v4 353–365ms、Python v4 418–442ms、
   Rust v3 214–233ms、Python v3 301–309ms。主表会话（11:52）轮间只有 0.8–4.1%，
   11:05 / 11:31 两次是 9–21%（其中一次遇到 loadavg 10.6 的尖峰，整轮作废未采用）。
   简历用下限（v4 1.2×、v3 1.3×）。
7. `subtitle_only` 的窗口定义三实现不完全一致：Python 用绝对像素 `620 ≤ y ≤ 700`，
   Rust/C++ 用比例 `0.85 ≤ y/h ≤ 0.99`（720p 下 612–713px）。本帧的字幕框（y 647–682）
   两种窗口都包含，故不影响本次单帧结果；换帧复核时需注意。
8. **arena 差异的机制没有逐项拆解**：只确认了「开关值 200–590MB」「Rust 与 C++ 同配置下
   基本持平」，没有做 `smaps_rollup`/各 mapping 的归因（heap vs arena vs 模型权重）。因此
   「开 arena 时 Python 为何比 Rust 多吃 236MB」目前只有方向性解释，不算结论。
9. **Rust 默认线程池的 1.74× 劣化原因未定位到代码级**：只确认「默认全局池 vs 显式 per-session
   池」差异在 25 vs 13 线程时出现，且高度可复现（638 vs 366ms）。是 ORT 的 spin 策略、
   全局池共享，还是 4 CPU cpuset 下的调度问题，需要单独分析。
10. **Python 的 v3 接线是驱动层适配**：改 `det_model_path/rec_model_path/rec_keys_path`，并把
    仓库 JSON 字典转成 Python 需要的 txt（去首占位 + Python 自动补 blank/空格）。它验证的是
    「同一份 v3 权重、同一个 `ocr_frame` 流程」，但字典加载路径与 Rust/C++ 不同；若怀疑字典
    映射有偏移，应以逐字输出（本帧一致）和小规模 CER 复核。
11. **v3 组的速度差距（1.33–1.48×）比 v4 组（1.18–1.25×）大**：§4.6 的逐阶段拆解给出了
    主因——v3 的 det 推理只占 ~200ms，而 v4 占 ~340ms，所以**同样 ~76ms 的宿主侧开销**落在更小的
    基数上，比值就更大。引用时仍按「实测比值」，不要外推成「v3 上 Rust 更强」。
12. **C++ 的 `totalMs` 不可用**：它把 `modelLoadMs` 每帧重复计入（虚高 ~110ms/帧），
    本文一律用分相相加。若引用 cpp 自报的 `totalMs` 会得到偏高 40% 的数字。
13. **C++ 的迁移链比值本身在漂**：Python→C++ 1.15–1.17×、C++→Rust 1.09–1.14×（两次全量
    会话）。C++ 与 Rust 的差距只有 20–35ms，已接近跨会话漂移量级；写简历用「累计 1.3×」
    比拆成两段更稳，拆段时要注明哪一次会话。
14. **C++ 的分相账不完整**：`detMs/postMs/recMs` 不含 cls 与 warp 裁剪（实测每帧还有
    ~4–14ms 未入账，`wall/帧 − modelLoad − 三相和` 得出），所以 §4.6 的分段表没有 C++ 列，
    C++ 只用整帧 wall。C++ 的 `recMs` 只包 rec `Run`，与 Python/Rust 的「cls+裁剪+rec」不同口径。
15. **det→rec 的首帧惩罚只测到现象、没归因到代码级**：Python 单框 rec 孤立 7.8ms、
    紧跟 det 后 23.9ms（第二次 15.3ms、停 20ms 后 9.9ms），C++ 同量级，Rust 明显更低。
    候选解释是 ORT 线程池的 spin/唤醒（空转 20ms 反而更快，说明不是纯粹的缓存冷暖），
    但本次没有做 perf/ftrace 级验证，故 §4.6 只把它记为「可复现的测量事实」。
16. **§4.6 的分段是独立循环测的**（每阶段各自 20 次），缓存/线程状态与真实流水线不完全相同；
    可信的是同口径的 `engine`/`detect` 总和（与 §3.1 差 ≤2%）以及各阶段之间的**差值方向与量级**。
