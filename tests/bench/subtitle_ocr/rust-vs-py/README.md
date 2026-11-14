# rust-vs-py：单帧耗时 / 内存实测驱动

对应文档：[`docs/benchmark-rust-vs-py-single-frame.md`](../../../../docs/benchmark-rust-vs-py-single-frame.md)

只测**同一帧**（不跑全视频），用于回答「Rust 相对 Python 快多少、省不省内存」。
与 `just bench`（全视频、`--dir` 墙钟）是两套口径，不要混用。

## 用法

```bash
# 从 bench 的 2fps 抽帧目录里取一帧，硬链接成 40 个名字（同一 inode）
bash create_frames.sh

# 三实现 × 3 轮，taskset 固定 4 CPU
bash run.sh 3

# 出表（同时写 results/summary.md）
python3 summarize.py | tee results/summary.md
```

## 文件

| 文件 | 说明 |
| --- | --- |
| `create_frames.sh` | 单帧 ×40 硬链接目录（默认取 `0020000.jpg`） |
| `run.sh` | 跑 rust v4/v3、cpp v3、python 批量、python one-shot |
| `monitor.py` | 子进程峰值（`ru_maxrss`）/ 平均（`/proc` 采样）RSS |
| `py_batch.py` | python 驱动：批量常驻 engine / 每帧一进程；`--arena-on` 是归因诊断开关 |
| `summarize.py` | 出 avg/P50/P95 + 内存表 |
| `results/` | 原始 JSON、cpp stderr、`summary.md` |
| `../src/bin/probe.rs` | Rust 单帧计时探针（调 `subtitle_ocr::ocr_entry`，不改实现逻辑） |

## 口径要点

- 逐帧耗时 = 读图解码 + det + 后处理 + cls + rec + 聚合；**不含**进程启动、模型/session
  加载、抽帧。
- cpp 的逐帧值 = `imageLoadMs + detMs + postMs + recMs`（剔除 `modelLoadMs`，它的
  `totalMs` 每帧都重复计入模型加载，会虚高 ~123ms）。
- python 批量 vs one-shot 是**两种调用方式**，不是同一口径的两个数字。
