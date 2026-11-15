#!/usr/bin/env python3
"""Python 侧分阶段探针：与 Rust `phase_probe` 对齐的口径。

阶段：
  det_pre     = DetPreProcess(roi)    （resize + numpy normalize + CHW）
  det_full    = TextDetector(roi)     （pre + det 推理 + DB 后处理）
  engine_full = RapidOCR(roi)         （det + cls + rec + 聚合）
派生：
  det_infer_post = det_full - det_pre
  cls_plus_rec   = engine_full - det_full

注意：`det_full` 会让 det 在一帧里跑两次（独立循环），与 `phase_probe` 的
`detect_raw_boxes` 同口径；绝对值以 `engine_full` 为准。

用法：
  python3 py_phase.py v3 20 [--out out.json]
"""
import json
import os
import statistics
import sys
import time

REPO = "/home/aa/repos/ai_ls/vision-lab"
sys.path.insert(0, os.path.join(REPO, "packages/subtitle_ocr_py"))
sys.path.insert(0, os.path.join(REPO, "tests/bench/subtitle_ocr/rust-vs-py"))

import cv2  # noqa: E402
import numpy as np  # noqa: E402
import py_batch  # noqa: E402
import rapidocr_onnxruntime as rr  # noqa: E402

MODEL = sys.argv[1] if len(sys.argv) > 1 else "v3"
REPS = int(sys.argv[2]) if len(sys.argv) > 2 else 20
OUT = None
if "--out" in sys.argv:
    OUT = sys.argv[sys.argv.index("--out") + 1]
WARMUP = 2

extra = py_batch.model_kwargs(MODEL) if MODEL != "v4" else {}
base = rr.RapidOCR


def patched(*a, **kw):
    kw.setdefault("intra_op_num_threads", 4)
    kw.update(extra)
    return base(*a, **kw)


rr.RapidOCR = patched
engine = rr.RapidOCR()
frame = os.path.join(REPO, "packages/tmp/ocr-bench/oneframe-replay40/0000000.jpg")
img = cv2.imread(frame)
roi = img[int(img.shape[0] * 0.6):, :]

det = engine.text_det
for _ in range(WARMUP):
    engine(roi, text_score=0.45)


def timed(fn, reps):
    out = []
    for _ in range(reps):
        t = time.perf_counter()
        fn()
        out.append((time.perf_counter() - t) * 1000.0)
    return out


def stats(v):
    return {
        "avg": statistics.mean(v),
        "p50": statistics.median(v),
        "p95": float(np.percentile(v, 95)),
    }


prep = det.get_preprocess(max(roi.shape[:2]))
t_pre = timed(lambda: prep(roi), REPS)
t_det = timed(lambda: det(roi), REPS)
t_full = timed(lambda: engine(roi, text_score=0.45), REPS)

mean = statistics.mean
out = {
    "impl": "py-phase",
    "model": MODEL,
    "frame": frame,
    "roi": [roi.shape[1], roi.shape[0]],
    "reps": REPS,
    "warmup": WARMUP,
    "preprocess_det_ms": stats(t_pre),
    "det_full_ms": stats(t_det),
    "engine_ms": stats(t_full),
    "derived": {
        "det_infer_post_ms": mean(t_det) - mean(t_pre),
        "cls_plus_rec_ms": mean(t_full) - mean(t_det),
    },
}
print(
    f"py-{MODEL}: det_pre {mean(t_pre):.1f} | det_full {mean(t_det):.1f} | "
    f"engine {mean(t_full):.1f} | cls+rec {mean(t_full) - mean(t_det):.1f} | "
    f"det_infer_post {mean(t_det) - mean(t_pre):.1f}"
)
s = json.dumps(out, indent=2)
if OUT:
    with open(OUT, "w") as f:
        f.write(s)
