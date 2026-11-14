#!/usr/bin/env bash
# 生成「同一帧重复 40 次」的帧目录：硬链接到同一 inode，物理上只有 1 张图。
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../../../.." && pwd)
cd "$ROOT"
SRC=${1:-packages/tmp/ocr-bench/frames-ocr-rust-fps2-so-ts0.45/0020000.jpg}
OUT=${2:-packages/tmp/ocr-bench/oneframe-replay40}
N=${3:-40}

rm -rf "$OUT"; mkdir -p "$OUT"
for i in $(seq 0 $((N - 1))); do
  ln "$SRC" "$OUT/$(printf '%07d' $((i * 500))).jpg"
done
echo "$OUT: $N entries, $(stat -c '%h' "$SRC") links, 1 distinct image ($SRC)"
