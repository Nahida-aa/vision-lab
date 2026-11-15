# 分阶段归因原始数据（§4.6）

主文档：`docs/ocr/ppocr/benchmark-rust-vs-py-single-frame.md` §4.6。
被测对象、模型、帧、线程与主表完全相同（4 线程、`taskset -c 0-3`、同一帧
`oneframe-replay40/0000000.jpg`，20 次重复 + 预热）。

## 文件

| 文件 | 内容 |
| --- | --- |
| `rust-v{3,4}-r{1,2,3}.json` | Rust `phase_probe` 三轮：`preprocess_det` / `detect_raw_boxes` / `detect` / `load_image` / `roi_copy` |
| `py-v{3,4}-r{1,2,3}.json` | Python `py_phase.py` 三轮：`det_pre` / `det_full`（det 单独跑）/ `engine`（det+cls+rec） |
| `py-det-split-v{3,4}[-arenaon].txt` | Python det 拆成 pre / infer / post（arena 开/关两格） |
| `py-detpre-breakdown-r{1,2,3}.txt` | Python det 预处理内部：resize / normalize / permute / expand+astype |

## 三轮中位（ms/帧，2026-11-15 12:32–12:38，loadavg 2.3→5.5）

| 阶段 | Python v3 | Rust v3 | Python v4 | Rust v4 |
| --- | ---: | ---: | ---: | ---: |
| det 预处理 | 40.1 | 14.4 | 40.6 | 14.5 |
| det 推理 + DB 后处理 | 219.9 | 202.0 | 349.7 | 332.9 |
| cls + 裁剪/resize + rec | 41.7 | 7.9 | 46.2 | 12.7 |
| **engine / detect（不含读图）** | **301.0** | **225.1** | **436.9** | **361.1** |
| + 读图 + ROI（Rust 整帧） | — | 227.8 | — | 363.8 |

> 各阶段在独立循环里各测 20 次，故分项差之和（77.4 / 76.4）与合计差（75.9 / 75.9）
> 差 1–2ms；以合计行为准。Python 的 `engine(roi)` 不含 `cv2.imread`（3–4ms/帧），
> Rust 的 `detect` 不含读图与 ROI 拷贝（已在上表最后一行补上）。

## det 推理单独计时（同会话）

| | pre | infer | post | det_full |
| --- | ---: | ---: | ---: | ---: |
| py v3 arena 关（包内默认） | 38.7 | 217.3 | 1.6 | 263.1 |
| py v3 arena 开 | — | **204.1** | — | 253.2 |
| py v4 arena 关 | 37.4 | 344.2 | 1.4 | 383.8 |
| py v4 arena 开 | — | **329.1** | — | 374.2 |

Rust 的 det 推理 + 后处理为 202.0（v3）/ 332.9（v4），后处理约 2ms。
**arena 对齐后两边 det 推理差 ≤2%**（v4 甚至反向），见主文档 §4.6。
