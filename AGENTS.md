# AGENTS.md

- 调试日志使用 tracing, 调试之后可以不用清除日志
- 修改代码后, 如果认为适合提交, 就自行提交
- 想要在全局 /tmp 中写文件时 可以优先考虑写的项目内的 /tmp 或者更合适的位置

给协作 AI / 自动化代理的入口提示。

## 阅读顺序建议

1. **根部文档**：`README.md`（项目概览）、`ROADMAP.md`（目标与规划）先看大图。
2. **包内文档**：每个 crate 目录下可能有 `README.md` 与 `docs/`（如
   `crates/screen_operator/docs/`，记录 ydotool 的已知坑与修复）。
3. **源码注释**：⚠️ **源码里的 `///` 文档注释时效性最强**，往往比 README / docs / ROADMAP
   更贴近当前实现。遇到 README 与代码不一致时，**以源码注释为准**。改动代码前优先
   读相关 `fn` / `struct` 上方的注释，里面常写着根因、坑、坐标系约定等关键约束
   （例如 `screen_operator` 的鼠标绝对移动在本机 KWin 下失效、须用相对移动模式）。

## 项目结构速览

- `crates/capturer` —— 屏幕抓取（ScreenCast 窗口流 / 全屏）。
- `crates/ocr_layout` —— 截图 → 控件候选（OCR + 颜色/布局分析）。
- `packages/ocr_agent` —— 业务执行层：「看」（识别/定位）+「操作」（点击），两条链路解耦（GUI 智能操作方向的 agent，当前未完善/未入 workspace）。
- `crates/screen_operator` —— 鼠标/键盘输入注入（ydotool 封装），含已知坑文档于 `docs/`。

## 约定

- 看/操作分离：`capturer`（看）与 `screen_operator`（操作）正交，互不直接依赖。
- 分数缩放下坐标要分清**物理像素**（ScreenCast buffer）与**逻辑像素**（KWin 几何 /
  ydotool 相对移动），换算靠 `scale`（窗口流宽 / KWin 逻辑宽）。
