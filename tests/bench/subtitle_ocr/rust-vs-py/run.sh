#!/usr/bin/env bash
# 复现「Rust vs Python 单帧耗时/内存」实测。
#
# 设计要点（口径纪律）：
# - 只测**同一帧**（`oneframe-replay40/` 里 40 个硬链接指向同一 inode），
#   不跑全视频；40 个副本只是为了让「常驻 session 的批量进程」产生 40 个样本，
#   以便算 P50/P95。
# - 所有实现读同一个目录 / 同一张图，均只做 OCR（抽帧在驱动层之外，不参与）。
# - `taskset -c 0-3` 把可用 CPU 固定为 4 个，cpp 显式 `--threads 4`，python
#   构造 `RapidOCR(intra_op_num_threads=4)`；rust 走 ORT 默认（= 可见物理核 4）。
# - 计时：rust/cpp/python 都在**进程内逐帧**量（不含进程启动、不含抽帧）；
#   另测 python「每帧一个进程」的 one-shot 模式，用来解释旧基准 590ms/帧。
#
# 用法（仓库根）：
#   bash tests/bench/subtitle_ocr/rust-vs-py/run.sh [reps]
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../../../.." && pwd)
cd "$ROOT"
HERE=tests/bench/subtitle_ocr/rust-vs-py
RES="$HERE/results"
FRAME_DIR=packages/tmp/ocr-bench/oneframe-replay40
FRAME="$FRAME_DIR/0000000.jpg"
REPS=${1:-3}
SAMPLES=40
CPUSET=0-3

mkdir -p "$RES"

# 探针与本包一起构建（不改变被测实现）。
cargo build -p bench_subtitle_ocr --release --bin probe >/dev/null 2>&1 || {
  echo "probe 构建失败（cargo build -p bench_subtitle_ocr --release --bin probe）" >&2
  exit 1
}

if [[ ! -f "$FRAME" ]]; then
  echo "缺少单帧目录：先跑 create_frames.sh" >&2
  exit 1
fi

echo "loadavg(before) = $(cut -d' ' -f1-3 /proc/loadavg)" | tee "$RES/env.txt"
{
  echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs)"
  echo "cores: $(nproc) logical / $(lscpu | awk -F: '/Core\(s\) per socket/{print $2}' | xargs) per socket"
  echo "mem: $(free -m | awk '/Mem:/{print $2" MB"}')"
  echo "cpuset: $CPUSET (4 CPUs)"
  echo "frame: $FRAME (1 distinct image, hardlinked 40x)"
  echo "samples/run: $SAMPLES, runs: $REPS"
  echo "ort: $(python3 -c 'import onnxruntime;print(onnxruntime.__version__)' 2>/dev/null || echo n/a)"
  echo "rust: $(./target/release/subtitle_ocr --help 2>/dev/null | head -1)" 
  echo "rapidocr_onnxruntime: $(packages/subtitle_ocr_py/.venv/bin/python -c 'import rapidocr_onnxruntime as r;print(getattr(r,\"__version__\",\"?\"))' 2>/dev/null)"
} >> "$RES/env.txt"

for r in $(seq 1 "$REPS"); do
  echo "=== run $r/$REPS   loadavg=$(cut -d' ' -f1-3 /proc/loadavg) ==="

  # rust v4（与 python 的权重版本一致）+ rust v3（与 cpp 一致）
  for m in v4 v3; do
    python3 "$HERE/monitor.py" "$RES/rust-$m-r$r.mem.json" -- \
      taskset -c "$CPUSET" ./target/release/probe --dir "$FRAME_DIR" --model "$m" \
      --reps 1 --warmup 1 --threshold 0.45 --subtitle-only \
      --out "$RES/rust-$m-r$r.json" >/dev/null
  done

  # rust 关 arena（与 Python 包内默认 enable_cpu_mem_arena=False 对齐）
  for m in v4 v3; do
    python3 "$HERE/monitor.py" "$RES/rust-$m-arenaoff-r$r.mem.json" -- \
      taskset -c "$CPUSET" env RAPIDOCR_CPU_ARENA=0 ./target/release/probe --dir "$FRAME_DIR" --model "$m" \
      --reps 1 --warmup 1 --threshold 0.45 --subtitle-only \
      --out "$RES/rust-$m-arenaoff-r$r.json" >/dev/null
  done

  # cpp v3（权重写死 v3）
  python3 "$HERE/monitor.py" "$RES/cpp-v3-r$r.mem.json" -- \
    taskset -c "$CPUSET" env \
      LD_LIBRARY_PATH=packages/subtitle_ocr_cpp/build \
      OCR_MODELS_DIR=data/models/rapidocr \
      OCR_KEYS_PATH=data/models/rapidocr/ppocr_keys.json \
      ./packages/subtitle_ocr_cpp/build/subtitle_ocr_ort_cpp \
      --dir "$FRAME_DIR" 0.45 --subtitle-only --threads 4 \
      >/dev/null 2>"$RES/cpp-v3-r$r.stderr"

  # python v4：单进程常驻 session
  python3 "$HERE/monitor.py" "$RES/py-v4-r$r.mem.json" -- \
    taskset -c "$CPUSET" packages/subtitle_ocr_py/.venv/bin/python "$HERE/py_batch.py" \
      --frame "$FRAME" --reps "$SAMPLES" --threads 4 \
      --out "$RES/py-v4-r$r.json" >/dev/null

  # python v4：开 arena（归因实验，与 rust/cpp 的 ORT 默认对齐）
  python3 "$HERE/monitor.py" "$RES/py-v4-arenaon-r$r.mem.json" -- \
    taskset -c "$CPUSET" packages/subtitle_ocr_py/.venv/bin/python "$HERE/py_batch.py" \
      --frame "$FRAME" --reps "$SAMPLES" --threads 4 --arena-on \
      --out "$RES/py-v4-arenaon-r$r.json" >/dev/null

  # python v3（显式指向仓库 v3 权重）：与 rust-v3 / cpp-v3 同权重
  python3 "$HERE/monitor.py" "$RES/py-v3-r$r.mem.json" -- \
    taskset -c "$CPUSET" packages/subtitle_ocr_py/.venv/bin/python "$HERE/py_batch.py" \
      --frame "$FRAME" --reps "$SAMPLES" --threads 4 --models v3 \
      --out "$RES/py-v3-r$r.json" >/dev/null
  python3 "$HERE/monitor.py" "$RES/py-v3-arenaon-r$r.mem.json" -- \
    taskset -c "$CPUSET" packages/subtitle_ocr_py/.venv/bin/python "$HERE/py_batch.py" \
      --frame "$FRAME" --reps "$SAMPLES" --threads 4 --models v3 --arena-on \
      --out "$RES/py-v3-arenaon-r$r.json" >/dev/null

  # python v4：每帧一个进程（旧基准口径，5 次足够）
  taskset -c "$CPUSET" packages/subtitle_ocr_py/.venv/bin/python "$HERE/py_batch.py" \
    --frame "$FRAME" --reps 5 --oneshot --out "$RES/py-v4-oneshot-r$r.json" >/dev/null
done

echo "loadavg(after) = $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$RES/env.txt"
echo "done -> $RES"
