# 对齐 C++ VideoSubFinder 的经验与结果

> 记录 Rust `subtitle_finder` 与 C++ VideoSubFinder 输出（关键帧段）对齐的排查经验、
> 验证方法、最终结果与坑。目标：让后续避免重复走弯路。

## 一、最终结果（已对齐）

| 段 | Rust `subtitle_finder` | C++ `cli` 分支 |
|---|---|---|
| 段1 | 132-932 | 133-932 |
| 段2 | 932-2265 | 933-2265 |
| 段3 | 2266-3499 | 2266-3499 |
| 段4 | 3700-5033 | 3700-5032 |

- **段数、段边界完全一致**（差 ≤1ms，为解码器帧时序）。
- 测试视频：`/tmp/clip5s.mp4`（5s，30fps，720p，152 帧）。

## 二、发现的三个根因（按排查顺序）

### 1. `g_text_alignment` 默认是 `Center`，不是 Any（最关键）
- **C++ `IPAlgorithms.cpp:170`**：`TextAlignment g_text_alignment = TextAlignment::Center;`
- Rust 实现假设了 Any（params.rs / filter.rs 无 alignment 概念）。
- Center 路径在 `SecondFiltration` 有额外清理：段合并（btd）、中心偏移移除、`mpd`
  最小点密度（S < mpd·SS 时移除）、`mpned` 最小边缘密度（nNE < mpned·S 时移除）。
  Any 跳过这些 → Rust 无法清理噪声 → ISA 过密 → `im_res` 过密 → compare 过度敏感
  → **过度切分（7 段 vs 4 段）**。
- **修复**：`filter.rs::second_filtration` 实现完整 Center 路径
  （`is_too_right`/`farthest_from_center` + 逐条带 `while(1)` 迭代）。
- 效果：SF 从 54157 → 2103（C++ 4579），TF 从 54459 → 6984（C++ 6535）。段数 7→4。

### 2. `get_intersect_images` 交集被空字幕帧清空（段提前结束）
- 窗口 [fn..fn+DL-1] 里若有 `has_text=0` 的空帧（如 fn=105），其全 0 像素把交集清空
  → `analyse_image_flat` 判 false → 段提前结束（段3 从 3499 提前到 3332），
  尽管 fn=100-104 都有字幕。
- **修复**：只交集 `has_text=1` 的帧，跳过空字幕帧。段3 修复到 3499。

### 3. EOF break 不保存末尾段（段丢失）
- 状态机外层 `fn_ >= count` 的 `break` 直接退出循环，没保存进行中的末尾段
  → 段4 丢失。
- **修复**：EOF break 时保存进行中的段（ef/et 定为最后一帧）。

## 三、可靠的验证方法（关键经验）

### ⚠️ 不要用 C++ cli 分支的 `FastSearchSubtitles` dump 做对比
- 它有**帧同步 bug**：`GetTransformedImage` 的 `ImBGR` 与 `ImY` 可能非同一帧
  （并发 `AddGetRGBImagesTask`/`AddConvertImageTask` 覆盖缓冲）。
- 基于它的 dump（BGR 16% 一致等）**不可靠**。

### ✅ 可靠方法：用**相同 BGR 输入**分别喂两边算法
- C++ 独立程序（cli 分支临时写 `edge_dump`/`tf_dump`）：
  - `edge_dump`：读固定 BGR → `GetImNE`/`GetImHE` → dump N/H edge。
  - `tf_dump`：读固定 BGR → `GetTransformedImage` → FF/SF/TF/NE 白点。
  - 需 stub `g_ReportFileName`/`GetFileNameWithExtension` + 链接 `MyClosedFigure.o`。
- Rust 侧用 `#[test] #[ignore]` 读同一 BGR 算对应输出。
- **相同 BGR 时 Rust 与 C++ 的 N/H edge、FF、NE 完全一致（差 <1%）**，证明算法无 bug。

### ✅ 用 ffmpeg CLI 作为 OpenCV 的可靠参照
- `ffmpeg -i clip -frames:v 1 -f rawvideo -pix_fmt bgr24 out.raw`（默认 bt709）
  与 OpenCV VideoCapture 输出 **100% 一致**。
- Rust 的 ffmpeg scaler 默认用 bt601（`sws_getContext` 默认），与 bt709 差 ±1-4，
  但**不影响段数**（尝试 bt709 反而更差，说明不是主因）。

### ✅ 用 `subtitle_ocr` 验证具体帧有无字幕
- `cargo run -p subtitle_ocr --release -- frame.png --subtitle-only`
- 确认 fn=104 有字幕（"这可是剑仙啊"）、fn=105 空；C++ ISA 段3="这可是剑仙响"、段4="不行我得出手了"。
- Rust 与 C++ 的 `has_text`/`TF` 判定在相同帧上**完全一致**。

## 四、坑与注意事项

1. **C++ 全局参数默认值要认真核对**（不只 params.rs 里列的那些）。`g_text_alignment`
   是 Center 而非想当然的 Any，是最隐蔽的坑。
2. **调试日志用 tracing**（`RUST_LOG=subtitle_finder=trace`），`eprintln!` 只在测试里用。
   tracing 输出带 ANSI 颜色码，`grep frame=100` 会匹配不到——先 `sed -r 's/\x1b\[[0-9;]*m//g'`
   去色。
3. **不要在未提交的文件上用 `git checkout -- <file>`**：会整文件回退，丢失未提交工作
   （本会话曾误删 state.rs 的完整状态机，后从 C++ `FastSearchSubtitles` 重新移植）。
4. `get_intersect_images` 的 bln 语义要与 C++ `AND(每帧 has_text)` 对齐：空字幕帧
   （has_text=0）不应参与交集。

## 五、相关提交

- `5c10a58` — second_filtration 实现 Center 路径（段数 7→4）
- `1ba7a5a` — get_intersect_images 跳过空字幕帧 + EOF 保存末尾段（段边界完全对齐）
- `af26cdd` — 调查记录（`.agents/subtitle-finder-cpp-diff.md`）

## 六、已解决：幽灵带 → 丢字幕段（A/B 框架二分定位）

**症状**：Rust 漏字幕段 / 过度切分，跨视频复现：
- 大/13：第一个"走吧"（11666-12465ms）丢失（C++ 有，Rust 无）
- 大/11：末尾段 56600-58032 被切成两段（C++ 一整段）

**根因（最终定位）**：`second_filtration` 的 btd 段合并分支（两段距离 > btd_max 时
移除偏离中心段）**误用 `farthest_from_center`**（返回**全局**首/尾段 0 或 ln-1），
而 C++ 移除**相邻两段之一**（l 或 l+1，SSAlgorithms.cpp:2037-2058 的 Center 判定）。
导致 Rust 移除错误段 → 顶部内容过度清除（row 24 段数 18→2 vs C++ 18→9）→
TF 顶部 26-44 差异 → `get_lines_info` 幽灵带（compare2 cmb=0）→ 段不稳定。

**修复**：merge 分支改用 C++ 的 l/l+1 Center 判定（`is_too_right` + val1/val2/offset）。
提交 `8de7302`。

**A/B 验证框架**（定位工具）：
- Rust `src/bin/ab_dump.rs` + C++ `cli/ab_dump.cpp`：跑 `get_transformed_image` 各阶段，
  dump 指纹（区域白点 + 校验和）+ raw 数组，喂同一帧 BGR。
- `scripts/ab_compare.py`：逐阶段对比，报首个差异阶段 + 像素坐标。
- 二分：y/u/v/ff/sf/ne0/he/ne/sf_step1 全一致 → 首个差异在 `filter_transformed_image` →
  逐步 top26 → `second_filtration` 段合并分支。修复后所有阶段指纹与 C++ 完全一致。

**前置修复**：解码 OpenCV VideoCapture（bt709）+ `bgr_to_yuv` 用 `cvtColor`（`08b5307`/`4443773`）。

**已排除**：`get_lines_info`/`compare2`/`intersect_y_images`/`second_filtration` 其余逻辑
与 C++ 逐行一致；`analyse_image` 改 Center 无影响；decoder 色彩非最终根因。

## 七、已解决：39466 误段（Center 右半屏清理遗漏）

**症状**：大/11 多出一个 `39466,39733` 段（OCR 验证为 AD/BB 背景噪声，tc 0.54/0.91），
C++ 没有此段。Rust 段数 24 vs C++ 23。

**定位方法（关键）**：对 `analize_for_sub_presence`/`AnalizeImageForSubPresence` 逐层做
原始数组对比，确认**输入完全一致**、差异纯粹在 `second_filtration` 内部：
- `im_int_sp`/`ImIntSP` **逐字节一致**（1158 白点，CPP_IMINTSP + raw dump）
- step1 `im_sf`（`isa ∩ ila ∩ dilate(NE)`）**逐字节一致**（421 白点）
- C++ `second_filtration` 返回 0（sf 清 0），Rust 返回 1（保留 158 白点）

**根因**：`second_filtration` 漏了 C++ `SecondFiltration` 的一个 **Center 右半屏清理分支**
（IPAlgorithms.cpp:2472-2484）：当 `g_text_alignment==Center` 且 `lb[0] >= real_im_x_center`
（段首 x 在水平中心线右侧）时**整带清除**。Rust 在 mpned 循环后直接进 `ln==ln_orig` 判定，
漏了这一步 → 右半屏孤立噪声带被保留 → `second_filtration` 返回 1 → 39466 段被误保存。
C++ 把 rows 294/297 的右半屏噪声带整带清除 → 返回 0（跳过）。

**修复**：`filter.rs::second_filtration` 在 mpned 循环后、`ln==ln_orig` 判定前补 Center
右半屏检查（`if seg_lb[0] >= real_im_x_center { 整带清除; ln=0; break; }`）。提交 `e0472d7`。

**验证**：大/11 段数 24→23（对齐 C++），39466 消失；大/13 段数 33（对齐 C++），
"走吧"与末尾段均正常。

> 与第六节的幽灵带同属 `second_filtration` Center 路径的**分支遗漏**（一个漏 btd 合并的
> l/l+1 判定，一个漏右半屏整带清除）。排查这类问题优先对比完整 Center 路径的每个分支。

**相关提交**：
- `e0472d7` — 补 second_filtration 的 Center 右半屏清理，修复 39466 误段
- `5c2de80` — second_filtration 注释补充右半屏清理到 Center 路径清单
