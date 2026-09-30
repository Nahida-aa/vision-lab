# subtitle_finder 性能参照（C++/OpenCV vs Rust SIMD）

本目录给 `subtitle_finder` 的逐像素算子一个 C++/OpenCV 性能参照，回答
「我们的 Rust 复刻是比原始 C++（VideoSubFinder 用 OpenCV）快还是慢」。

## 编译 & 跑

```sh
g++ -O3 -march=native -std=c++17 -I/usr/include/opencv5 perf_compare.cpp \
    -L/usr/lib -lopencv_core -lopencv_imgproc -o perf_compare
./perf_compare
```

> 注意：必须用 `volatile g_sink` 累加结果，否则 g++ `-O3` 会死代码消除无副作用的循环。

Rust 侧对照（geometry 的 sobel/conv 微基准）：
```sh
# 默认 release（无 -march=native）
cargo test -p geometry --release --lib sobel_bench -- --ignored --nocapture
# 开 native（标量循环被 LLVM 自动向量化）
RUSTFLAGS="-C target-cpu=native" cargo test -p geometry --release --lib sobel_bench -- --ignored --nocapture
cargo test -p geometry --release --lib aply_ess_bench -- --ignored --nocapture
```

## 一键稳定对比：`./bench.sh [runs]`

**重要：单次微基准波动可达 ~50%（系统负载/频率），单次测量会误导**（例如同一算子
一次显示 Rust 快、一次显示 Rust 慢）。`bench.sh` 对两边各采样 N 次（默认 5）**取最小值**
（最接近真实性能，抗负载拖慢），输出可信对比表：

```sh
./bench.sh 5
```

输出示例（min-of-3，720p，release）：
```
算子           C++     Rust(较快)   Rust/C++
sobel_m       0.246    0.210        0.85x   # Rust 快
sobel_n       0.101    0.190        1.88x   # Rust 慢（真实差距）
sobel_h       0.103    0.200        1.94x   # Rust 慢（真实差距）
aply_ess      0.684    0.990        1.45x   # Rust 慢
aply_ecp      4.441    3.390        0.76x   # Rust 快
```

> 用 `min-of-N` 后：sobel_m / aply_ecp Rust 其实**快于** C++（单次测量的"慢"是方差假象）；
> sobel_n/h 稳定慢 ~1.9×（真实差距，g++ 对简单 stencil 自动向量化更强）。


## 结果（720p，release，C++ -O3 -march=native）

### 无 `-C target-cpu=native`（旧，手写 AVX2 为主）

| 算子 | C++ 自定义核 | Rust 手写 AVX2 | Rust/C++ |
| --- | --- | --- | --- |
| sobel_m | 0.286 ms | 1.25 ms | **~4.4× 慢** |
| sobel_n | 0.141 ms | 0.67 ms | **~4.8× 慢** |
| sobel_h | 0.156 ms | 0.38 ms | ~2.4× 慢 |
| aply_ess | 0.822 ms | 1.70 ms | ~2.1× 慢 |
| aply_ecp | 6.595 ms | 3.13 ms | **~2.1× 快** |

### 开 `-C target-cpu=native`（配 `.cargo/config.toml`，推荐）

> 关键：让 LLVM 对**标量循环**自动向量化（等价 g++ -O3 -march=native 对朴素循环的做法）。
> 这是最大的一步改进（Sobel 从 ~4× 慢缩到 ~1.5× 慢）。

| 算子 | C++ 自定义核 | Rust 标量(native) | Rust AVX2(native) | Rust/C++（取较快） |
| --- | --- | --- | --- | --- |
| sobel_m | 0.286 ms | 0.56 ms | 0.24 ms | **~1.2× 慢** |
| sobel_n | 0.141 ms | 0.20 ms | 0.23 ms | ~1.4× 慢 |
| sobel_h | 0.156 ms | 0.21 ms | 0.19 ms | ~1.2× 慢 |
| aply_ess | 0.822 ms | 0.89 ms | 1.03 ms | ~1.1× 慢 |
| aply_ecp | 6.595 ms | 7.46 ms | 6.35 ms | ~1.0× 持平 |
| bgr2yuv | cv::cvtColor 0.202 | — | ~0.66ms/帧 | 慢于 OpenCV |

### 结论
1. **`-C target-cpu=native` 是最重要的优化**：让 LLVM 自动向量化标量循环，效果接近 g++。
   subtitle_finder 全流程从 ~55ms/帧 → ~36ms/帧（~34% 提升），连 bgr2yuv（274→101ms）、
   filter（1415→1176ms）都大幅改善（不只是手写 SIMD 的算子）。
2. sobel_n/h 用 **3-load AVX-512**（`permutexvar_epi16` 派生偏移 tap，减少内存带宽）
   后，同 crate 内联达 ~0.11ms（追平 C++ 0.10ms）；但**跨 crate 调用仍 ~1.8× 慢**
   （见下方「Rust 平台限制」）。
3. aply_ecp Rust 反而快（~0.4-0.6×）；sobel_m ~0.8× 持平。

## Rust 平台限制（`#[target_feature]` 跨 crate 不内联）

**为什么同 crate 0.11ms、跨 crate 0.18ms？**

- `#[inline(always)]` 不能与 `#[target_feature]` 同用（Rust issue #145574，nightly 同禁）。
- 运行时 `is_x86_feature_detected` 分派让 LLVM 无法静态确定调用方 feature，故跨 crate
  （即使开 thin LTO + codegen-units=1）也不内联 `#[target_feature]` 函数。
- 唯一让调用方静态确定 feature 的办法是把分派函数也标 `#[target_feature]`，但那会让
  无 avx512 机器的 fallback 失效。

**结论**：g++ 的 AVX-512 在调用点自动向量化（无此限制）；Rust 手写 intrinsics 跨 crate
有内联惩罚。sobel_n/h 的 ~1.8× 是平台约束，非算法问题。若想彻底解决：把 Sobel 内联进
调用方，或等 Rust 支持 `#[target_feature]` 跨 crate 内联。

## 优化方向（基于此基准）

- **必须配 `.cargo/config.toml` 开 `-C target-cpu=native`**（已做）。这是性价比最高的一步。
  注意：产物不跨机器可移植（换机器需重编）；要可移植改 `-C target-feature=+avx2`。
- sobel_n/h 已用 3-load AVX-512（同 crate 达 C++ 水平）；跨 crate 受限，需内联进调用方或等 Rust。
- sobel_m 用 16×i16 宽度 + shift+add（非 mullo_epi32），~2× 优于标量。
- 若要对齐 OpenCV 的 cvtColor（bgr2yuv 仍慢），需专门 SIMD 处理 BGR 解交错。

