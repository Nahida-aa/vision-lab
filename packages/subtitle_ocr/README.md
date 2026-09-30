# subtitle_ocr

字幕 OCR：基于 `rapidocr_ort` 引擎的上层封装，把「一帧图片」转成「该帧的字幕行」，
并能把「一组帧」合并成带时间轴的字幕段（对标 LocalDub / `subtitle_ocr_cpp`）。

本 README 讲清楚**从一张图片（或一帧视频）进来，到拿到最终字幕段，中间经过哪些
步骤**，以及每步耗时占比。底层引擎 `rapidocr_ort` 的单帧 det/rec 链路见它的
README，这里聚焦字幕专属层。

## 一、单帧处理链路（`SubtitleOcr::ocr_image`）

输入：`Array3<u8>`，H×W×3 BGR。输出：`Vec<FrameLine>`（每条 = 文本 + 四点框 +
置信度 + y 中心）。

```
帧 (HxWx3 BGR)
  │
  ├─[1] bottom_only：裁底部 40% 作 ROI          （y_offset = H*0.6，ROI = H*0.4 高）
  │     ── 字幕几乎都在画面底部，先裁掉上部减少 det 无关区域
  │     ── 耗时：几乎为 0（数组切片 + 拷贝）
  │
  ├─[2] engine.detect(&roi)                     （整段 = rapidocr_ort 的 det+rec+warp）
  │     · preprocess_det → det 推理 → db_postprocess → 逐个文本框 warp/axis 裁剪 → rec
  │     · 详见 rapidocr_ort README 的单帧链路
  │     ── 耗时占比：**绝大部分**（400-600ms/帧）
  │
  ├─[3] 坐标还原：ROI 框坐标 + y_offset 映射回原图
  │
  ├─[4] subtitle_only：y 中心须落在画面底部比例区间 [0.85, 0.99] 才保留
  │     · ratio = y_center / H（画面底部字幕）
  │     ── 耗时：几乎 0（纯过滤）
  │
  ├─[5] 丢弃空文本 & text_confidence < text_confidence_threshold 的框
  │
  ├─[6] NMS（use_nms 默认开）：按面积降序，剔除被已保留大框覆盖 >70% 的小框
  │
  ├─[7] 排序：先按 y 中心（差 ≤20px 视为同行），再按 x 中心
  │
  └─ 返回 Vec<FrameLine>
```

### 与 cpp 的对齐开关

| 开关 | 默认 | 说明 |
| --- | --- | --- |
| `--subtitle-only` | 关 | 只保留画面底部 [0.85,0.99] 的字幕 |
| `--warp-crop` | 关 | rec 裁剪用透视矫正（warpPerspective），对齐 cpp 的 rec 输入 |
| `--no-nms` | 关 | 关闭 NMS（对齐 cpp `--no-nms`） |
| `--full-frame` | 关 | 关闭 bottom_only，整帧 OCR |

`--warp-crop` **开** 时 rust 与 cpp 逐位一致（CER/spurious/missed 全对齐，见
`tests/bench/subtitle_ocr` README）；关时用轴对齐裁剪，文本精度最高（paired CER
0.18%）但会多出 4 个 spurious。

## 二、整段视频链路（`--dir --merge`）

从一组帧到带时间轴的字幕段：

```
帧目录 (frame_00001.jpg ...)
  │
  ├─[1] 逐帧：load_rgb(BGR) → ocr_image → 每帧得到 boxes
  │     · 推理耗时由调用方在 ocr_image 前后 Instant 自行测量（旁路观测，不进 JSON）
  │
  ├─[2] 每帧聚合：aggregate_frame → FrameResult { text, confidence, box_y, timestamp }
  │     · timestamp = 帧序号 × (1000/fps) ms
  │
  ├─[3] merge_frames：把相邻、文本接近的帧合并成段
  │     · merge_gap_ms(500)：间隔 ≤ 500ms 且文本接近则并入
  │     · 文本取更长者；置信度取所有帧算术均值；end 刷新到末帧
  │     · 单帧出现的字幕 → start==end（zero-dur 段，LocalDub 同款行为）
  │
  └─ 返回 Vec<Segment> { text, start_ms, end_ms, confidence, box_y }
```

## 三、性能

单帧分步耗时（release，720p，实测）：见 `rapidocr_ort` README 的「性能」表。字幕
专属层（ROI 裁剪 / y 过滤 / NMS / 排序 / 聚合）耗时**可忽略**（纯数组操作），真正
耗时全在底层 `engine.detect`（det 推理 400-500ms 是大头）。

**背靠背单帧对比**（同帧同负载）：rust 总推理 707ms vs cpp 825ms（frame_230 三框），
rust 不比 cpp 慢。**全量 RTF 必须在空载（loadavg <1）下跑**，否则负载污染数据。

`--merge` 只影响后处理（merge_frames），不增加每帧 OCR 耗时。

## 四、通道约定

本包 `load_rgb` **显式把 image crate 的 RGB 转成 BGR** 再喂引擎（PP-OCR 模型按
BGR 训练）。若不转，彩色字幕会漏检/误识——这是本仓库踩过的坑（`tests/bench`
README 有完整排查记录）。

## 五、相关

- `rapidocr_ort`：底层引擎（单帧 det/rec/warp 链路）。
- `tests/bench/subtitle_ocr`：三实现对比基准 + 与 cpp 完美对齐的排查记录。
