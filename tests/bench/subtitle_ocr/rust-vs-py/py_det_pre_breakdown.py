#!/usr/bin/env python3
"""Python det 预处理内部拆解：resize / normalize / permute / expand+astype。"""
import os
import statistics
import sys
import time

REPO = "/home/aa/repos/ai_ls/vision-lab"
sys.path.insert(0, os.path.join(REPO, "packages/subtitle_ocr_py"))
sys.path.insert(0, os.path.join(REPO, "tests/bench/subtitle_ocr/rust-vs-py"))

import cv2  # noqa: E402
import numpy as np  # noqa: E402
import rapidocr_onnxruntime as rr  # noqa: E402

frame = os.path.join(REPO, "packages/tmp/ocr-bench/oneframe-replay40/0000000.jpg")
img = cv2.imread(frame)
roi = img[int(img.shape[0] * 0.6):, :]

# 不建 session，只取 DetPreProcess 的同一份实现。
from rapidocr_onnxruntime.ch_ppocr_det.utils import DetPreProcess  # noqa: E402

prep = DetPreProcess(736, "min", [0.5, 0.5, 0.5], [0.5, 0.5, 0.5])
resized = prep.resize(roi)

def timed(fn, reps=20):
    out = []
    for _ in range(3):
        fn()
    for _ in range(reps):
        t = time.perf_counter()
        fn()
        out.append((time.perf_counter() - t) * 1000.0)
    return out

t_resize = timed(lambda: prep.resize(roi))
t_norm = timed(lambda: prep.normalize(resized))
n = prep.normalize(resized)
t_perm = timed(lambda: prep.permute(n))
p = prep.permute(n)
t_expand = timed(lambda: np.expand_dims(p, axis=0).astype(np.float32))
t_all = timed(lambda: prep(roi))

for name, v in (("resize", t_resize), ("normalize", t_norm), ("permute", t_perm),
                ("expand+astype", t_expand), ("total __call__", t_all)):
    print(f"{name:15s} avg {statistics.mean(v):6.1f}  p50 {statistics.median(v):6.1f}")
print("input:", roi.shape, "-> resized", resized.shape)
