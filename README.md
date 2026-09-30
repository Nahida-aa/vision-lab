# vision-lab

单仓库，**聚合三个强相关的独立目标**（因共享底层能力——OCR 引擎、抓图、
输入注入——而放在一起，彼此并非包含关系）：

1. **视频字幕识别**（核心方向，`subtitle_ocr`）：从视频帧抽取字幕文字。
   由通用 OCR 能力孵化出的垂直场景——字幕水平、底部、高对比的分布正好契合
   PP-OCR，已沉淀专属后处理（底部 ROI / `subtitle_only` y 过滤 / 按行聚合 /
   时间轴合并 / 离群框过滤）与时间轴合并链路。实现变体
   `packages/subtitle_ocr_cpp`（ONNX Runtime 直连）、`packages/subtitle_ocr_py`、
   以及 `packages/subtitle_ocr`（Rust，已实现 OCR + 后处理 CLI 链）。横比基准见
   `tests/bench/subtitle_ocr`。
2. **GUI 自动化测试**：用 `tools/gen_ui_img`（gpui 真实渲染 → 抓图）生成可复现
   fixture（`tests/fixtures/ui_*.png`），对识别/操作做验证。
3. **GUI 智能操作**：「看屏幕 → 理解 → 操作」闭环。由 `crates/capturer`（抓图）+
   `crates/rapidocr_ort`（文字识别）+ `crates/screen_operator`（点击/输入回灌）组合。

三个方向共用底层 OCR 引擎与抓图/注入设施，但各有独立交付物与验收标准。
视频字幕识别的基准（`tests/bench/subtitle_ocr`）**只服务字幕识别**，不要与
GUI 自动化测试 / GUI 智能操作的验收混淆。详见 [ROADMAP.md](./ROADMAP.md)。

## 组成

| 路径                        | 目标  | 作用                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| --------------------------- | ----- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/rapidocr_ort`       | 1,2,3 | PP-OCR 文字识别（基于 ONNX Runtime）。`det` 检测 + `rec` 识别 + `cls` 方向分类，DB 后处理对齐官方实现。三方向共用的识别引擎，也是视频字幕识别的核心底座。                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `crates/capturer`           | 2, 3  | 屏幕/窗口抓图基础设施。含 `PortalCapturer`（xdg-desktop-portal Screenshot，全屏）与 `ScreenCastCapturer`（ScreenCast + PipeWire，可选窗口且不受遮挡）。                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `crates/screen_operator`    | 3     | 屏幕**操作**层：把识别结果映射成可执行的点击/输入，与「看」侧（`capturer`）正交。后端无关抽象 [`InputBackend`](./crates/screen_operator/src/input_backend.rs)（指针 + 键盘原子原语）+ [`Probe`](./crates/screen_operator/src/probe.rs)（读数），泛型 [`ScreenOperator`](./crates/screen_operator/src/operator.rs) 组合二者、**内含相对移动闭环 `ensure_move_to`**（绕开 Wayland/KWin 下失效的绝对移动 + 加速度过冲，ydotool 虚拟设备加速度由 KWin D-Bus 幂等确保 flat）。桌面实现 = `YdotoolBackend`（ydotool 注入）+ `KwinProbe`（KWin `cursorPos` 读数）。详见 [mousemove.md](./crates/screen_operator/docs/mousemove.md)。 |
| `tools/gen_ui_img`          | 2     | 用 gpui 真实渲染文字卡片，再经 `capturer` 抓图，生成 OCR 测试 fixture。                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `packages/subtitle_ocr_cpp` | 1     | 字幕识别 C++ 实现（ONNX Runtime 直连），含正确性测试 `ocr.test.ts` 与 `test.justfile`。                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `packages/subtitle_ocr_py`  | 1     | 字幕识别 Python 实现（`rapidocr_onnxruntime`）。                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `packages/subtitle_ocr`     | 1     | 字幕识别 Rust 实现（**核心方向**，OCR + 后处理 CLI 链，已实现）。                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `tests/bench/subtitle_ocr`  | 1     | 字幕识别三实现（cpp/py/rust）的横比基准（正确性 `bin/test.rs` + 性能 `bin/bench.rs` 占位）。                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `data/models/rapidocr`      | 共用  | PP-OCR v3 / v6 的 ONNX 权重与字典（被 gitignore，不入库；需本地放置，见下）。                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |

## 识别结果结构

`rapidocr_ort` 对每个文本框输出：

```jsonc
{
  "text": "Count:100",
  "text_confidence": 0.95,   // 识别置信度（rec 分支平均字符概率）
  "box_confidence": 0.80,    // 检测框得分（框内平均概率）
  "box": [[x,y],[x,y],[x,y],[x,y]],  // 四点（顺时针：左上、右上、右下、左下）
  "center": [x, y],     // 四点平均的几何中心，便于点击回灌
  "x_range": [min_x, max_x],   // 横向值域，便于按列/区域过滤
  "y_range": [min_y, max_y]    // 纵向值域，便于按行/区域过滤
}
```

## 构建

```bash
cargo build --workspace
```

依赖：Rust 工具链 + 系统库 `libopencv`（抓图/后处理用到 `imgproc`）+ ONNX
Runtime（`ort` crate 自带）。`capturer` 的 ScreenCast 路径还需要
`libpipewire-0.3`（已在多数 KDE/GNOME 桌面预装）。

## 准备模型权重

`data/models/rapidocr/` 下的权重被 `.gitignore` 排除、**不入库**，需本地放置（从
[PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR) 导出为 ONNX，或团队内部同步）。
代码通过 `--model-dir`（默认 `data/models/rapidocr`，相对仓库根）查找：

- v3：`ch_PP-OCRv3_det_infer.onnx` / `ch_PP-OCRv3_rec_infer.onnx` /
  `ch_ppocr_mobile_v2.0_cls_infer.onnx` / `ppocr_keys.json`
- v6 tiny / medium：`pp-ocrv6_{tiny,medium}_{det,rec}.onnx` + 对应 `.txt` 字典

## 用法

### 1. 对图片跑 OCR

```bash
cargo run -p rapidocr_ort -- --model v3 tests/fixtures/ui_stable1.png
```

输出 JSON，含 `model` / `image` / `width` / `height` 与每个文本框的
`results`（结构见上）。

支持 `--model v3|v6-tiny|v6-medium`，以及 `--model-dir <dir>` 指定权重目录。

### 2. 生成 OCR 测试 fixture

`tools/gen_ui_img` 用 gpui 渲染文字卡片并抓图，产出与 `rapidocr_ort`
输入对齐的 fixture：

```bash
# 批处理全部内置 fixture（每个单独起进程，干净退出）
cargo run -p gen_ui_img

# 单个 fixture
cargo run -p gen_ui_img -- --capture ui_stable1

# 仅渲染、不抓图（窗口常驻 10s 供肉眼核对）
cargo run -p gen_ui_img -- --gui ui_stable1
```

抓图后端为 `ScreenCastCapturer`：首次会弹对话框让你选 gpui 窗口，选完后
`restore_token` 存入 `tests/fixtures/.capture_token`，后续运行免对话框
（即「提前赋权」）。也可 `--token <t>` 显式传入，或 `--reset-token` 强制重选。

> 注意：ScreenCast 抓图需在真实 Wayland/X11 桌面会话中运行，且窗口本身
> 即为卡片（760×260），故抓到的就是窗口本体、无需裁切。

### 3. 直接抓图（不跑 OCR）

```bash
# 全屏
cargo run -p capturer --bin capturer_cli -- full --out capture.png
# 区域
cargo run -p capturer --bin capturer_cli -- region 120 120 720 220 --out capture_region.png
```

## 已知限制

- `capturer` 的 ScreenCast 后端在 OpenCV 5 环境下退化为**轴对齐包围盒**
  （`min_area_rect` 在该组合下无绑定）；近水平文本不受影响，旋转文本支持
  需 OpenCV 4。
- 「识别结果 → 模拟操作」的回灌执行层已实现（`crates/screen_operator`）：`ScreenOperator`
  对外暴露 `ensure_move_to` / `click_at` / `key` 等直觉入口，内部走相对移动闭环（每步读回
  确认）而非失效的绝对移动。实测限制与对策见
  [screen_operator 文档](./crates/screen_operator/docs/mousemove.md)（含绝对移动失效、加速度
  过冲、单步可靠区、KWin D-Bus 设 flat 等）。
