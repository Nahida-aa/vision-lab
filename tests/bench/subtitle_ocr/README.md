# subtitle_ocr 基准

对 `subtitle_ocr_cpp` / `subtitle_ocr_py` / `subtitle_ocr`（rust）三个实现做
**正确性灰盒测试**与**性能/质量基准**的驱动包。

两类测试共享抽帧、GT 对齐、JSON 解析等代码，故都用 Rust 编写：

- `src/bin/test.rs` —— 正确性灰盒测试（只跑 3 帧，验证 CLI 契约与输出结构）
- `src/bin/bench.rs` —— 性能与质量基准（整段视频）
- `src/lib.rs` —— 共享算法：`merge_frames`、CER、时序对齐

## 谁是被测对象，谁是测试驱动

⚠️ **抽帧策略属于基准测试的驱动层，不是被测试对象。** 这是本基准最重要的边界：

- **被测试对象**是三个 OCR 实现各自的**单帧识别能力**与**合并逻辑**：
  - 单帧识别质量（CER、对齐偏移、confidence）；
  - 单帧 / 批量推理性能（RTF）；
  - `merge_frames` 的合并正确性（rust 侧已有白盒单测覆盖）。
- **基准测试负责把视频变成「帧目录」并喂给实现**，实现只接收「已经抽好的帧」
  （cpp/py/rust 的 `--dir` 都是 `list_frames` 枚举目录里现成的 jpg/png，**不抽帧**）。
  抽帧的 fps、间隔、采样分辨率是**测试侧变量**，由 bench 控制，不是实现参数。

推论：**`start bias ~+230ms` 是基准测试的采样分辨率局限（fps=2 → 500ms/帧），
不是任一 OCR 实现的缺陷。** 把它当成实现 bug 去调，或当成实现参数去优化，都是
搞错了边界。fps=2 vs fps=4 对比是在验证「测试采样误差模型」，属于测试方法论自检，
优先级低于实现本身的正确性与质量。


入口统一走 `justfile`，见 `just --list`。

## 参考素材

| 文件 | 是否入库 | 说明 |
| --- | --- | --- |
| `ref/ocr_manual.json` | ✅ | 人工标注 GT，75 条，含 `start`/`end`（ms） |
| `ref/video_source.mp4` | ❌ **需自备** | 34M，体积过大不入库（`.gitignore`） |
| `../../.test-frames/` | ✅ | 灰盒测试用的 3 帧，460K |

`video_source.mp4` 规格（`bench` 依赖它抽帧，缺失会直接失败）：

```
分辨率  1280x720
帧率    30 fps
时长    170.062993 s
大小    34209105 bytes
sha256  344316821096379954e84a9bc1a437814ab4c5640b44fdb81792545480b76908
```

换用其他视频时 GT 也必须同步替换，否则 CER 与时序指标均无意义。

## 质量指标

`bench` 输出两组互相独立的质量指标，都写进 `summary.json`：

**文本质量**（拼串比对，对齐 LocalDub `eval-ocr.ts`）

- `CER(raw)` —— 去空白后的字符错误率
- `CER(norm)` —— 再做同音字/标点/数字归一化

**时序质量**（按时间重叠配对后统计）

拼串 CER 会把时间戳完全丢掉——字幕整体早/晚 500ms 也照样是 0.36%。
故另按时间区间配对再算一组：

- `IoU(mean)` —— 区间重合度
- `start Δms` / `end Δms` —— 边界偏移的 mean / median / p95，**正值 = 偏晚**
- `paired / missed / spurious / split / merged` —— 对齐计数
- `CER(paired)` —— 配对后逐条 CER，按 GT 字符数加权（漏检记为全错）

### 已知现象

**起点系统性偏晚约 230ms（属测试采样误差，非实现缺陷）。** `fps=2` 意味着
500ms 的时间分辨率，字幕在两帧之间出现时要等下一帧才被检出，期望损失恰为半帧。
终点偏移则接近 0。当前只报告不补偿——这个数字正是 fps 选型的依据（见上方
「谁是被测对象」）。

**`zero-dur` 零时长段。** `merge_frames` 对「只在单帧出现」的字幕会产出
`start == end` 的退化区间（实测 341 帧中有 4 条）。这是 LocalDub 原版行为，
此处不修改产出以保持可对比性；但配对时对退化区间改用「点落在区间内」判定，
否则它们会被误计为漏检 + 虚检各一次。

### rust 实现与 cpp 的残余差异归因（实测结论）

三套实现共享同一份 rapidocr 权重，单帧识别质量高度一致。逐帧对比 rust 与 cpp
（fps=2 / text-score 0.45 / subtitle-only，同机同视频）的实测——**注意当前提交的
rust 代码完全确定（连跑 3 次逐位一致），不是 run 间抖动**：

| 指标 | cpp | rust（当前提交） |
| --- | --- | --- |
| CER(norm) | 0.36% | 0.72% |
| CER(paired) | 0.36% | **0.18%（优于 cpp）** |
| spurious | 0 | 4 |
| missed | 0 | 0 |
| split / merged | 0 / 0 | 1 / 0 |
| zero-dur | 4 | 6 |
| IoU(mean) | 0.67 | 0.66 |
| start Δ | +221ms | +228ms（同，印证测试采样） |

> 历史记录过 0.54% / 2 spurious，但那是一次早期测量，当前代码无法复现（连跑 3
> 次都是 0.72% / 4 spurious）。以当前可复现数字为准。

#### 4 个 spurious 已定位到具体帧（bench 抽帧法：`select='not(mod(n,15))'`）

用 bench 的抽帧方式复现，rust 在 4 帧各检出**一个孤立字符**，cpp 在同一帧检出空框
或什么都不检：

| 帧 | rust 检出 | conf | cpp 检出 |
| --- | --- | --- | --- |
| frame_00225 | `V` | 0.58 | 空框 `('',0)` |
| frame_00251 | `3` | 0.45 | 空框 |
| frame_00262 | `E` | 0.67 | 空框 |
| frame_00300 | `9` | 0.76 | 空框 |

frame_00225 的 rust 框是 `[894,613]-[913,640]`（约 19×26px 小框，字幕区 y 613-640）。
这些是 rust det 用**轴对齐包围盒 + 简化 unclip** 让一个小文本状轮廓成框，cpp 用
`minAreaRect` + 标准 unclip 则压掉了（box_score 低、或框未形成）。`--no-nms` 下
依然存在，与 NMS 无关。

#### 已逐项排查并尝试对齐、均以证据否定/回退的点

1. **抽帧策略 / start bias**：属测试驱动层（见上「谁是被测对象」），非实现差异。
2. **det 缩放尺寸约定**（`det_target_size`）：已对齐到 cpp 的
   `round(dim*ratio/32)*32`（`ratio=736/short` 或 1.0）——实测**未消除** spurious。
3. **det 框打分**（`box_score_fast`）：已替换为官方
   `rapidocr_onnxruntime/ch_ppocr_v3_det/utils.py` 的 `box_score_fast`——实测
   **未消除** spurious。
4. **det 缩放插值核**：cpp 用 opencv `cv::resize(INTER_LINEAR)`，rust 用
   `image` 的 `Triangle`。曾切换 rust 到 opencv 双线性——聚合上**反而更差**
   （0.72%→0.89%、spurious 4→5）。**关键的单帧隔离实验**：把 rust det 临时换成
   与 cpp 完全一致的 bilinear，重跑 frame_225——**'V' 依然出现（conf 0.579）**。
   证明缩放核**不是**这些 spurious 的来源，回退到 `Triangle` 是安全的。
5. **BOX_THRESH**：cpp=0.5、rust=0.6。曾给 cpp 加 `OCR_BOX_THRESH` 环境变量设成
   0.6 与 rust 对齐——cpp 结果**完全不变**（仍是 0 spurious / 0.36%），证明阈值差异
   不是 spurious 来源。cpp 侧该环境变量可保留（默认仍 0.5）。
6. **det 后处理几何**：曾移植 cpp 的 `min_area_rect`（旋转卡壳）+ `offsetPolygon`
   到 rust——**反而更差**（CER(paired) 0.18%→0.36%、CER(norm) 0.72%→3.22%、
   zero-dur 6→7）。自写旋转矩形与 cv::minAreaRect 的 width/height/angle 归一化约定
   不一致。**已回退**到轴对齐包围盒 + 矩形 unclip。

**结论**：rust 与 cpp 的残余差异是 **4 个确定性 spurious（孤立 V/3/E/9）+ 6 个
zero-dur**。关键澄清：cpp 在 frame_225/251/262/300 的 `segments` 是**空数组**——
cpp 的 det **根本没有成框**（不是 rec 读空）。而 rust 与 cpp 的 det 输入已确认完全
一致（同 ROI 288×1280、同尺寸 736×3264、同缩放核），唯一剩余差异是 **det 后处理
几何**（rust 轴对齐包围盒 vs cpp `minAreaRect`）。

**决定性实验（2026-08-05）**：用已验证与真实 cv::minAreaRect 逐位一致的
`packages/geometry` 重写 rust det 几何，复刻 cpp 的 `minAreaRect → boxPoints →
box_score_fast(4点框) → offsetPolygon → 再 minAreaRect` 完整管线——
- 单帧层面：frame_225/251/262 的 V/3/E **消失**（3/4 spurious 消除，证明 det 几何
  确为 spurious 来源）；
- **聚合层面反而严重退化**：CER(paired) 0.18%→0.36%、CER(norm) 0.72%→**5.72%**、
  zero-dur 6→8、split 1→2。
- 原因：rust 的 rec 裁剪 `crop_for_rec` 是**轴对齐包围盒**，而 cpp 对每个 det 框做
  `warpPerspective`（透视矫正成水平）。新几何让 rust 也产出**旋转框**（如 frame_189
  的 '中' 框是倾斜 ~5° 的四边形），轴对齐裁剪会带进背景/旋转 → rec 严重劣化。
  **det 几何与 rec 裁剪是耦合的**：轴对齐 det 框 + 轴对齐裁剪本来自洽；一旦 det 框
  变旋转，必须同步引入 cpp 的 warpPerspective 裁剪，否则必然回归。

**因此"完美对齐"需要两步一起做**：① det 后处理几何对齐 minAreaRect（单独做会
回归）；② rec 裁剪改成 cpp 的 `warpPerspective + rotate90`。

**✅ 已完成（2026-08-05）**：两步骤都用 `packages/geometry` + faer 在 rust 落地：
- ① det 几何：`minAreaRect → boxPoints → box_score_fast(4点框) → offsetPolygon →
  再 minAreaRect`（`packages/geometry` 已验证与 cv::minAreaRect 逐位一致）。
- ② rec 裁剪：`crop_for_rec_warp` 用 faer 解 `getPerspectiveTransform`（DLT 8×8
  LU）+ `warpPerspective`（**INTER_CUBIC，A=-0.75 三次卷积**，对齐 cpp）+ rotate90。

实测（`--warp-crop`，fps=2 / ts0.45 / subtitle-only）rust vs cpp **基本逐位一致**：

| 指标 | rust（对齐后） | cpp |
| --- | --- | --- |
| CER(norm) | 0.36% | 0.36% |
| CER(paired) | 0.36% | 0.36% |
| spurious | 0 | 0 |
| zero-dur | 4 | 4 |
| split / merged | 0 / 0 | 0 / 0 |
| missed | 1 | 0 |
| segs | 74 | 75 |

即：CER、spurious、zero-dur、split **全部对齐 cpp**，仅剩 **1 个 missed**（rust 比
cpp 少检出 1 条字幕，segs 74 vs 75）。这是"完美对齐"的实质达成。
注意：此模式下 rust 放弃了自己之前的 rec 精度优势（CER(paired) 0.18%→0.36%，与
cpp 持平），换取与 cpp 的段结构完全一致。若追求文本精度最大化可回退到轴对齐模式
（`--warp-crop` 关闭，0.18% paired 但 4 spurious）。

#### 唯一 missed 的根因（'啊'，frame_00257）

GT 在 127700-128500ms 有 '啊'，rust warp-on 漏掉。定位：
- frame_00257 的 '啊' 框 `[624,647]-[657,683]`（近方形 33×36，单字）。
- `axis-off`（轴对齐裁剪）rec 读出 '啊' conf 0.64 ✓；`warp-on` 同一个框 rec 读出
  **'gsd' conf 0.177**（垃圾）→ 被 text_score 0.45 过滤 → missed。
- cpp 同帧读出 '啊'。

即：这是 **rec 裁剪在 warpPerspective 下的亚像素差异**——'啊' 是小字符，我的
`warpPerspective`（INTER_CUBIC 逆映射采样）与 OpenCV 的 warpPerspective 在亚像素
采样上有细微差别，足以把该字符的 rec 结果从 '啊' 翻转成 'gsd'。det 几何已对齐，
纯属 rec 采样精度的边界情况。要消除需让 warp 的逆映射/插值与 OpenCV 逐位一致
（含亚像素坐标约定），对单个字符成本高、收益低，当前判断可接受。

**决定性的 BGR 发现（2026-08-06）**：rust 读图用 RGB，cpp 用 BGR（`cv::imread`）。
把 rust 整帧换 BGR 后全量重跑（warp-crop）：

| 配置 | CER(norm) | CER(paired) | missed | spurious |
| --- | --- | --- | --- | --- |
| rust RGB | 0.36% | 0.36% | 1 | 0 |
| rust **BGR** | 0.54% | 0.54% | **0** | 0 |
| cpp BGR | 0.36% | 0.36% | 0 | 0 |

- **BGR 让 missed 1→0**（'啊' 恢复）——证实 **PP-OCR/rapidocr 模型按 BGR 训练**
  （PaddleOCR 用 cv2.imread=BGR，cpp 是对的），rust 用 RGB 是错的，任何彩色字幕
  都有 missed 风险（不止这个视频）。
- **但 BGR 让 CER 0.36%→0.54%**（变差）——同一 BGR 下 rust 仍比 cpp 差 0.18pp，
  差异在 **warp 的 INTER_CUBIC 插值舍入**（对照实验：扣除通道顺序后 rust/cpp warp
  输出仍有 50-60% 像素差 ±1-9，均匀分布，非坐标偏移）。

**结论**：真正要修的是**两件事**——① rust 应改用 BGR（修 missed，否则彩色字幕有
系统性风险）；② warp 插值需对齐 OpenCV 的 `initInterTab1D` 插值表，让 rust-BGR 的
CER 也降到 cpp 的 0.36%。两者都需落地，"完美对齐"才算真正完成；当前 RGB+warp 的
0.36%/1-missed 是"段结构对齐但通道错、missed 恰好 1"的中间态。

**✅ 完美对齐完成（2026-08-06）**：三项修复叠加后，rust 与 cpp **所有指标逐位一致**：

| 指标 | rust（对齐后） | cpp |
| --- | --- | --- |
| CER(norm) | 0.36% | 0.36% |
| CER(paired) | 0.36% | 0.36% |
| spurious / missed | 0 / 0 | 0 / 0 |
| zero-dur / split / merged | 4 / 0 / 0 | 4 / 0 / 0 |
| segs | 75 | 75 |
| IoU(mean) | 0.6741 | 0.6742 |

三项修复：
1. **BGR 通道**（`af03310`）：模型按 BGR 训练，rust 之前用 RGB 导致彩色字幕 missed
   （'啊'）。改 BGR 后 missed 0。
2. **OpenCV `warpPerspective`**：rec 裁剪改用 opencv 绑定的 `cv::warpPerspective`
   （INTER_CUBIC + BORDER_REPLICATE），替代手写插值——消除 ±1-9 像素舍入差。之前手写
   INTER_CUBIC 无法逐位复刻 OpenCV 的 HAL 定点路径，直接调 OpenCV 即 bit-exact。
3. **rec 宽度 floor 对齐**：`preprocess_rec` 的 rec_w 从 `round()` 改为 cpp 的
   `floor()`（`int(48*ratio)`），并补零到 imgW——修掉 `48*ratio` 非整数时的 1px 差异，
   消除 '白'→'百' 形近字翻转。

至此，rust `--warp-crop` 与 cpp 在 CER/spurious/missed/zero-dur/split/段数上完全一致，
是"完美对齐"的完成态。`--warp-crop` 关闭则回到轴对齐模式（0.18% paired 文本精度最优，
但有 4 spurious）。

## 性能注意事项

⚠️ **ORT 线程数不是越大越好。** det 输入小、rec 是逐行小图，算子粒度细，
线程超订的调度开销会盖过并行收益。16 核机实测（`--dir`，341 帧）：

| threads | det | rec | RTF |
| --- | --- | --- | --- |
| 2 | 106s | 3.2s | 0.65 |
| 4（cpp 默认） | 127~186s | 3~4s | 0.77~1.12 |
| 16 | 194s | 14.8s | 1.24 |

线程数优先级：CLI `--threads` > env `OCR_INTRA_THREADS` > cpp 内置默认 4。
用 `just scan-threads` 扫描本机最优值。

⚠️ **跑基准前确认机器空载。** 同参数连跑两轮 RTF 波动可达 22%，高负载下测出
的 RTF 没有参考价值。CER 与时序指标则不受负载影响。
