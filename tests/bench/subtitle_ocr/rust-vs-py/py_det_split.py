#!/usr/bin/env python3
"""Python det 推理单独计时：pre / infer / post 三段。

目的：回答「Python 与 Rust 的 det 推理是否一样慢/快」——把 ORT `Run` 单独拎出来，
避免被 numpy 预处理（40ms）和 cls/rec 段掩盖。配合 `--arena-on` 可复现 §4.6 里
「arena 对齐后 det 推理差 ≤2%」的结论（Python 包内默认 arena 关，Rust/C++ 默认开）。

用法：
  python3 py_det_split.py v3 [--arena-on]
"""
import os
import statistics
import sys
import time

REPO = "/home/aa/repos/ai_ls/vision-lab"
sys.path.insert(0, os.path.join(REPO, "packages/subtitle_ocr_py"))
sys.path.insert(0, os.path.join(REPO, "tests/bench/subtitle_ocr/rust-vs-py"))

import cv2  # noqa: E402
import py_batch  # noqa: E402
import rapidocr_onnxruntime as rr  # noqa: E402

MODEL = sys.argv[1] if len(sys.argv) > 1 else "v3"
ARENA_ON = "--arena-on" in sys.argv

if ARENA_ON:
    import rapidocr_onnxruntime.utils.infer_engine as ie

    orig = ie.OrtInferSession._init_sess_opts

    def patched_opts(config):
        opt = orig(config)
        opt.enable_cpu_mem_arena = True
        return opt

    ie.OrtInferSession._init_sess_opts = staticmethod(patched_opts)

extra = py_batch.model_kwargs(MODEL) if MODEL != "v4" else {}
base = rr.RapidOCR


def patched(*a, **kw):
    kw.setdefault("intra_op_num_threads", 4)
    kw.update(extra)
    return base(*a, **kw)


rr.RapidOCR = patched
engine = rr.RapidOCR()
img = cv2.imread(os.path.join(REPO, "packages/tmp/ocr-bench/oneframe-replay40/0000000.jpg"))
roi = img[int(img.shape[0] * 0.6):, :]
det = engine.text_det
for _ in range(2):
    engine(roi, text_score=0.45)

prep = det.get_preprocess(max(roi.shape[:2]))
pre = prep(roi)


def timed(fn, n=20):
    for _ in range(2):
        fn()
    out = []
    for _ in range(n):
        t = time.perf_counter()
        r = fn()
        out.append((time.perf_counter() - t) * 1000)
    return out, r


t_pre, _ = timed(lambda: prep(roi))
t_infer, preds = timed(lambda: det.infer(pre)[0])
t_post, _ = timed(lambda: det.postprocess_op(preds, (roi.shape[0], roi.shape[1])))
t_det, _ = timed(lambda: det(roi))
tag = "arenaon" if ARENA_ON else "arenaoff"
print(
    f"py-{MODEL}-{tag}: det pre {statistics.mean(t_pre):.1f} | "
    f"infer {statistics.mean(t_infer):.1f} p50 {statistics.median(t_infer):.1f} | "
    f"post {statistics.mean(t_post):.1f} | det_full {statistics.mean(t_det):.1f}"
)
print("  preds shape:", preds.shape)
