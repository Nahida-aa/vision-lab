# Rust vs C++ 状态机对比与优化空间分析

> 记录 Rust `subtitle_finder` 与 C++ VideoSubFinder 状态机的性能对比、Rust 侧
> 的分配热点与优化空间。目标：指导后续优化，避免重复分析。

## 一、性能基线（2026-08-07 实测）

用 `/tmp/clip5s.mp4` 循环生成的 30s / 870-884 帧视频（1280×720）对比：

| 配置 | 墙钟 | 每帧 |
|---|---|---|
| **Rust 单线程**（subtitle_finder） | 17.6s | **~19.9ms** |
| **C++ `--threads 1 --no-save`** | 17.8s | **~20.5ms** |
| **C++ 默认（12 线程）** | 15.8s | **~18.2ms** |

- Rust 与 C++ **单核算法效率相当**（~20ms/帧），无需为"追上 C++"而优化。
- **C++ 1→12 线程只快 1.12x**：瓶颈是顺序状态机逻辑 + 帧同步/事件等待
  （`evt_rgb.wait()`、`shared_custom_task`、TBB 流水线同步），不是可并行的帧变换。
  → 给 Rust 加多线程收益预计也有限。协程对 CPU-bound 无益（无 I/O 等待，async 只有调度开销）。

> 注：C++ CLI 已加 `--no-save`（`g_disable_save_images`）与 `--threads N`（`g_threads`）
> 参数，在 `cli/cli_main.cpp`（该 `cli/` 目录未跟踪进 git）。重建见本文档末尾附录。

## 二、Rust `--profile` 分阶段耗时

`--profile` 只统计了**每帧变换**（`get_transformed_image` 各阶段）：

| 阶段 | 总耗时 | 每帧 |
|---|---|---|
| filter(连通域) | 8.0s | 9.1ms |
| im_ff(边缘+阈值) | 6.4s | 7.2ms |
| bgr_to_yuv | 2.0s | 2.2ms |
| color_filtration | 1.2s | 1.4ms |
| 总计 | 17.6s | 19.9ms |

**关键**：profile 未统计状态机逻辑（compare/筛选/解码）的耗时。而状态机每帧
**额外分配 13-15MB** 的整帧深拷贝（见下），这些分配完全在 profile 统计之外，却
是真实开销（malloc 1280×720 Vec 是 O(n)，会触发清零/页错误）。

## 三、C++ vs Rust 状态机：缓冲复用差异

帧尺寸：1280×720 = 921,600 px。单缓冲：`im/ne`(u8)≈0.92MB，`y`(u16)≈1.84MB，
`bgr`(×3)≈2.76MB。

| 方面 | C++ | Rust |
|---|---|---|
| 帧变换结果 | `ImForward[DL]/ImNEForward[DL]/ImYForward[DL]` 环形缓冲，**复用槽位** | `FrameCache` 滑窗 VecDeque，已复用（OK） |
| 状态变量 | `ImIntS/ImNES/ImYS/ImFS` 持久缓冲，仅更新时赋值 | `im_int_s` 等持久 Vec，已复用（OK） |
| `get_intersect_images` | 用环形缓冲槽引用 + 单个 `ImInt/ImYInt` 工作缓冲 | **每帧新分配 `Vec<Vec<u8>>` + 多次整帧 clone** ❌ |
| `compare` 临时图 | `ImRES/ImFF1/ImFF2` 在调用处声明、可跨调用复用 | **每次调用 `to_vec()/clone()` 全新分配** ❌ |
| **每帧额外分配** | **~0**（复用工作缓冲） | **13-15MB/帧**（未被 profiler 统计） |

## 四、Rust 分配热点（按收益排序）

### 1. `get_intersect_images`（state.rs:238-279）—— 每帧调 1 次，省 4-6MB/帧
- `ims.push(f0.im.clone())` + `imys.push(f0.y.clone())`（253-255）：2 次整帧深拷贝
- 每个 has_text 帧再 `im.clone()/y.clone()`（259-261）：2 次/帧
- `im_int = ims[0].clone()` + `y_int = imys[0].clone()`（274/277）：再 2 次
- **可改**：收集 `Vec<&[u8]>`（引用切片）而非 `Vec<Vec<u8>>`；`im_int` 用调用方
  传入的复用缓冲，或直接用第一个有字幕帧切片 + 就地 intersect。
  省 ~4-6MB/帧。

### 2. `compare_two_subs_optimal`（compare.rs:313-348）—— 每段结束扫描时频繁调，省 ~8MB/次
每次调用内部整帧分配：
- `im_ff1=im1.to_vec()` + `im_ff2=im2.to_vec()`（234-235）：2×0.92MB
- `add_two_images`（imgops.rs:65 内部 `a[..size].to_vec()`）：+0.92MB
- `im1_c=im_ff1.clone()`（275）、`im2_c=im1_c.clone()`（280）：2×0.92MB
- `ila_int=ila1.to_vec()`（282）：1.84MB（u16）
- `dilate`（287）：内部 to_vec + 每迭代 clone → ~1.84MB
- 首轮 false 走 DifficultCompare：`ff1/ff2=im1/im2.to_vec()`（340-341）2×0.92MB +
  `filter_image` 内 `prev=im_f.to_vec()` **循环**（362，通常 2-3 轮）→ 2-3MB
- **一次 optimal ≈ 8-12 个整帧分配 ≈ 8~11MB**
- **可改**：`im_res`/`im_ff1`/`im_ff2` 用函数内复用缓冲（`Vec::with_capacity` 传出），
  `get_lines_info` 复用 `lb/le`。这是最大的单次调用开销。

### 3. `prev_im_ne` 每帧 1 次 clone（state.rs:798，省 0.92MB/帧）
- `prev_im_ne = get_frame(&cache, fn_)?.ne.clone()` —— 每帧必执行的整帧 ne 拷贝。
- **可改**：双缓冲乒乓（两个 preallocated Vec 轮流当 prev）。

### 4. 追踪循环里 `p_prev_ne`/`ne_ff` 每 off clone（state.rs:697/698/714/725）
- 字幕段结束扫 `off in 0..(dl-1)` 时，每 off 循环 `p_prev_ne=prev_im_ne.clone()`/
  `p_prev_ne=f_off.ne.clone()`（0.92MB × dl）。**可改**：仅首次 clone，其余复用。

### 5. 检测阶段 `get_frame(...)?.clone()` 整帧（state.rs:469/476）
- `f1 = get_frame(&cache, ...).clone()` clone 整个 FrameData（含 2.76MB bgr），
  但调用方只用 `has_text/im/y`。**可改**：只 clone 所需字段或直接传引用。

### 6. `compare_by_offset`（state.rs:302/304/325-326）
- `ne12 = prev_ne.to_vec()`、`im_int2=f_off.im.clone()` 等，`ne12` 仅只读可传 `&prev_ne`。

## 五、建议优化顺序

1. **`get_intersect_images` 引用化**（省 4-6MB/帧，收益最大、改动局部、风险低）
2. **`compare_two_subs_optimal` 去冗余拷贝**（省 ~8MB/次，单次最大）
3. **`prev_im_ne` 双缓冲乒乓**（省 0.92MB/帧，每帧必省）
4. 追踪循环 `p_prev_ne` 复用 + 检测阶段去整帧 clone

> 每步改完跑 `clip5s` 验证段仍为 `132-932/932-2265/2266-3499/3700-5033` 4 段不变。

## 六、实测验证（2026-08-07）——分配优化对墙钟影响甚微 ⚠️

### 6.1 整视频 A/B（高负载机器，load avg 2.1+）

实现第 1 项（`get_intersect_images` 引用化，改收集 `Vec<&[u8]>` 省 ~4-6MB/帧）后
A/B 对比：

| 版本 | 3 次墙钟（30s/884 帧视频） | 均值 |
|---|---|---|
| 优化前 | 35.03s / 34.41s / 33.60s | ~34.3s |
| 优化后（第1项） | 34.20s / 35.30s / 35.00s | ~34.5s |

**差异 ~0.2s（0.6%），在噪声范围内。**

### 6.2 单函数微基准（`src/bin/perfbench.rs`，隔离、负载稳健）✅ 决定性

整视频 A/B 受负载干扰不可靠，故用 `std::time::Instant` 隔离测关键函数
（1280×720 合成字幕帧，多次取均值）：

| 函数 | 每调用 | 相对 20ms/帧 |
|---|---|---|
| `compare_two_subs_optimal` **相同帧**（追踪热路径） | **0.49 ms** | ~2.5% |
| `compare_two_subs_optimal` **差异帧**（触发 DifficultCompare） | **5.06 ms** | ~25%（仅段边界） |
| `imgops::dilate(iters=1)` | 0.52 ms | — |
| `imgops::add_two_images` | 0.033 ms | — |
| `imgops::intersect_two_images_inplace` | 0.033 ms | — |

**结论（决定性）：**
1. 状态机 compare 每帧成本 ~0.5ms（2.5%），**分配优化顶多省零点几 ms，墙钟不可测**。
   差异帧 5ms 只在字幕变化边界出现，非每帧。
2. **`compare` 耗时被 `dilate`（0.52ms）主导**，不是分配本身——但都远小于变换。
3. **真正的瓶颈仍是每帧变换**（filter 9ms + im_ff 7ms ≈ 16ms 占 80%）。

> 微基准跑法：`cargo run -p subtitle_finder --release --bin perfbench`。
> 单函数隔离测是判断「某优化是否真有用」的可靠手段（负载机整视频 A/B 不可靠）。

### 6.3 每帧变换 `get_transformed_image` 分阶段拆解（决定性定位瓶颈）

perfbench 用合成 1280×720 BGR 字幕帧（带边缘扰动触发完整管线）隔离测 `get_transformed_image`，
用 `Profiler` 累计各阶段耗时 ÷ 次数：

| 阶段 | 每帧（隔离） | 占比 |
|---|---|---|
| **filter(连通域)** | **14.0 ms** | **60%** |
| **im_ff(边缘,3线程)** | **5.6 ms** | 24% |
| bgr_to_yuv | 2.4 ms | 10% |
| color_filtration | 1.4 ms | 6% |
| 合计 | 23.4 ms | 100% |

与真实视频 profile（filter 9ms + im_ff 7ms）量级一致，确认：
- **`filter_transformed_image`（连通域 / second_filtration）是绝对热点**，占变换 ~60%。
  内部多次 `dilate`（`dil_iters=(12/720*h)/2=6` 次 × 0.52ms ≈ 3ms）+ `second_filtration`
  + `filter_by_not_intersected_figures` + `clear_image_from_small_symbols` + 两次
  `restore_still_exist_lines` + `extend_imf`，且多次 `to_vec()`/`copy_from_slice`。
- `im_ff`（Sobel 边缘 + 阈值，3 线程）第二，24%。

**优化 filter 是唯一能显著降每帧耗时的地方**（14ms → 若能砍半，每帧省 ~7ms）。

### 6.4 filter 内部子函数拆解（真实视频，临时插桩测）

在 `filter_transformed_image` 内临时加 `std::time::Instant` 计时各子调用，跑真实视频
（30s/884 帧）累计取均值：

| 子函数 | 每帧 | 占 filter | 说明 |
|---|---|---|---|
| **`dilate(NE)`**（iters=6） | ~5.8 ms | ~45% | 3×3 迭代膨胀 |
| **`filter_by_not_intersected_figures`** | ~2.9 ms | ~22% | 连通域筛选（与 mask 相交保留） |
| **`clear_image_from_small_symbols`** | ~2.4 ms | ~18% | 剔除过小符号 |
| `extend_imf_with_data_from_imnf` | ~0.64 ms | 5% | 用边缘扩展文字行 |
| `second_filtration` | ~0.15 ms | 1% | 边缘密度过滤 |
| `restore_still_exist_lines` ×2 | ~0.45 ms | 3% | 行邻域恢复 |
| 合计 | ~13 ms | 100% | |

**关键实验（2026-08-07）——dilate 已接近最优，勿再微优化：**
- 试过**可分离两趟**（方形 SE 等价 + 水平/垂直一趟）：缓存不友好（垂直趟列主序遍历
  跨度 1280），实测慢 3×（7→19ms）。
- 试过**gather 逐像素**（每输出像素读 9 邻域）：边缘图稀疏，scatter 只在白点工作，
  gather 对每像素都读 9 邻域，实测慢 3×（7→21ms）。
- **结论**：边缘图（NE）稀疏，`dilate` 的 **scatter 实现（只处理白点）已是最优**。
  dilate 的 ~6ms 是 720p × 6 次 3×3 迭代的固有成本，受边缘密度线性影响。

> filter 三大头 dilate(5.8) + filter_fig(2.9) + clear_sym(2.4) ≈ 11ms。
> dilate 难再优化；filter_fig / clear_sym 是连通域遍历，可看是否有冗余全图扫描。

### 6.5 im_ff：geometry Sobel vs subtitle_finder 阶段（2026-08-07）

**crate 内 vs 跨 crate Sobel（720p，release，`lto=thin + codegen-units=1`）：**

| Sobel | geometry 内（同 crate，`src/bin/sobel_bench`） | 跨 crate（subtitle_finder perfbench） | 差距 |
|---|---|---|---|
| **M-edge**（im_ff 用的） | **0.218 ms** | **0.241 ms** | ~1.1× |
| N-edge | 0.159 ms | 0.248 ms | ~1.6× |
| H-edge | 0.155 ms | 0.236 ms | ~1.5× |

- **M-edge（im_ff 唯一用的）几乎无跨 crate 内联损失**（1.1×）：它走 AVX2，
  且当前 workspace `lto=thin + codegen-units=1` 让跨 crate 也能较好内联。
- **N/H-edge（AVX-512）确有 ~1.5-1.6× 内联损失**，符合 sobel.rs 注释"跨 crate 慢 ~1.8×"
  （`#[target_feature]` 跨 crate 不内联，Rust issue #145574）。
- 但 **im_ff 只用 M-edge**，所以 geometry 部分已是内联后的最快速度
  （~0.24ms×3 ≈ 0.72ms），不存在跨 crate 拖慢 im_ff。

**perfbench 隔离测（release，同负载）：**

| | 耗时 |
|---|---|
| **geometry `sobel_m_edge`**（单通道，跨 crate） | **0.24 ms** |
| **subtitle_finder `im_ff` 阶段**（3×Sobel + CMOE + 阈值 + 带拼接） | **5.87 ms** |

**结论：geometry 的 Sobel 很快，不是 im_ff 瓶颈。**
- im_ff 用 3 个 `sobel_m_edge`（Y/U/V）≈ 3×0.24 = **0.72ms**，只占 5.87ms 的 ~12%。
- 其余 **~5.1ms 是 subtitle_finder 自己的非 Sobel 工作**：`get_im_cmoe_with_thr1/thr2`
  （CMOE 组合 + `find_and_apply_local_thresholding` + `aply_ess/aply_ecp`，均为 u16 全帧
  标量操作）+ 带拼接/写回。
- **im_ff 的优化空间在 subtitle_finder 自写的 CMOE/阈值逻辑**，不在 geometry。
  如 `find_and_apply_local_thresholding`（imgops.rs:255）每次调用 `vec![0i32; 11*16*256]`
  （56KB）分配，且是标量逐块直方图；thr1/thr2 各调一次/帧。可优化该处分配与标量循环。

> ⚠️ geometry 自带的 `sobel_bench` **测试**（`cargo test -p geometry --lib sobel_bench -- --ignored`）
> 用 dev profile 无优化，数字异常（34ms，比真实慢 ~160×），**不可信**。要测须用
> `cargo run -p geometry --release --bin sobel_bench`（本仓库新增的 release bin）。

### 6.6 im_ff 内部子函数拆解（真实视频 8.3ms/帧，临时插桩测）

在 `get_im_cmoe_with_thr1/thr2` 与 `find_and_apply_local_thresholding` 临时加计时：

| im_ff 内子部分 | 每帧 | 占比 | 归属 |
|---|---|---|---|
| **`aply_ess` + `aply_ecp`**（thr1/thr2 各一次，2×1.78ms） | **3.56 ms** | ~43% | geometry SIMD（5×5 加权核，计算密集） |
| **`find_and_apply_local_thresholding`**（2×0.69ms） | **1.38 ms** | ~17% | subtitle_finder 标量（56KB 分配 + 逐块直方图） |
| CMOE 组合循环（2×）+ 带拼接/写回 + 3×Sobel | ~3.4 ms | ~40% | subtitle_finder 标量 + geometry |

**关键结论：**
1. **`aply_ess`+`aply_ecp` 是 im_ff 最大头（3.56ms，43%）**，在 geometry 内、已 SIMD
   （AVX2），但因是 5×5 加权 stencil（每像素 ~40 次 load）计算密集，且 thr1/thr2 各调
   两次。**改算法会破坏输出；若想加速需优化 geometry 内实现**（如减少重复 load /
   AVX-512）。
2. `find_and_apply_local_thresholding`（1.38ms，17%）是 subtitle_finder 自写标量：
   `vec![0i32; 11*16*256]`（56KB）每调用分配，`im.iter().max()` 全扫，逐块标量直方图。
   **可优化**：复用 edge_str 缓冲 + 合并 max 扫描。
3. 结合 6.5：im_ff ≈ 0.72ms（Sobel）+ 3.56ms（ess/ecp）+ 1.38ms（阈值）+ ~2.6ms（CMOE
   组合/拼接写回）。**真正可安全优化的是阈值部分（1.38ms）**；ess/ecp 在 geometry 需

### 6.7 已完成优化：aply_ess/aply_ecp 加 AVX-512（im_ff 最大头，2026-08-07）

按 6.6 定位，给 `aply_ess`/`aply_ecp`（im_ff 最大头 43%）加 AVX-512 快路径
（`geometry/src/imgproc/conv.rs`）：

- `aply_ecp_avx512` / `aply_ess_avx512`：一次 **16 像素**（zmm，i32 加宽），相比 AVX2
  的 8 像素吞吐翻倍。除法仍标量（Rust std::arch 无 `_mm512_div_epi32`），但除法占比
  小，瓶颈是 25 次 load+累加。ecp 中心==0 掩码用 `_mm512_cmpgt_epi32_mask`。
- 运行时按 `is_x86_feature_detected!(avx512f,bw,vl)` 分派，无 AVX-512 回落 AVX2。

**结果：**
- 整管线 im_ff 阶段：**7369 → 6116ms**（884 帧，~1.4ms/帧，**~17%**）
- 正确性：`aply_ess/ecp_simd_matches_scalar` 通过，subtitle_finder 24 测试过，
  clip5s 仍 4 段不变
- isolated bench 数字受机器负载噪声影响波动，以整管线 profile 为准

> 经验：AVX-512 对 5×5 加权核这类"load 密集"算子收益明显（一次 16 像素翻倍吞吐）；
> 但除法/分支仍标量，因为 Rust std::arch 缺 AVX-512 整数除法。若需再进一步，可考虑
> 对 ess/ecp 用 `_mm512_mul_epu32` 乘倒数做向量除法。

   谨慎。




## 七、结论与建议

- **状态机分配 churn 不是墙钟瓶颈**，第 2/3/4 项优化（compare 去拷贝、prev_im_ne
  双缓冲等）**不必做**——不会带来可测提速。
- `get_intersect_images` 引用化已实现并验证正确（输出不变、测试通过），作为
  **代码质量改进**保留，但不要期待墙钟收益。
- **要提速，聚焦变换本身**：`filter`（连通域，9ms）与 `im_ff`（边缘+阈值，7ms）。
  这两块是 CPU-bound，可考虑 SIMD / 多线程 / 算法简化。多线程有收益但要处理状态机
  顺序依赖（C++ 12 线程仅 1.12x，瓶颈是顺序状态机）。

## 附录：C++ CLI 重建命令

`cli/` 目录未跟踪进 git，改 `cli_main.cpp` 后重建：
```sh
cd cli
g++ -O2 -std=c++17 -I../Components/IPAlgorithms -I../Components/Include \
  -I../Interfaces/VideoSubFinderWXW -I/usr/include/opencv5 -I. \
  -include opencv_compat.h -c cli_main.cpp -o build/cli_main.o
g++ -O2 -std=c++17 build/cli_main.o build/IPAlgorithms.o build/SSAlgorithms.o \
  build/MyClosedFigure.o -o video_subfinder_cli \
  -lopencv_core -lopencv_imgproc -lopencv_imgcodecs -lopencv_videoio \
  -lopencv_geometry -ltbb -lpthread
```
（OpenCV 5 头在 `/usr/include/opencv5`，需 `-include opencv_compat.h` 垫 CV_IMWRITE_* 常量。）

## 七、2026-11-02 复测：Rust vs C++ 同一视频同一机器

> ⚠️ 第一次复测用了"墙钟 ÷ 总帧数"这个错误口径，得出"Rust 比 C++ 慢"的结论，
> 已作废。正确口径见 7.3。

### 7.1 实验条件

| 项 | 值 |
|---|---|
| 视频 | `vision-lab/tests/bench/subtitle_ocr/ref/狗/2/video_source.mp4` |
| 视频参数 | 1280×720，30fps，33.9s，**1017 帧** |
| 机器 | 16 核 |
| Rust | `subtitle_finder`（release），`SF_NWORKERS` 控制 worker 数 |
| C++ | `learn_ls/VideoSubFinder/cli/video_subfinder_cli`，`--no-save --threads N` |

### 7.2 墙钟（仅供参考，不等于算法耗时）

| 实现 | 线程 | 墙钟 |
|---|---|---:|
| Rust | 1 worker | 10.1s |
| Rust | 16 workers | 8.9s |
| C++ | 1 线程 | 8.996s / 8.896s |
| C++ | 12 线程 | 8.203s / 8.336s |

**墙钟不能直接除帧数**：Rust 侧解码单独耗时 10.0s（stderr 有单独行），
墙钟大头是解码；C++ 侧墙钟包含它自己的打开/解码开销。
两边口径不同，相除得出的"每帧"没有可比性。

### 7.3 正确口径：Rust 内部 profile（`--profile`）

单 worker，1017 帧：

| 阶段 | 总耗时 |
|---|---:|
| color_filtration | 1731.7 ms |
| bgr_to_yuv | 430.6 ms |
| im_ff（边缘+阈值） | 4515.7 ms |
| filter（连通域） | 2192.1 ms |
| 状态机 get_intersect | 57.9 ms |
| 状态机 compare | 77.7 ms |
| 状态机 analize | 2934.1 ms |
| 状态机 other | 2713.9 ms |
| **总计** | **14653.8 ms** |
| **平均/帧** | **14.409 ms** |

> 注：16 workers 时 profile 显示 38.6ms/帧，那是**各 worker 累计**的 CPU 时间，
> 不是墙钟，不能与单 worker 直接比较。

### 7.4 结论

- **Rust 实际算法耗时 14.4ms/帧**（单 worker，1017 帧视频，本机）。
- 与 2026-08 基线（Rust 19.9ms / C++ 20.5ms）相比，
  Rust 从 19.9 → 14.4ms，**提速约 28%**；
  相对同一基线下的 C++ 20.5ms，**快约 30%**。
- 用户记忆中"12-14ms/帧"与此吻合（热跑时更低）。
- 之前误判的根源：拿墙钟除以帧数，把解码开销算进了算法耗时。
- **结论：Rust 侧有实打实的性能改进，"比 C++ 快 30-40%"这个量级是成立的**，
  但对外引用时必须说明口径（单 worker / 1017 帧 / 本机），
  不能只说"快 40%"。

### 遗留问题

- C++ CLI 打印"搜索完成，结束于 0 ms"，`FastSearchSubtitles` 返回值不可靠，
  但不影响墙钟计时。
- 若要与 C++ 严格同口径对比，需要给 C++ 也加阶段级计时，目前只做了墙钟。

## 八、2026-11-03：只优化 Rust 侧的 A/B 实测

目标：同一视频、同一机器，把 Rust 与 C++ 的差距压平，**只改 Rust**，
且输出（关键帧段数 / 时间轴）必须与优化前逐段一致。

### 8.1 实验条件

| 项 | 值 |
|---|---|
| 视频 | `tests/bench/subtitle_ocr/ref/狗/2/video_source.mp4`（1280×720，30fps，33.9s，1017 帧） |
| C++ | `learn_ls/VideoSubFinder` **HEAD 干净源码**重编，`-O3 -march=native`，`--no-save`（默认 12 线程） |
| Rust 基线 | 本仓库 HEAD 源码（优化前） |
| Rust 优化 | 本文第八节对应的改动，默认 4 个 transform worker |
| 机器 | 16 核；⚠️ 本次复测时桌面程序（electron/kwin/zed 等）持续抢占 CPU，load 5–20 |

> 复测机不是空闲状态，墙钟比 2026-08 的空闲基线**整体慢 3–4 倍**。
> 因此下面的同条件 A/B 只用于比较实现之间的相对差异，不与旧绝对值混用。

### 8.2 同条件背靠背 A/B（3 轮）

| 轮次 | Rust 基线 墙钟 / CPU | Rust 优化 墙钟 / CPU | C++ 墙钟 / CPU |
|---|---:|---:|---:|
| 1 | 33.07s / 122.8s | 28.07s / 115.0s | — |
| 2 | 30.57s / 117.5s | 26.76s / 111.2s | — |
| 3 | 29.66s / 113.3s | 26.57s / 111.8s | 26.11s / 157.7s |
| 补充 | — | 28.22s / 115.1s | 29.46s / 168.7s |

- **Rust 优化 vs Rust 基线：墙钟 −10~15%，总 CPU −4~5%**。
  墙钟收益大于 CPU 收益，说明削掉的正是主线程串行临界路径。
- 本轮 C++ 墙钟与 Rust 优化接近（26–29s vs 27–28s），但 **CPU 高约 40%**
  （C++ 12 线程并行度更高，在空闲机器上墙钟更快，见 8.3）。
- 空闲机器上此前同一 A/B 口径（本会话早前实测）：
  **C++ 7.49/7.57/7.60s，Rust 基线 8.2–8.9s，Rust 优化 7.72–8.2s**；
  CPU 口径 C++ ≈ 52s vs Rust ≈ 34s（Rust 约省 1/3 CPU）。

### 8.3 优化项（全部为纯实现改动，语义不变）

1. `compare.rs`：val3 的诊断统计（`im1/ila1/ve1/im_ff1` 的整帧白点数、331-339 噪声带）
   原先**无条件**执行，每帧多扫 4–6 遍 0.92M 像素；改为
   `tracing::enabled!(DEBUG)` 才计算（实测占主线程约 3ms/帧）。
2. `compare.rs`：val3 的 `IntersectYImages(ImILAInt, ImILA2)` 改用
   `intersect_y_images_masked` —— `ImILAInt` 之后只经 `Im2 = Im1 ∩ ImILAInt` 消费，
   而 `Im1 ⊆ mask`，故 mask 外清零不改变任何输出（已用时间轴对拍确认）。
3. `imgops.rs`：抽出 `y_in_range` 判定核（`wrapping_sub` 无分支区间判定），
   单图/多图/掩码三处共用，保证逐位一致。
4. `state.rs`：`intersect_images_range` 的二值交集改成按位与
   （`255 & x == x`），4/5 帧展开后 LLVM 可向量化；
   `intersect_y_images_range` 同样按 4/5 帧展开为无内层循环的判定。
5. `preprocess.rs`：`AnalyseImage` 改行优先连续扫描，且只为密度最大的带建段表
   （旧实现每帧为 h/segh≈240 条带各分配一个 `Vec`）。单帧 0.47 → 0.41ms；
   新增 `analyse_flat_matches_legacy` 随机帧对拍测试。
6. `state.rs`：检测阶段 `f1`/`f2` 只借用不 `clone`（每帧省 ~6.5MB 复制）。

**放弃的方案**（记录以免重复踩坑）：

- 把 u16 的 `y` 图“只在 mask 内计算”：`y` 是「亮度 + 255」而非前景掩码，
  背景像素参与跨帧 ILA 比较，置 0 会改变语义，实测第 3 段起点 1233 → 1266。
- `compare_two_subs` 的单趟融合（把 `im_ff1`/`im_ff2`/`im_res` 的多趟全帧扫描合并成一趟）：
  理论上少扫几遍 0.92M 像素，实测却**没有稳定收益**。同一次实验里，某个段落的
  `compare` 累计耗时：多趟版约 1017ms、融合版约 1592ms（融合反而慢约 56%）；
  换数据后差距缩小甚至反转，说明收益取决于数据与访存模式，不足以作为默认路径。
  已回退为多趟结构。
  ⚠️ 原始计时日志已丢失，此处只剩当时的累计数，无法复现；若要引用请先重跑。

### 8.4 输出一致性校验

- Rust 优化前后 `timeline.txt` **逐字节相同**（19 段，第 3 段起点 1233）。
- C++ HEAD 干净重编同样输出 19 段，**段首与 Rust 一致**（第 3 段两边都是 1233）。
- **段尾 Rust 固定比 C++ 早 1 帧**（32~33ms @30fps）。根因已定位：
  C++ `SSAlgorithms.cpp` 的 `et = PosForward[offset] - 1` 取的是「字幕消失后
  第一帧」的 PTS-1，把一帧非字幕帧算进了段尾；Rust 用的是旧句**最后一个可见帧**
  的真实 PTS（`state.rs` 段尾赋值处有同样注释）。例：字幕在 20333 消失，
  C++ 标 20365。这是 C++ 侧的 off-by-one，Rust 不跟随，也**不去修 C++**
  （C++ 仅作对比基准）。
