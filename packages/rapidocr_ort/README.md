# rapidocr_ort

PP-OCR det / cls / rec 三阶段 OCR 引擎，底层用 onnxruntime（`ort` crate）推理，
配合 OpenCV（`opencv` crate）做几何/插值，`faer` 做矩阵求解，`geometry` 提供
多边形几何原语。

本 README 讲清楚**一张图片进来，到拿到识别结果，中间经过哪些步骤**，以及每步的
耗时占比——这样你能判断性能瓶颈在哪。

同配置（同权重、同 4 线程、同 ORT 版本、同一帧、常驻 session）下，这套 Rust 实现
比 Python / C++ 参考实现**更快**，且**内存更省**。这不是「Rust 语言算得快」：三个
实现最终都调同一份 onnxruntime C++ 内核，det 推理耗时基本持平。快的部分来自**宿主侧
少做的功**——常驻 session 不每帧重建、一趟 SIMD 的 CHW 归一化替代 numpy 的临时数组、
det→rec 切换首帧惩罚更小。逐阶段的量化归因见
[`docs/ocr/ppocr/benchmark-rust-vs-py-single-frame.md`](../../docs/ocr/ppocr/benchmark-rust-vs-py-single-frame.md)
（§4.6 Python 归因、§4.7 同 ORT 下 C++ 对比），本 README 的性能章节与之一致。

## 一、单帧处理链路（`OcrEngine::detect`）

输入：`Array3<u8>`，H×W×3，0-255，**BGR 通道顺序**（PP-OCR 模型按 `cv2.imread`
的 BGR 训练，见下方「通道约定」）。输出：`Vec<OcrResult>`，每个元素是一条
识别文本 + 四点框 + 置信度。

```
图片 (HxWx3 BGR)
  │
  ├─[1] preprocess_det         缩放 + 归一化 → det 输入张量 [1,3,H',W']
  │     · 短边缩放到 736（DET_LIMIT_SIDE），长边等比，round 对齐 32 网格
  │     · geometry::imgproc 的纯 Rust SIMD 双线性缩放（half-pixel，对齐 cpp 的 cv::resize）
  │     · 归一化 (x/255 - 0.5)/0.5（PP-OCR 训练口径），一趟 SIMD 写出 CHW
  │     · ⚠️ 不是 ImageNet 的 0.485/0.229：曾误用 ImageNet 常量导致 det 输入偏置错乱、漏检
  │     ── 耗时占比：小（一趟 SIMD 缩放 + 归一化，同帧约 14ms）
  │
  ├─[2] det 推理（onnxruntime）  det 输出热力图 [1,1,H'/32,W'/32]
  │     ── 耗时占比：**最大**（单帧 ~190ms 量级，全帧单次）；这一段由 ORT 内核决定，
  │        同权重/同线程/同 ORT 版本下 Python/C++/Rust **基本持平**，不是优化空间
  │
  ├─[3] db_postprocess（DB 后处理）  热力图 → 文本框四点框列表
  │     · sigmoid → 二值化（thr=0.3）→ 2×2 dilate → cv::findContours
  │     · 每个轮廓：geometry::minAreaRect → boxPoints(tl-tr-br-bl) → 边长<3 早退
  │       → box_score_fast(4点框掩码内对 prob 取均值) → score<thresh(0.6) 丢弃
  │       → offsetPolygon 外扩 unclip → 再 minAreaRect → 边长<5 早退 → 缩放回原图
  │     ── 耗时占比：小（每帧几十个轮廓，几何运算）
  │
  ├─[4] 逐个文本框（N 个框循环）
  │     ├─ crop：crop_for_rec_warp（透视矫正）或 crop_for_rec（轴对齐包围盒）
  │     │     · warp 版：cv::warpPerspective(INTER_CUBIC, BORDER_REPLICATE) + rotate90
  │     │     ── 耗时占比：~2ms/框（经 perf 修复后，见「性能」）
  │     │
  │     ├─ recognize：
  │     │   ├─[4a] preprocess_cls  resize 48×192 + 归一化 → cls 推理
  │     │   │      判断是否需 180° 旋转（need_rotate_180）
  │     │   │      ── 耗时占比：小
  │     │   ├─[4b] preprocess_rec  resize 到 48×resized_w（ceil(48*ratio)，≤img_w）
  │     │   │      img_w = int(48*max(320/48, ratio))，单框恒 320；右侧补零到 img_w
  │     │   │      → rec 输入张量 [1,3,48,img_w]（近方形单字不补零会输出全 blank，见 preprocess.rs）
  │     │   └─[4c] rec 推理 → CTC greedy decode → (文本, 置信度)
  │     │         ── 耗时占比：中（每框一次，逐字解码）
  │     │
  │     └─ 坐标还原 + 聚合 → OcrResult
  │
  └─ 返回 Vec<OcrResult>
```

### 通道约定（重要）

PP-OCR 的 Python 实现（`cv2.imread`）和 cpp 参考（`cv::imread`）都喂 **BGR** 给
det/rec/cls 模型，模型按 BGR 训练。因此 `detect` 期望 BGR 输入。若外部喂 RGB，
彩色字幕/文字会漏检或误识（本仓库踩过，见 `subtitle_ocr` README）。上层负责把
图转成 BGR（`image` crate 默认给 RGB，需 R/B 交换）。

### 关键常量

| 常量 | 值 | 说明 |
| --- | --- | --- |
| `DET_LIMIT_SIDE` | 736 | det 输入短边目标 |
| `DET_THRESH` | 0.3 | 二值化阈值 |
| `BOX_THRESH` | 0.6 | 检测框得分下限 |
| `UNCLIP_RATIO` | 1.6 | 文本框外扩比例 |
| det / cls / rec 归一化 | `(x/255 - 0.5)/0.5` | 三者统一用 PP-OCR 训练口径 `mean=std=0.5`；**非** ImageNet 的 0.485/0.229（误用会漏检，见 `preprocess.rs`） |
| `REC_H` / `REC_MAX_WH_RATIO` | 48 / 320÷48 | rec 输入高 + 默认最大宽高比（单框输入宽恒 320、右侧补零，见 `preprocess_rec`） |

## 二、性能

> **数据版本**：本节「为什么更快」的分阶段表是**当前**同配置口径（v3/v4、4 线程、
> `taskset -c 0-3`、同一帧、常驻 session、三轮取中位），以
> [`docs/ocr/ppocr/benchmark-rust-vs-py-single-frame.md`](../../docs/ocr/ppocr/benchmark-rust-vs-py-single-frame.md)
> 为唯一数据源。后面标「早期实测 / frame_230 / frame_299」的两张分步表是 det 预处理
> SIMD 优化**前后**的历史记录，绝对值已随模型与预处理改动变化，**不要拿它们当当前
> 性能结论**；保留它们是为说明「瓶颈一直在 det 推理，宿主侧优化动不了它」这一结构。

### 为什么更快（同配置下的归因）

先排除一个常见误解：**不是 Rust 语言让推理变快**。det 推理由 onnxruntime 的 C++
内核执行，同权重、同 4 线程、同 ORT 版本下，Python / C++ / Rust 的纯 det `Run`
耗时**基本持平**（同窗口配对差 ≤3ms，见基准文档 §4.7.2）。所以「更快」不来自内核，
而来自**宿主侧少做的功**。以 v3 单帧（不含读图）为例：

| 阶段（ms/帧） | Python v3 | C++ v3 | Rust v3 | 谁在省 |
| --- | ---: | ---: | ---: | --- |
| det 预处理 | 40.1 | 21.3 | **14.4** | Rust 一趟 SIMD 写 CHW；Python numpy 每帧 ~115MB 临时数组、C++ 手写三重 CHW 循环 |
| det 推理（C++ 列不含后处理，见口径注） | 219.9 | 202.9 | **202.0** | 三者持平（ORT 内核），后处理 ~2ms |
| cls + 裁剪/resize + rec | 41.7 | 20.0 | **7.9** | Rust 付的 det→rec 切换首帧惩罚最小 |
| **合计（不含读图）** | **301.0** | **250.1** | **225.1** | — |

> 口径：Python/Rust 两列来自基准文档 §4.6 的分阶段独立循环（`engine` / `detect` 口径，
> det 推理含 ~2ms DB 后处理）；C++ 列来自 §4.7.1 的逐帧分相（`detRun` 不含后处理，
> 但 rec 是紧跟 det 的真实流水线值）。**看差值方向与量级，不要把某一项跨列相减**——
> 纯 det `Run` 同窗口配对后三者持平（§4.7.2）。

具体省在四处：

1. **进程 / session 模型（最大的一块）**：本包是常驻进程，3 个 ORT session 只建一次。
   旧的 Python CLI 调用方式每帧起一个进程、重建 3 个 session，**827ms → 365ms（2.27×）**，
   其中 ~385ms/帧 是进程启动 + session 重建，根本不是推理。
2. **宿主侧数据搬运**：det 归一化用 `geometry::imgproc` 一趟 SIMD 直接产出 CHW f32，
   不像 numpy 那样先生成 ~115MB 中间数组再逐框 crop/resize。det 预处理 **40.1 → 14.4ms**
   （v3），v4 同样 40.6 → 14.5ms。
3. **det→rec 切换首帧惩罚更小**：Python/C++ 在「det 刚跑完紧接着跑 rec」时有一段
   首帧惩罚（受控探针：C++ rec 单独跑 5.1ms、紧跟 det 后 22.9ms），Rust 付得少，
   所以 `cls + 裁剪 + rec` 整段 **41.7 → 7.9ms**。机制仍疑似 ORT 线程池 spin/唤醒，
   未做 perf 级归因，但现象可复现（基准文档 §4.6 / §4.7）。
4. **同 ORT 下 C++ 比 Rust 慢 1.15× 也不是语言**：差在 C++ 手写 CHW 循环 vs Rust SIMD
   （+7~8ms）和 C++ 中招的 det→rec 切换惩罚（+11~18ms）；纯 det 推理同窗口配对持平。
   （另外 C++ det 用了 ImageNet 归一化常量，属**正确性差异**，本帧恰好都检出字幕不影响
   速度；本包用 0.5/0.5，见 `preprocess.rs`。）

内存同理：arena 开（速度最优、也是 Rust/C++ 默认）时 Rust 峰值比 Python 低 **20–24%**
（v4 927 vs 1163MB、v3 736 vs 972MB）；但 arena 一个开关就值 200–590MB，引用内存
结论必须写明 arena 条件（基准文档 §3.3）。

> 跨会话绝对值有 ±10% 漂移，同会话比值只漂 ±3%；引用倍率请用基准文档的同会话数字，
> 不要拿不同帧、不同负载下的绝对值相除。

### 单帧分步耗时（早期实测，720p，见上方「数据版本」）

| 步骤 | 耗时 | 说明 |
| --- | --- | --- |
| det 推理 | 400-500ms | 绝对大头，全帧单次（该帧 det 输入更大/模型版本更早） |
| DB 后处理 | ~12ms | 几何 + findContours |
| warp 裁剪 | ~2ms/框 | 经 perf 修复（见下） |
| rec 推理 | ~20ms/框 | 逐框一次 |

### 一个值得知道的坑：warp 别重复调用

`crop_for_rec_warp` 的 `warp_perspective` 曾**先调 `warp_perspective_def`（默认
INTER_LINEAR）又调显式 `warp_perspective`（INTER_CUBIC）**，同一次透视跑了两次，
单框 warp 从 ~2ms 飙到 ~13.4ms。已删除第一次调用（`perf:` commit）。改代码时务必
只调一次，且用显式 flags。

### 分步计时（release，frame_299 单框，实测）

**模型加载**（每次进程启动一次）：

| 步骤 | 耗时 |
| --- | --- |
| det 模型加载 | ~123ms |
| rec 模型加载 | ~67ms |
| cls 模型加载 | ~34ms |
| vocab 加载 | ~0.4ms |

**单帧推理**：

| 步骤 | 耗时 | 说明 |
| --- | --- | --- |
| `pre_det`（缩放 + 归一化） | **~44ms** | imgproc SIMD（原 128ms，见「优化点」） |
| det 推理 | ~387ms | 大头，onnxruntime |
| db_postprocess | ~8ms | findContours + 几何 |
| crop（warp） | ~1.6ms | cv::warpPerspective |
| cls 推理 | ~2.2ms | |
| preprocess_rec | ~0.8ms | |
| rec 推理 | ~16ms | |
| **合计** | **~520ms** | |

> 注：以上含模型加载（CLI 每次启动加载一次）。全量基准里模型只加载一次、启动开销
> 摊薄，单帧推理纯看 det 387ms + 其余 ~30ms ≈ 420ms。这是**早期该帧**的绝对值；
> 当前同配置口径下 det 预处理约 14ms、整帧约 230ms（v3，见基准文档 §4.6/§4.7）。

**优化点（`pre_det` 128ms）**：细分实测 `pre_det` ≈ 128ms，其中 **resize 71ms**、
**normalize 14ms**（余下为 det 输入张量构造等）。已用 `geometry::imgproc`（纯 Rust
SIMD，`wide` crate）实现 bilinear resize + normalize 后：

| 步骤 | 旧（image Triangle） | wide 标量 | **AVX2 gather** |
| --- | --- | --- | --- |
| resize | 71ms | 40ms | **15.2ms** |
| normalize | 14ms | 8.5ms | ~12ms |
| **pre_det 合计** | **128ms** | ~50ms | **~28ms**（-78%） |

同时把 det 缩放核从 Triangle 改为 bilinear（对齐 cpp 的 `cv::resize` INTER_LINEAR，
half-pixel 坐标）。**全量基准确认 spurious/CER 完全不变**（CER 0.36%、spurious 0、
missed 0、segs 75）——因为其他环节都已对齐 cpp，bilinear 是正确选择。

resize 最终用 **AVX2 gather**（`std::arch` 的 `_mm256_i32gather_ps`，一次 gather 8 个
任意位置的 f32），配合可分离水平+垂直插值与行缓存。`imgproc` 的
`resize_bilinear_hwc` 在运行时 `is_x86_feature_detected!("avx2")` 分派，AVX2 可用走
快路径，否则回退 wide 标量版。

**效果**：det 预处理（缩放 + 归一化 + 一趟写 CHW）现在合起来约 **14ms/帧**，
比 C++ 参考实现的手写三重 CHW 归一化循环（约 21ms）还快 ~7ms，更是远低于 Python
numpy 版的 ~40ms——差别全在「是不是一趟 SIMD 直接产出 CHW」，不在语言，也不在
ORT 内核（基准文档 §4.6 / §4.7.1）。

## 三、依赖与分层

| 依赖 | 用途 |
| --- | --- |
| `ort` | onnxruntime 推理（det/cls/rec 三个 Session） |
| `opencv` | det 的 findContours/dilate/fillPoly；rec 的 warpPerspective（INTER_CUBIC） |
| `geometry::imgproc` | det 的 SIMD 双线性缩放 + 一趟归一化写 CHW（纯 Rust，AVX2 gather 分派） |
| `image` | rec/cls 的缩放（Triangle 核，纯 Rust） |
| `faer` | `getPerspectiveTransform`（DLT 8×8 线性系统求解） |
| `geometry` | 多边形几何（minAreaRect / boxPoints / offsetPolygon），纯 Rust、glam |
| `glam` | geometry 的 Vec2 点类型 |

`geometry` / `faer` 是纯 Rust，逐步降低对 OpenCV 的依赖。三条预处理路径里，
**det 的缩放 + 归一化已全部走 `geometry::imgproc` 的纯 Rust SIMD**（一趟写 CHW），
rec/cls 的缩放走 `image` crate（也是纯 Rust）；当前仍用 OpenCV 绑定的只剩像素级
算子 `findContours` / `dilate`（det 后处理）与 `warpPerspective`（rec 透视裁剪），
这些 OpenCV 的 C 实现最难在纯 Rust 里逐位复刻，故保留。det 预处理换成纯 Rust
SIMD 后并未牺牲精度（全量 CER/spurious 不变），反而比 C++ 手写 CHW 循环更快，
见「为什么更快」。

## 四、相关

- `subtitle_ocr`：基于本包的上层封装，加字幕专属逻辑（ROI 裁剪、y 过滤、NMS、
  帧合并计时）。
- `tests/bench/subtitle_ocr`：三实现（cpp/py/rust）对比基准，记录了同配置下的
  逐阶段归因与探针（`results/phase/`）。
- [`docs/ocr/ppocr/benchmark-rust-vs-py-single-frame.md`](../../docs/ocr/ppocr/benchmark-rust-vs-py-single-frame.md)：
  单帧实测主文档，含逐阶段归因（§4.6 Python、§4.7 同 ORT 下 C++ 对比）。本 README
  的性能数字与结论均以它为准。
