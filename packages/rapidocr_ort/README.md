# rapidocr_ort

PP-OCR det / cls / rec 三阶段 OCR 引擎，底层用 onnxruntime（`ort` crate）推理，
配合 OpenCV（`opencv` crate）做几何/插值，`faer` 做矩阵求解，`geometry` 提供
多边形几何原语。

本 README 讲清楚**一张图片进来，到拿到识别结果，中间经过哪些步骤**，以及每步的
耗时占比——这样你能判断性能瓶颈在哪、为什么比别家快/慢。

## 一、单帧处理链路（`OcrEngine::detect`）

输入：`Array3<u8>`，H×W×3，0-255，**BGR 通道顺序**（PP-OCR 模型按 `cv2.imread`
的 BGR 训练，见下方「通道约定」）。输出：`Vec<OcrResult>`，每个元素是一条
识别文本 + 四点框 + 置信度。

```
图片 (HxWx3 BGR)
  │
  ├─[1] preprocess_det         缩放 + 归一化 → det 输入张量 [1,3,H',W']
  │     · 短边缩放到 736（DET_LIMIT_SIDE），长边等比，round 对齐 32 网格
  │     · image crate Triangle 双线性缩放（与 cpp 的 cv::resize 略有核差异，见 README/bench）
  │     · 归一化 (x/255 - mean)/std，mean=[0.485,0.456,0.406], std=[0.229,0.224,0.225]
  │     ── 耗时占比：小（缩放 + 逐像素归一化）
  │
  ├─[2] det 推理（onnxruntime）  det 输出热力图 [1,1,H'/32,W'/32]
  │     ── 耗时占比：**最大**（400-500ms/帧，全帧单次）
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
  │     │   ├─[4b] preprocess_rec  resize 到 48×W'（W'=int(48*ratio)，floor 对齐 cpp）
  │     │   │      补零到 batch 宽 → rec 输入张量 [1,3,48,W']
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
| `REC_H` / `REC_NORM` | 48 / (0.5,0.5) | rec 输入高 + 归一化 |

## 二、性能

### 单帧分步耗时（release，720p，实测）

| 步骤 | 耗时 | 说明 |
| --- | --- | --- |
| det 推理 | 400-500ms | 绝对大头，全帧单次 |
| DB 后处理 | ~12ms | 几何 + findContours |
| warp 裁剪 | ~2ms/框 | 经 perf 修复（见下） |
| rec 推理 | ~20ms/框 | 逐框一次 |

### 一个值得知道的坑：warp 别重复调用

`crop_for_rec_warp` 的 `warp_perspective` 曾**先调 `warp_perspective_def`（默认
INTER_LINEAR）又调显式 `warp_perspective`（INTER_CUBIC）**，同一次透视跑了两次，
单框 warp 从 ~2ms 飙到 ~13.4ms。已删除第一次调用（`perf:` commit）。改代码时务必
只调一次，且用显式 flags。

### 为什么 rust（本包 + subtitle_ocr）单帧推理不比 cpp 慢，甚至略快

- **det 推理**：两端都用 onnxruntime，相同权重，耗时基本一致（400-500ms），是共同瓶颈。
- **DB 后处理**：rust 用 `geometry` 的旋转卡尺 + OpenCV 的 findContours，cpp 用
  OpenCV minAreaRect。都很快（~12ms），几何计算量小，差别可忽略。
- **warp 裁剪**：rust 直接用 OpenCV 绑定 `cv::warpPerspective`，与 cpp 的
  `cv::warpPerspective` **同一个 C 实现**，逐位一致且性能相同（~2ms/框）。
- **rec 推理**：两端都用 onnxruntime，相同 rec 权重，一致。

所以 rust 没有比 cpp 慢的环节——背靠背单帧（同帧、同负载）实测 rust 总推理
707ms vs cpp 825ms（frame_230 三框）。差异主要来自进程级开销/线程调度，非算法。
**全量 RTF 对比必须在空载（loadavg <1）下跑**，否则负载会污染数据（本仓库 README
明确警告过）。

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
> 摊薄，单帧推理纯看 det 387ms + 其余 ~30ms ≈ 420ms。

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
快路径，否则回退 wide 标量版。resize 15.2ms 已接近 cpp 的 OpenCV SIMD resize
（<5ms，仍有 3× 差距，因额外 f32 转换与 gather 开销）。

## 三、依赖与分层

| 依赖 | 用途 |
| --- | --- |
| `ort` | onnxruntime 推理（det/cls/rec 三个 Session） |
| `opencv` | det 的 findContours/dilate/fillPoly；rec 的 warpPerspective（INTER_CUBIC） |
| `faer` | `getPerspectiveTransform`（DLT 8×8 线性系统求解） |
| `geometry` | 多边形几何（minAreaRect / boxPoints / offsetPolygon），纯 Rust、glam |
| `glam` | geometry 的 Vec2 点类型 |

`geometry` / `faer` 是纯 Rust，逐步降低对 OpenCV 的依赖；当前仍用 OpenCV 的地方是
像素级算子（findContours / warpPerspective / resize），这些 OpenCV 的 SIMD 实现
很难在纯 Rust 里逐位复刻，故保留绑定。

## 四、相关

- `subtitle_ocr`：基于本包的上层封装，加字幕专属逻辑（ROI 裁剪、y 过滤、NMS、
  帧合并计时）。
- `tests/bench/subtitle_ocr`：三实现（cpp/py/rust）对比基准，记录了对齐 cpp 的
  完整排查过程。
