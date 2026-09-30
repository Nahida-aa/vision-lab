# subtitle_finder 与 C++ VideoSubFinder 的对比调查结论

> 目的：记录 Rust `subtitle_finder`（packages/subtitle_finder）与 C++ VideoSubFinder
> 输出差异（Rust 7 段 vs C++ 4 段）的根因调查结论，避免以后重复走弯路。

## 已验证的事实（决定性实验）

### 1. Rust 算法与 C++ 逐像素一致（无算法 bug）
- 用**相同 BGR 帧**分别喂 Rust 和 C++ 的 `get_im_ne`/`get_im_he`：
  - N-edge：C++ 35229 vs Rust 34903（差 <1%）
  - H-edge：C++ 24572 vs Rust 24303（差 <1%）
- Sobel 公式、阈值（mnthr=0.3）、segment 查找、compare 逻辑均已逐行对齐。

### 2. ⚠️ C++ cli 分支有帧同步 bug（重要陷阱）
- `FastSearchSubtitles` 里 `GetTransformedImage` 的 `ImBGR` 与 `ImY` 可能非同一帧
  （并发 `AddGetRGBImagesTask`/`AddConvertImageTask` 覆盖缓冲）。
- 基于 cli 分支 dump 的对比**不可靠**（BGR 16% 一致等数据是假的）。
- 可靠对比必须：独立解码（OpenCV VideoCapture 直接读 vs Rust ffmpeg）+ 相同输入喂两边。

### 3. 解码器色彩差异（已定位但非主因）
- Rust 的 ffmpeg scaler 用 **bt601**（`sws_getContext` 默认），视频实际是 **bt709**。
- OpenCV / ffmpeg CLI 用 bt709。
- Rust 与 OpenCV 的 BGR 仅 ~44.9% 像素一致（差 ±1-4）。
- 手动实现 bt709 转换后 BGR 匹配率提升到 ~55%，**但段数反而从 7 → 10（更差）**。
- **结论：解码器色彩差异不是过度切分的主因。**

### 4. 调阈值无效
- 提高 veple 0.30→0.60：段数反而更多（8），边界移动。
- 说明过度切分不是阈值边界敏感，是结构性差异。

## 当前结论
- 过度切分（7 vs 4）**不是**：
  - 边缘检测算法 bug（已证一致）
  - 解码器色彩差异（bt709 反而更差）
  - 阈值边界（调阈值更差）
- **真正根因（重大发现）：`g_text_alignment` 默认是 `Center`，不是 Any！**
  - C++ `IPAlgorithms.cpp:170`：`TextAlignment g_text_alignment = TextAlignment::Center;`
  - Rust 实现假设了 Any（params.rs / preprocess.rs / filter.rs 无 alignment 概念）。
  - 相同 BGR 对比：C++ FF=58005 SF=**4579** TF=6535；Rust FF=57915 SF=**54157** TF=54459。
    - FF/NE 完全一致（输入算法对齐），但 SF 天差地别：C++ 清理 92%，Rust 只清理 6%。
  - Center 路径在 `SecondFiltration` 有额外清理：合并/移除偏离中心的段、`mpd` 最小点密度
    检查（S < mpd*SS 时移除最远段）、`mpned` 最小边缘密度检查（nNE < mpned*S 时移除）。
    Any 路径跳过这些，导致 Rust 无法清理噪声 → ISA 过密 → im_res 过密 → compare 过度敏感。

## ✅ 已实现 Center 对齐（filter.rs second_filtration）
- 已实现完整 Center 路径：`is_too_right` / `farthest_from_center` helper + 逐条带 `while(1)`
  迭代，含段合并（btd_max）、中心偏移移除、`mpd` 最小点密度、`mpned` 最小边缘密度检查。
- 效果：SF 从 54157 → 2103（C++ 4579），TF 从 54459 → 6984（C++ 6535）。
- **段数 7 → 4，与 C++ 对齐**（提交 5c10a58）。

## ✅ 段边界完全对齐 C++（修复 get_intersect_images + EOF 保存）
- 之前误判「末尾段时间差是 C++ 异步时序」——实际是 **Rust 自己的 bug**：
  1. `get_intersect_images` 把空字幕帧（has_text=0，如 fn=105）也加进交集，把交集清空
     → analyse false → 段3 提前在 fn=100 (3332) 结束，尽管 fn=100-104 有字幕。
     修复：只交集 has_text=1 的帧 → 段3 延伸到 fn=104 (3499)。
  2. 状态机外层 `fn_ >= count` 的 break 直接退出，没保存进行中的末尾段 → 段4 丢失。
     修复：EOF break 时保存进行中的段（ef/et 定为最后一帧）。
- **验证**：用 `subtitle_ocr` OCR 确认 fn=104 有字幕（"这可是剑仙啊"）、fn=105 空；
  C++ ISA 图段3="这可是剑仙响"、段4="不行我得出手了"。两边 has_text 判定一致（Rust/C++ TF 一致）。
- **结果**：Rust `132-932, 932-2265, 2266-3499, 3700-5033`，与 C++ `133-932, 933-2265,
  2266-3499, 3700-5032` **完全一致**（差 ≤1ms 为解码器帧时序）。

## 现状
- Rust 当前输出：4 段
  `132-932, 932-2265, 2266-3499, 3700-5033`
- C++（cli 分支）：4 段
  `133-932, 933-2265, 2266-3499, 3700-5032`
- **完全对齐**（差 ≤1ms 为解码器帧时序）。
- 23 单测通过，workspace 干净，frame.rs 用 bt601 scaler（未改）。

## 排查工具备忘
- Rust 调试日志：`RUST_LOG=subtitle_finder=trace`（decode frame isa_wc / 边缘图白点 /
  dilate(NE) / second_filtration step / compare 详情 / 内容变化判定帧 / save_keyframe）。
- C++ 独立对比程序：cli 分支写 `edge_dump`（读固定 BGR → GetImNE/GetImHE → dump）、
  `tf_dump`（读固定 BGR → GetTransformedImage → FF/SF/TF/NE），需 stub
  `g_ReportFileName`/`GetFileNameWithExtension` + 链接 MyClosedFigure.o。
- 用 ffmpeg CLI 转 BGR 作为 OpenCV 的可靠参照（默认 bt709 与 OpenCV 100% 一致）。
- 关键结论已反复验证：用相同 BGR 喂两边才能可靠对比（cli 分支 FastSearchSubtitles 有帧同步 bug）。

## ⚠️ 新发现（2026-08-06）：subtitle_finder 对长视频内存爆炸（OOM）
- `FrameCache::decode_all` 把所有帧的 `FrameData`（bgr 2.7MB + im + ne + y ≈ 6.2MB/帧）解码到内存。
- 170s / 30fps = 5100 帧 ≈ **30GB**，超出环境内存（27GB / cgroup 限制更低）→ 进程被 SIGTERM(143) 终止。
- 现象：`find_keyframes` / `sf_ocr` 对 video_source.mp4 约 58s 后静默退出，连"找到 N 个关键帧"都没打印。
- **对比意义**：传统抽帧（`extract_frames` 用 ffmpeg 流式）不全部驻留内存，可处理长视频；subtitle_finder 全量解码是资源瓶颈。这正是"sf 关键帧 vs 传统抽帧"性能对比的核心差异之一。
- **待办**：把 decode 改成滑动窗口/流式（状态机 fn 单调递增，只需 DL 窗口 + 少量回溯），避免全量驻留，才能跑完整 170s 对比。
