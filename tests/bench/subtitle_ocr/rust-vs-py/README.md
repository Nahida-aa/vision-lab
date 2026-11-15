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

# 只跑 cpp（三个 ORT 版本构建，轮内轮换顺序以摊平负载漂移）
bash run.sh 3 cpp

# 出表（同时写 results/summary.md）
python3 summarize.py | tee results/summary.md
```

## cpp 的 ORT 版本实验

同一份 `ocr_pipeline.cpp` 分别链接 `packages/tmp/` 下解压的三个 ORT，各自独立构建目录
（**必须**显式 `-DORT_DIR`，否则 CMake 会自动挑最新的一份，标签就会名不副实）：

```bash
cmake -S packages/subtitle_ocr_cpp -B packages/subtitle_ocr_cpp/build \
      -DORT_DIR="$PWD/packages/tmp/onnxruntime-linux-x64-1.24.4"
cmake -S packages/subtitle_ocr_cpp -B packages/subtitle_ocr_cpp/build-ort128 \
      -DORT_DIR="$PWD/packages/tmp/onnxruntime-linux-x64-1.28.0"
cmake -S packages/subtitle_ocr_cpp -B packages/subtitle_ocr_cpp/build-ort132 \
      -DORT_DIR="$PWD/packages/tmp/onnxruntime-linux-x64-1.32.0"
cmake --build packages/subtitle_ocr_cpp/build -j8
cmake --build packages/subtitle_ocr_cpp/build-ort128 -j8
cmake --build packages/subtitle_ocr_cpp/build-ort132 -j8
```

结果标签 `cpp-v3-ort124 / -ort128 / -ort132` 对应 ORT 1.24.4 / 1.28.0 / 1.32.0。

## 文件

| 文件 | 说明 |
| --- | --- |
| `create_frames.sh` | 单帧 ×40 硬链接目录（默认取 `0020000.jpg`） |
| `run.sh` | 跑 rust v4/v3、cpp v3 的三个 ORT 版本构建、python 批量、python one-shot；第二个参数可只跑一类（`cpp`/`rust`/`py`） |
| `monitor.py` | 子进程峰值（`ru_maxrss`）/ 平均（`/proc` 采样）RSS |
| `py_batch.py` | python 驱动：批量常驻 engine / 每帧一进程；`--models v3` 指仓库 v3 权重（字典自动转 txt）；`--arena-on` 是归因诊断开关 |
| `summarize.py` | 出 avg/P50/P95 + 内存表 |
| `results/` | 原始 JSON、cpp stdout/stderr、`summary.md`、`threadpool-check.txt`、`cpp-text-check.txt` |
| `../src/bin/probe.rs` | Rust 单帧计时探针（调 `subtitle_ocr::ocr_entry`，不改实现逻辑） |

## 口径要点

- 逐帧耗时 = 读图解码 + det + 后处理 + cls + rec + 聚合；**不含**进程启动、模型/session
  加载、抽帧。
- cpp 的逐帧值 = `imageLoadMs + detMs + postMs + recMs`（剔除 `modelLoadMs`，它的
  `totalMs` 每帧都重复计入模型加载，会虚高 ~110ms）。
- python 批量 vs one-shot 是**两种调用方式**，不是同一口径的两个数字。
- **三个实现都显式钉 4 线程**：rust `RAPIDOCR_INTRA_THREADS=4`、cpp `--threads 4`、
  python `intra_op_num_threads=4`。rust 不钉会走 ORT 默认全局池（本机 16 线程），在
  `taskset -c 0-3` 内超订，同一二进制同一帧从 366ms 劣化到 638ms（`threadpool-check.txt`）。
- 内存要**同 arena 配置**比：`RAPIDOCR_CPU_ARENA=0` 让 Rust 走 `DisableCpuMemArena`
  （等价 Python 包内的 `enable_cpu_mem_arena=False`）；不设则走 ORT 默认（arena 开）。
- 速度也受 arena 影响：开不比关慢（0.5–15%，跨会话幅度不稳）。速度优先时取 arena 开，该格
  Rust 与 Python 的 v4/v3 两套权重都是「更快 + 峰值更低」。
- ORT 版本也是变量：cpp 的三个构建分别链接 1.24.4 / 1.28.0 / 1.32.0，而 rust/python
  的 ORT 是 1.28.0。跨实现比速度时优先用 `cpp-v3-ort128`（同 ORT 版本），
  `cpp-v3-ort124` 只用于「升级 ORT 值不值得」这一问（结论：单帧不提速，1.32 只省初始化）。
