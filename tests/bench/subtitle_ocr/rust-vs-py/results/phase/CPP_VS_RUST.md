# C++ vs Rust 分阶段原始数据（§4.7，同 ORT 1.28.0）

主文档：`docs/ocr/ppocr/benchmark-rust-vs-py-single-frame.md` §4.7。
目的：回答「同 ORT 1.28.0、同权重 v3、同 4 线程、同 `taskset -c 0-3` 下，
C++ 整帧 ~263ms 为什么比 Rust ~230ms 慢 ~1.15×」。**结论不归给语言**。

## 条件

- 帧：`oneframe-replay40/0000000.jpg`（1280×720，字幕「不对醒来」）；40 帧硬链接重放。
- C++：`build` 目录已 re-configure 到 ORT 1.28.0（`cmake -S ... -B build -DORT_DIR=...1.28.0`）。
- Rust：`phase_probe`，`RAPIDOCR_INTRA_THREADS=4`，`--model v3 --reps 40`，
  各阶段**分开循环**计时（`detect` / `detect_raw_boxes` / `pre` / `load` / `roi`）。
- 全部 `taskset -c 0-3`，三轮连跑取中位；同窗口配对测 det Run 时 C++ 与 Rust 交错跑。
- C++ 探针默认关闭：`OCR_PHASE_MS=1` 逐帧分相、`OCR_PROBE=1` 受控循环，二者都不改默认路径。

## 文件

| 文件 | 内容 |
| --- | --- |
| `cpp-phase-r{1,2,3}.txt` | C++ `OCR_PHASE_MS=1` 三轮 ×40 帧，每行 `[phase] image= detPre= detRun= post= warp= clsPre= clsRun= recPre= recRun=` |
| `rust-phase-r{1,2,3}.json` | Rust `phase_probe` 三轮（与 `cpp-phase-r*` **交错同窗口**） |
| `cpp-probe-r{1,2,3}.txt` | C++ `OCR_PROBE=1` 受控循环三轮：`detArena/detDevice/detFresh/recAlone/recAfterDet` |
| `paired-cpp-r{1,2,3}.txt` | 同窗口配对：C++ 受控探针（纯 det Run + rec 切换对照） |
| `paired-rust-r{1,2,3}.json` | 同窗口配对：Rust `phase_probe`（det Run + 后处理） |

## 三轮中位（ms/帧，2026-11-15，同窗口）

### A. 逐帧分相（C++ `OCR_PHASE_MS` 与 Rust `phase_probe` 交错跑，drop 首帧冷启动）

| 阶段 | C++ v3 | Rust v3 | 差 |
| --- | ---: | ---: | ---: |
| image 读图 | 3.57 | 2.40（load_image p50） | +1.2 |
| det 预处理（resize + 归一化 + CHW） | **21.3** | **14.4** | **+6.9** |
| det 推理 + DB 后处理 | **202.9**（`detRun`，不含 post；post 另 1.25） | **188.7**（`det_infer_post`，含 ~2ms post） | +14.3（见下：配对后实际持平） |
| post（DB + NMS） | 1.25 | （含在上行 Rust 的 ~2ms 内） | — |
| warp + clsPre + clsRun + recPre | 1.19 | — | — |
| rec Run / cls+rec 整段 | **20.0**（`recRun`，紧跟 det） | **9.0**（`cls_plus_rec`） | **+11.0** |
| **分相合计（不含读图）** | **250.1** | **210.0**（`detect` p50） | — |

> C++ `detRun` 与 Rust `det_infer_post` 来自不同测量循环、且 Rust 那项含 ~2ms 后处理，
> **不能直接相减**（见 C 的同窗口配对结论）。

### B. C++ 受控循环探针（`OCR_PROBE=1`，N=40，三轮中位）

| 项 | 含义 | 值 |
| --- | --- | ---: |
| `detArena` | 复用已构造 det 输入 + `OrtArenaAllocator` MemoryInfo，det `Run` | 184.6 |
| `detDevice` | 同上，但用 `OrtDeviceAllocator` MemoryInfo（= Rust `ort` 的 `MemoryInfo::default()`） | 183.8 |
| `detFresh` | 每轮重新 `CreateTensor`（模拟真实流水线每帧重建输入） | 182.5 |
| `recAlone` | rec `Run` 单独反复跑，前面**没有** det，固定 rec 输入 | 5.1 |
| `recAfterDet` | 每轮先 det `Run` 再 rec `Run`，只计 rec | 22.9 |

三条 det 值相差 ≤2ms → **输入 MemoryInfo（arena vs device）、输入是否每帧重建，
都不是 det 推理差的原因**。rec 切换惩罚 = `recAfterDet − recAlone` ≈ **+17.8ms**，
三轮极稳（22.5–23.6 vs 5.0–5.4），坐实 §4.6 的「det→rec 首帧惩罚」。

### C. 同窗口配对：纯 det Run（C++ 探针 vs Rust `phase_probe` 交错，三轮）

| round | C++ `detArena`（纯 det Run） | Rust `det_infer_post`（det Run + ~2ms post） |
| --- | ---: | ---: |
| 1 | 186.0 | 191.8 |
| 2 | 185.9 | 187.9 |
| 3 | 184.4 | 188.9 |
| **中位** | **185.9** | **188.9** |

C++ 纯 det Run 185.9 vs Rust 的 188.9（扣掉 ~2ms 后处理 ≈ 186.9）→ **同窗口内
两者 det 推理持平（差 ≤3ms，噪声级）**。交接文档里「C++ detRun 208.8 vs Rust ~200，
差 ~9ms」是**跨窗口漂移 + 不同测量循环**（一边在真实流水线里、一边在独立循环里）
叠加出来的，不是 C++ 比 Rust 算得慢，也不是 ORT 输入/arena 路径差异。

## 结论（写进 §4.7）

同 ORT 1.28.0 下 C++→Rust ~1.15×（~33ms/帧）拆成：

| 项 | 量级 | 归因 | 是否语言 |
| --- | ---: | --- | --- |
| det 预处理 | +7~8ms | C++ 手写三重 CHW 归一化循环 vs Rust 一趟 SIMD；另 C++ 用 ImageNet 常量（0.485/.../0.229）而 Rust/Python 用 0.5/0.5（`preprocess.rs` 注明曾误用 ImageNet 导致漏检）——**正确性差异**，本帧两者都检出字幕，与速度无关 | ❌ |
| det 推理 | ≈0（同窗口配对持平） | 交接里的 ~9ms 是跨窗口/跨测量循环漂移；输入 MemoryInfo / arena / 每帧重建输入已被受控探针排除 | ❌ |
| rec 切换惩罚 | +11~18ms | det→rec 首帧惩罚，C++/Python 中招、Rust 不中招；受控探针 `recAlone 5.1 → recAfterDet 22.9` 坐实。机制仍疑似 ORT 线程池 spin/唤醒，未做 perf 级归因 | ❌ |
| image 读图 | +1ms | 解码实现差异，量级可忽略 | ❌ |

**不要把 1.15× 归给语言**：三个实现都调同一份 ORT C++ 内核；差距在宿主侧预处理
实现、det→rec 切换首帧惩罚，以及测量口径/窗口漂移。

## 复现

```bash
# C++ re-configure（build 之前曾指向不存在的 build-ort128）
cmake -S packages/subtitle_ocr_cpp -B packages/subtitle_ocr_cpp/build \
      -DORT_DIR="$PWD/packages/tmp/onnxruntime-linux-x64-1.28.0"
cmake --build packages/subtitle_ocr_cpp/build -j8

# 逐帧分相（40 帧，drop 首帧）
taskset -c 0-3 env LD_LIBRARY_PATH="$PWD/packages/subtitle_ocr_cpp/build" \
  OCR_MODELS_DIR=data/models/rapidocr OCR_KEYS_PATH=data/models/rapidocr/ppocr_keys.json \
  OCR_PHASE_MS=1 ./packages/subtitle_ocr_cpp/build/subtitle_ocr_ort_cpp \
  --dir packages/tmp/ocr-bench/oneframe-replay40 0.45 --subtitle-only --threads 4 2>&1 | grep '^\[phase\]'

# 受控循环探针
taskset -c 0-3 env LD_LIBRARY_PATH="$PWD/packages/subtitle_ocr_cpp/build" \
  OCR_MODELS_DIR=data/models/rapidocr OCR_KEYS_PATH=data/models/rapidocr/ppocr_keys.json \
  OCR_PROBE=1 ./packages/subtitle_ocr_cpp/build/subtitle_ocr_ort_cpp \
  --dir packages/tmp/ocr-bench/oneframe-replay40 0.45 --subtitle-only --threads 4 2>&1 | grep '^\[probe\]'

# Rust（与上面交错同窗口）
cargo build --release -p bench_subtitle_ocr --bin phase_probe
taskset -c 0-3 env RAPIDOCR_INTRA_THREADS=4 ./target/release/phase_probe \
  --dir packages/tmp/ocr-bench/oneframe-replay40 --model v3 --reps 40
```
