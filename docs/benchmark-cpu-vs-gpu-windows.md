# OCR 基准 — CPU vs GPU（DirectML），Windows 本机

复刻 [benchmark-2fps-4-engines.md](./benchmark-2fps-4-engines.md) 的实验设置，
在同一台 Windows 笔记本上对比 rust 实现（`subtitle_ocr`）的 **CPU 执行**与
**DirectML（GPU）执行**。日期：2026-10-01。

## 摘要

DirectML 在 RTX 3060 上比同机 CPU 快 **5.4~5.9×**，且**输出与 CPU 逐位一致**
（文本、置信度、段结构、全部质量指标相同）——对该管线而言 GPU 后端可以无差别
替换 CPU。2fps 抽帧下 RTF 从 1.7~1.8（无法实时）降到 **0.31**（富余 3× 实时余量）。

| 运行 | 执行后端 | 推理总耗时 | 平均/帧 | RTF |
| ---- | -------- | ---------: | ------: | ---: |
| run 1 | CPU（主 bin `subtitle_ocr`） | 310.3 s | 910 ms | 1.825 |
| run 2 | CPU | 286.8 s | 841 ms | 1.687 |
| run 1 | DirectML（专用 bin `directml`，RTX 3060） | 52.5 s | 154 ms | 0.309 |
| run 2 | DirectML | 52.9 s | 155 ms | 0.311 |

加速比（同序号运行相除）：5.91× / 5.43×。CPU 两轮波动 7.6%（在 README 记录的
空载波动范围内）；DirectML 两轮几乎重合（<1%）。

**质量指标（四轮全部逐位一致）**：

| 指标 | 值 |
| ---- | -- |
| CER(raw) / CER(norm) | 1.40% / 1.43% |
| CER(paired) | 0.18% |
| paired / missed / spurious | 75 / 0 / 5 |
| zero-dur / split / merged | 6 / 0 / 0 |
| IoU(mean) | 0.6742 |
| start Δms（mean/median/p95） | +221 / +230 / 470 |
| end Δms（mean/median/p95） | −37 / −30 / 470 |
| 段数 / hyp_chars / ref_chars | 80 / 579 / 572 |

- start Δ ≈ +221ms 与 README 记录的 fps=2 采样分辨率局限（半帧期望损失）一致，
  印证 GT 对齐正常。
- CPU 与 GPU 的输出逐位一致意味着 DirectML 的浮点实现对该模型组没有引入
  可观测的数值漂移——质量指标一列即两种后端共用。

## 实验设置

与 4 引擎基准相同：170.1s 内部参考视频（sha256 见
`tests/bench/subtitle_ocr/README.md`）→ ffmpeg `select='not(mod(n,15))'` @2fps
→ **341 帧**；PP-OCR **V4**（`--model` 默认），`--subtitle-only`，
`--text-confidence-threshold 0.45`，`--warp-crop`（cpp 同款透视矫正裁剪），
`--dir` 批量模式（单进程、模型只加载一次）。

本机环境：

| 项 | 值 |
| -- | -- |
| CPU | Intel i5-11400H（6C/12T） |
| GPU | NVIDIA RTX 3060 Laptop 6GB（驱动 566.07）+ Intel UHD + Todesk 虚拟显示适配器 |
| OS | Windows（MSVC 工具链，VS2019 14.29） |
| rust | stable 1.97.1（`.cargo/config.toml` +AVX2） |
| OpenCV | 本地官方 prebuilt **4.10.0**（CI 用 5.0.0，见下「构建差异」） |
| ONNX Runtime | MS 官方 DirectML nuget **1.24.4**（`onnxruntime.dll`，动态链接，API 17） |
| 引擎线程数 | OcrEngine 默认（4 intra threads，与 cpp 默认一致） |

## 复现

```bash
# 1. 构建（VS2019 工具链须走 ort 动态链接形态，见「构建差异」）。
#    产物两个 exe：subtitle_ocr.exe（CPU 主 bin）+ directml.exe（DirectML 专用
#    bin，无条件编译；实际跑 DirectML 需开 ep-directml feature，缺 feature 的
#    产物运行时硬报错）。
export ORT_LIB_LOCATION=<含 onnxruntime.lib 的目录>   # MS DirectML nuget 解包
export ORT_PREFER_DYNAMIC_LINK=1
cargo build --release -p subtitle_ocr --bin subtitle_ocr --bin directml \
  --no-default-features --features ep-directml
cargo build --release -p bench_subtitle_ocr --bin bench --no-default-features

# 2. 跑基准（exe 须放在仓库根下两级目录内，模型目录按 exe 位置解析；
#    --ep 选后端 → 自动选 exe：cpu → subtitle_ocr，directml → 同目录 directml.exe，
#    也可 --directml-bin 显式指定）
./target/release/bench.exe --impl rust --dir --warp-crop --ep cpu \
  --rust-bin packages/tmp/subtitle_ocr.exe
ORT_EP_DEVICE_ID=1 ./target/release/bench.exe --impl rust --dir --warp-crop \
  --ep directml --rust-bin packages/tmp/subtitle_ocr.exe
```

结果写至 `packages/tmp/ocr-bench/<label>/metadata/{ocr,summary}.json`
（label：`ocr-rust[-directml]-fps2-so-ts0.45`）。

## 后端分离设计（DirectML 专用 bin）

CPU 与 DirectML 不共用 CLI 旗标——后端差异在**二进制层面**分开：

| bin | 后端 | 编译条件 | meta 溯源 |
| --- | ---- | -------- | --------- |
| `subtitle_ocr` | CPU（固定，无 GPU 旗标） | 默认 | engine `ort-rust` / device `Cpu` |
| `directml` | DirectML（固定） | 无条件编译；跑 GPU 需 `--features ep-directml`，缺 feature 运行时硬报错 | engine `ort-rust-directml` / device `Directml` |

两个 bin 的旗标集一致，解析后的执行流程共用（`subtitle_ocr::cli` 模块：
建引擎 → 建条目 → 逐条 OCR → 落盘/打印），差异收敛在 `RunArgs` 的三个溯源
字段上。`rapidocr_ort` 库层的 `from_profile_with_backend` / `ExecutionBackend`
是共用底座，但任何 CLI 都不暴露「选后端」的旗标。

## ⚠️ 多适配器机器的设备陷阱（`ORT_EP_DEVICE_ID`）

本机有 **3 个 D3D12 适配器**，DirectML 的 device id 枚举顺序实测为：

| device id | 适配器 | 实测 |
| --------- | ------ | ---- |
| 0 | Todesk 虚拟显示适配器 | **63 s/帧**（软件渲染/WARP，灾难级） |
| 1 | **RTX 3060 Laptop** | 154 ms/帧 ✓ |
| 2 | Intel UHD | 明显慢于 3060（数百 ms/帧量级） |

- **默认 device 0 落在虚拟适配器上时不会报错**，只是慢 400 倍——非常具有迷惑性。
- 多适配器机器上务必显式指定 `ORT_EP_DEVICE_ID`，并用显存/利用率确认 GPU
  真的在算（`nvidia-smi --query-gpu=memory.used,utilization.gpu --format=csv,noheader -l 2`，
  运行期间显存应跳变数百 MB）。
- CUDA 不可用的原因：ort rc.13 的预编译 CUDA 发行版按 **CUDA 13** 构建，本机
  驱动（566.07，支持到 CUDA 12.7）无法加载；DirectML 无此约束。

## 构建差异（本机 VS2019 vs CI VS2022）

- **ort 静态预编译库链接不上**：pyke `download-binaries` 的静态库按 VS2022 STL
  构建（引用 `__std_max_8u` 等 SIMD 符号），VS2019 (14.29) 链接器缺这些符号
  （LNK2019 ×40）。故 workspace 统一 `default-features = false` 关掉 ort 默认
  （含 `api-27`），用 feature 层级还原两种形态：
  - 常规（CI）：`cargo build -p subtitle_ocr` → `ort-default` → ort 默认
    （静态链接 + api-27），行为与改动前完全一致；
  - 本机（VS2019）：`--no-default-features` → 仅请求 ORT API 17 +
    `ORT_LIB_LOCATION` / `ORT_PREFER_DYNAMIC_LINK=1` 动态链接（MS nuget 的
    `onnxruntime.lib` 是 2.8KB 导入库）。DirectML nuget 的 DLL 自带 CPU EP，
    故 CPU / DirectML 共用一个 DLL（各用专属 bin，见「后端分离设计」）。
- **OpenCV 4.10 兼容**：本机无 OpenCV 5 prebuilt（GitHub 直连下载过慢），本地
  4.10 的 `warp_perspective` 没有 OpenCV 5 新增的第 8 参 `AlgorithmHint`。
  `packages/rapidocr_ort/build.rs` 从头文件解析主版本发
  `ocvrs_opencv_branch_5` cfg，`src/pipeline.rs` 按 cfg 分流 7/8 参——语义等价
  （4.x 默认行为即 5 的 `ALGO_HINT_ACCURATE`）。
- opencv crate 绑定生成需要 clang：本机无管理员权限装 LLVM，用 TUNA 镜像的
  LLVM 23.1.2 tar.xz 免安装解压（`PATH` + `LIBCLANG_PATH`），并用
  `CPATH`/`CPLUS_INCLUDE_PATH` 注入 Windows SDK 的 UCRT/um/shared 头
  （VS2019 的 STL 头 `cstdio` 指向 UCRT）。

## 与 4 引擎历史基准的关系

**不可横向对比机器差异**（历史：linux + 更强 CPU，cpp 355ms/帧 RTF 0.717）。
本机 CPU 侧 841~910ms/帧（RTF 1.69~1.82）只说明 6 核笔记本 CPU 跑不动实时，
不代表实现回归。质量指标与历史数字也不完全一致（本机 spurious 5、CER(norm)
1.43%；历史 warp-on 为 0 spurious、0.36%）——OpenCV 4.10/5.0 与 ffmpeg 8.1
抽帧的差异足以移动这些边界数字。本实验的结论只取**同机 CPU vs GPU 的内部对比**
（后端切换不改变任何质量指标），这部分是严格成立的。

## 本次顺带修复的 bench 缺陷（rust 路径此前从未真正跑通过）

1. `bench.rs` 给 rust CLI 传 `--text-score`，而 rust 旗标是
   `--text-confidence-threshold` → 调用必失败（此前 rust 基准结果只能来自
   更早版本的旗标名）。
2. rust `--dir` 要求文件名符合 `ms`/`ms_ms` 时间约定，ffmpeg 抽帧默认
   `frame_%05d.jpg` 不符合 → bench 的 rust 路径现在抽帧后重命名为 ms 形式
   （cpp/py 驱动不受影响）。
3. 上次运行异常中断会残留帧目录，与新抽帧混在一起导致 CER 被污染 → 抽帧前
   先清空目录。
4. bench 新增 `--ep`（选后端 → 自动选对应 exe）与 `--rust-bin` / `--directml-bin`
   （CPU / GPU 两个 feature 形态的 exe 分开跑），GPU 后端进 label 避免结果互相覆盖。
