#!/usr/bin/env bash
# 复现「Rust vs Python 单帧耗时/内存」实测。
#
# 设计要点（口径纪律）：
# - 只测**同一帧**（`oneframe-replay40/` 里 40 个硬链接指向同一 inode），
#   不跑全视频；40 个副本只是为了让「常驻 session 的批量进程」产生 40 个样本，
#   以便算 P50/P95。
# - 所有实现读同一个目录 / 同一张图，均只做 OCR（抽帧在驱动层之外，不参与）。
# - `taskset -c 0-3` 把可用 CPU 固定为 4 个，**三个实现都显式钉 4 线程**：
#   cpp `--threads 4`、python `intra_op_num_threads=4`、rust `RAPIDOCR_INTRA_THREADS=4`。
#   rust 不钉会走 ORT 的默认全局线程池（按物理核/逻辑核开，本机宽到 16），在 4 CPU
#   的 taskset 内超订，实测单帧从 ~370ms 劣化到 ~640ms（1.7×）——所以必须钉死。
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
ONLY=${2:-all}   # all | rust | cpp | py  —— 只跑某一类，便于单独做 ORT 版本实验
SAMPLES=40
CPUSET=0-3

# cpp 构建目录 → 结果标签。每个目录都必须是用 -DORT_DIR=... 显式配置出来的，
# 否则 CMakeLists 的自动探测会挑 packages/tmp 下最新的一份，标签就会名不副实。
CPP_VARIANTS=(
  "cpp-v3-ort124:packages/subtitle_ocr_cpp/build"
  "cpp-v3-ort128:packages/subtitle_ocr_cpp/build-ort128"
  "cpp-v3-ort132:packages/subtitle_ocr_cpp/build-ort132"
)

want() { [[ "$ONLY" == all || "$ONLY" == "$1" ]]; }

mkdir -p "$RES"

# 探针与本包一起构建（不改变被测实现）。
if want rust; then
  cargo build -p bench_subtitle_ocr --release --bin probe >/dev/null 2>&1 || {
    echo "probe 构建失败（cargo build -p bench_subtitle_ocr --release --bin probe）" >&2
    exit 1
  }
fi

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
  echo "threads: 4 per impl (rust RAPIDOCR_INTRA_THREADS=4 / cpp --threads 4 / py intra_op_num_threads=4)"
  echo "frame: $FRAME (1 distinct image, hardlinked 40x)"
  echo "samples/run: $SAMPLES, runs: $REPS"
  echo "ort(py venv): $(packages/subtitle_ocr_py/.venv/bin/python -c 'import onnxruntime;print(onnxruntime.__version__)' 2>/dev/null || echo n/a)"
  echo "rust: $(./target/release/subtitle_ocr --help 2>/dev/null | head -1)" 
  echo "rapidocr_onnxruntime: $(packages/subtitle_ocr_py/.venv/bin/python -c 'import rapidocr_onnxruntime as r;print(getattr(r,\"__version__\",\"?\"))' 2>/dev/null)"
  # cpp 的 ORT 版本取自各自构建目录的 CMakeCache，避免标签与实际链接库不符
  for variant in "${CPP_VARIANTS[@]}"; do
    _label=${variant%%:*}
    _dir=${variant#*:}
    if [[ -x "$_dir/subtitle_ocr_ort_cpp" ]]; then
      _ort=$(grep -m1 '^ORT_DIR:' "$_dir/CMakeCache.txt" 2>/dev/null | cut -d= -f2-)
      echo "cpp[$_label]: ${_dir##*/} -> ${_ort:-unknown}"
    fi
  done
} >> "$RES/env.txt"

for r in $(seq 1 "$REPS"); do
  echo "=== run $r/$REPS   loadavg=$(cut -d' ' -f1-3 /proc/loadavg) ==="

  # rust v4（与 python 的权重版本一致）+ rust v3（与 cpp 一致）
  if want rust; then
  for m in v4 v3; do
    python3 "$HERE/monitor.py" "$RES/rust-$m-r$r.mem.json" -- \
      taskset -c "$CPUSET" env RAPIDOCR_INTRA_THREADS=4 \
      ./target/release/probe --dir "$FRAME_DIR" --model "$m" \
      --reps 1 --warmup 1 --threshold 0.45 --subtitle-only \
      --out "$RES/rust-$m-r$r.json" >/dev/null
  done

  # rust 关 arena（与 Python 包内默认 enable_cpu_mem_arena=False 对齐）
  for m in v4 v3; do
    python3 "$HERE/monitor.py" "$RES/rust-$m-arenaoff-r$r.mem.json" -- \
      taskset -c "$CPUSET" env RAPIDOCR_CPU_ARENA=0 RAPIDOCR_INTRA_THREADS=4 \
      ./target/release/probe --dir "$FRAME_DIR" --model "$m" \
      --reps 1 --warmup 1 --threshold 0.45 --subtitle-only \
      --out "$RES/rust-$m-arenaoff-r$r.json" >/dev/null
  done
  fi

  # cpp v3（权重写死 v3）：同一份源码分别链接三个 ORT 版本，
  # 用来把「C++ 比 Python 快多少」拆成「代码/进程模型」与「ORT 版本」两部分。
  if want cpp; then
  # 每轮轮换顺序：同一轮内若固定「先 124 后 128」而负载又在漂移，
  # 版本差异就会和先后顺序混在一起。轮换后每轮的平均顺序偏差被摊平。
  _n=${#CPP_VARIANTS[@]}
  _off=$(( (r - 1) % _n ))
  for _i in $(seq 0 $(( _n - 1 ))); do
    variant=${CPP_VARIANTS[$(( (_i + _off) % _n ))]}
    cpp_label=${variant%%:*}
    cpp_dir=${variant#*:}
    python3 "$HERE/monitor.py" "$RES/$cpp_label-r$r.mem.json" -- \
      taskset -c "$CPUSET" env \
        LD_LIBRARY_PATH="$cpp_dir" \
        OCR_MODELS_DIR=data/models/rapidocr \
        OCR_KEYS_PATH=data/models/rapidocr/ppocr_keys.json \
        "./$cpp_dir/subtitle_ocr_ort_cpp" \
        --dir "$FRAME_DIR" 0.45 --subtitle-only --threads 4 \
        >"$RES/$cpp_label-r$r.stdout" 2>"$RES/$cpp_label-r$r.stderr"
  done
  fi

  if want py; then
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
  fi
done

echo "loadavg(after) = $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$RES/env.txt"
echo "done -> $RES"
