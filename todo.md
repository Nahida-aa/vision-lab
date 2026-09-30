# 当前 TODO（细项，勾选式）

> 大方向与架构见 [ROADMAP.md](./ROADMAP.md)。本文件只列「接下来具体做哪几件」，
> 避免路线图被长清单拖垮、失真。勾选项保留近期已完成的 `[x]` 作为进度感，定期清理。

## 方向 1：视频字幕识别（核心，subtitle_ocr Rust）— 主体已实现，收尾中

- [x] subtitle_ocr Rust 实现：OCR 引擎 + 后处理 CLI 链
      （ocr-frames-adjust/filter-box、merge-frames、ocr-segment-adjust/filter）
- [x] 进度条化（indicatif，独立 stderr）+ 关 ORT 噪声（ort::logging=error）+ `--out` 指定时不再向 stdout 重复打印整份 JSON
- [x] 修正 README/ROADMAP 里 subtitle_ocr「待实现」过期描述 → 已实现
- [ ] v3 识别掉字调参（扩张比例 / rec 输入高度 / 双线性），目标不丢字
      （环境变量 `OCR_EXPAND` 可覆盖扩张比例调试）
- [ ] subtitle_finder 长字幕段丢失（track 段保存失败）：
      Any-skip 修复（跳过对齐模式段清理）后 has_text 在 17-20.5s 稳定为 1
      （符合 VideoSubFinder），但状态机 detect 到帧 526 段起始后未保存该段
      （15-24s 全丢）。已定位问题在 run_state_machine 的 track 段结束/保存逻辑，
      需对比 FastSearchSubtitles 段保存判定（`ef-bf+1` / `analize_for_sub_presence`）
      （详见 `packages/subtitle_finder/DESIGN.md`「已知局限」）
- [ ] 补 v6 rec 预处理（当前占位 0.5/0.5，识别不准，标记实验性）
- [ ] cls 方向分类接入（已加载未使用，旋转文本待支持）
- [ ] 三实现横比基准：`tests/bench/subtitle_ocr` 的 `bin/bench.rs` 性能占位补实

## 方向 2：GUI 自动化测试

- [ ] opencv 视觉层：版面 / 图标 / 状态识别（文字层补充）
- [ ] (可选) yolo 控件检测，降低纯 OCR 误判

## 方向 3：GUI 智能操作

- [ ] ui_probe：OCR 结果 → 操作回灌最小闭环（waydroid 截图 → OCR → 断言）

## 跨仓维护

- [ ] 确认 `data/models/rapidocr` 权重交付方式（当前 `.gitignore`，本地需放置；
      考虑 sync 脚本 / 下载说明，避免协作者缺权重）
