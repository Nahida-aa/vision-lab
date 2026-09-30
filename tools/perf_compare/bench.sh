#!/usr/bin/env bash
# subtitle_finder 逐像素算子：C++/OpenCV vs Rust SIMD 性能对比。
#
# 关键：单次微基准受系统负载影响波动大（可差 ~50%）。本脚本对两边各采样 N 次，
# **取最小值**（最接近真实性能，抗负载拖慢），消除方差误导。
#
# 用法：
#   ./bench.sh [runs]        # runs 默认 5
#
# 依赖：先编译 C++ 参照（见 perf_compare.cpp 顶部），Rust 侧用 geometry 的
# sobel_bench / aply_ess_bench（#[ignore] 微基准）。

set -euo pipefail
cd "$(dirname "$0")"
shopt -s nullglob   # 无匹配的 glob 展开为空，避免 cat 报错

RUNS="${1:-5}"

# ---- 解析 C++ 输出：取每个算子的最小值 ----
# C++ 行格式：  sobel_m (IMOE)               :    0.269 ms
cpp_min() {
    local name="$1"
    # 匹配以该算子名开头的行，取冒号后、ms 前的数值（倒数第二列）。
    grep -E "^[[:space:]]*${name}[[:space:]]" | awk '{print $(NF-1)}' | sort -n | head -1
}
# 输出纯数字 ms。

# ---- 解析 Rust 输出：<op> 行里 SIMD/scalar 的最小值 ----
rust_min() {
    local line_pat="$1"   # 匹配行的子串，如 "M-edge 720p"
    local field="$2"      # "SIMD" 或 "scalar"
    grep -E "${line_pat}" | grep -oE "${field} [0-9.]+ms" | grep -oE "[0-9.]+" | sort -n | head -1
}

echo "采样 ${RUNS} 次取最小值（ms，越小越快）"
echo "==============================================="

# ---- C++ ----
echo "[C++] 编译/运行 perf_compare ..."
g++ -O3 -march=native -std=c++17 -I/usr/include/opencv5 perf_compare.cpp \
    -L/usr/lib -lopencv_core -lopencv_imgproc -o perf_compare 2>/dev/null || {
    echo "C++ 编译失败（需要 OpenCV 5）"; exit 1;
}
rm -rf /tmp/perf-cmp
mkdir -p /tmp/perf-cmp
for i in $(seq 1 "$RUNS"); do ./perf_compare | tee /tmp/perf-cmp/cpp_$i.txt >/dev/null; done
CPP_M=$(cat /tmp/perf-cmp/cpp_*.txt | cpp_min "sobel_m")
CPP_N=$(cat /tmp/perf-cmp/cpp_*.txt | cpp_min "sobel_n")
CPP_H=$(cat /tmp/perf-cmp/cpp_*.txt | cpp_min "sobel_h")
CPP_ESS=$(cat /tmp/perf-cmp/cpp_*.txt | cpp_min "aply_ess")
CPP_ECP=$(cat /tmp/perf-cmp/cpp_*.txt | cpp_min "aply_ecp")

# ---- Rust ----
echo "[Rust] 跑 geometry 微基准（release，native）..."
mkdir -p /tmp/perf-cmp
for i in $(seq 1 "$RUNS"); do
    cargo test -p geometry --release --lib sobel_bench -- --ignored --nocapture 2>/dev/null | tee /tmp/perf-cmp/rs_sobel_$i.txt >/dev/null
    cargo test -p geometry --release --lib aply_ess_bench -- --ignored --nocapture 2>/dev/null | tee /tmp/perf-cmp/rs_conv_$i.txt >/dev/null
done

RS_M=$(cat /tmp/perf-cmp/rs_sobel_*.txt | rust_min "M-edge" "SIMD")
RS_N=$(cat /tmp/perf-cmp/rs_sobel_*.txt | rust_min "N -edge" "scalar")
RS_H=$(cat /tmp/perf-cmp/rs_sobel_*.txt | rust_min "H -edge" "scalar")
RS_ESS=$(cat /tmp/perf-cmp/rs_conv_*.txt | rust_min "AplyESS" "SIMD")
RS_ECP=$(cat /tmp/perf-cmp/rs_conv_*.txt | rust_min "AplyECP" "SIMD")

# ---- 对比表 ----
printf "\n%-10s %12s %14s %10s\n" "算子" "C++" "Rust(较快)" "Rust/C++"
printf "%-10s %12s %14s %10s\n" "----" "-----" "---------" "-------"
ratio() { awk -v a="$1" -v b="$2" 'BEGIN{ if (a+0>0) printf "%.2fx", (b+0)/(a+0); else print "n/a" }'; }
printf "%-10s %10.3fms %12.3fms %9s\n" "sobel_m" "$CPP_M" "$RS_M" "$(ratio $CPP_M $RS_M)"
printf "%-10s %10.3fms %12.3fms %9s\n" "sobel_n" "$CPP_N" "$RS_N" "$(ratio $CPP_N $RS_N)"
printf "%-10s %10.3fms %12.3fms %9s\n" "sobel_h" "$CPP_H" "$RS_H" "$(ratio $CPP_H $RS_H)"
printf "%-10s %10.3fms %12.3fms %9s\n" "aply_ess" "$CPP_ESS" "$RS_ESS" "$(ratio $CPP_ESS $RS_ESS)"
printf "%-10s %10.3fms %12.3fms %9s\n" "aply_ecp" "$CPP_ECP" "$RS_ECP" "$(ratio $CPP_ECP $RS_ECP)"

rm -rf /tmp/perf-cmp
