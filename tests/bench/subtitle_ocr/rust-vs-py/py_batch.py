#!/usr/bin/env python3
"""Python 版单帧耗时/内存驱动（测量代码，不改被测实现）。

两种模式，对应两个不同口径：

1. batch（默认）：一个进程里只建一次 `RapidOCR()`（= 3 个 ORT session 常驻），
   然后对**同一帧**重复 N 次，逐次计时。这是与 rust/cpp 的 `--dir` 可比的
   「纯单帧处理耗时」。
2. oneshot（`--oneshot`）：每次调用起一个 `main.py` 进程（= 每帧重建 3 个
   session），逐次记录墙钟 + 子进程峰值 RSS。这是旧基准 590ms/帧的来源。

线程数默认 4：包内 config.yaml 是 -1（= ORT 默认，本机 8 物理核），
这里在 import main 之前 patch `RapidOCR` 构造参数把它钉到 4，与 cpp
默认 / taskset 4 核对齐。不改 main.py 源码。

用法：
  python3 py_batch.py --frame <x.jpg> --reps 20 --threads 4 --out py.json
  python3 py_batch.py --frame <x.jpg> --reps 5 --oneshot --out py_oneshot.json
"""
import argparse
import json
import os
import subprocess
import sys
import time

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../../.."))
PY_PKG = os.path.join(REPO, "packages/subtitle_ocr_py")
VENV_PY = os.path.join(PY_PKG, ".venv/bin/python")
WARMUP = 2


def run_batch(frame, reps, threads, text_score, subtitle_only, arena_on=False):
    sys.path.insert(0, PY_PKG)

    if arena_on:
        # 诊断开关（只用于归因实验，不是产品路径）：包内 OrtInferSession 默认
        # enable_cpu_mem_arena=False，而 ort-rs / cpp 用的是 ORT 默认（True）。
        # 打开它，才能把「内存差」归因到 ORT 配置而不是 Python vs Rust。
        import rapidocr_onnxruntime.utils.infer_engine as ie

        orig = ie.OrtInferSession._init_sess_opts

        def patched(config):
            opt = orig(config)
            opt.enable_cpu_mem_arena = True
            return opt

        ie.OrtInferSession._init_sess_opts = staticmethod(patched)

    # 关键：在 import main 之前替换 RapidOCR 构造函数，使 main.py 里
    # 模块级的 `engine = RapidOCR()` 带上 intra_op_num_threads。
    import rapidocr_onnxruntime as rr

    base = rr.RapidOCR

    def patched(*a, **kw):
        kw.setdefault("intra_op_num_threads", threads)
        return base(*a, **kw)

    rr.RapidOCR = patched

    t0 = time.perf_counter()
    import main as pyocr  # noqa: E402  （模块导入时创建常驻 engine）
    engine_init_ms = (time.perf_counter() - t0) * 1000.0

    kwargs = {
        "bottom_only": True,
        "text_confidence_threshold": text_score,
        "subtitle_only": subtitle_only,
        "providers": ["CPUExecutionProvider"],
    }

    for _ in range(WARMUP):
        pyocr.ocr_frame(frame, **kwargs)

    samples, inner_ms, texts = [], [], []
    for _ in range(reps):
        t = time.perf_counter()
        lines, inference_ms = pyocr.ocr_frame(frame, **kwargs)
        samples.append((time.perf_counter() - t) * 1000.0)
        inner_ms.append(inference_ms)
        if not texts:
            texts = [x["text"] for x in lines]

    return {
        "impl": "python",
        "mode": "batch",
        "arena_on": arena_on,
        "threads": threads,
        "frame": os.path.basename(frame),
        "engine_init_ms": engine_init_ms,
        "warmup": WARMUP,
        "per_frame_ms": samples,          # 含 cv2.imread 解码，与 rust/cpp 同口径
        "engine_only_ms": inner_ms,       # 仅 engine(roi) 内部
        "texts": texts,
    }


def run_oneshot(frame, reps, text_score, subtitle_only):
    import resource

    per_frame, inner = [], []
    for _ in range(reps):
        cmd = [VENV_PY, os.path.join(PY_PKG, "main.py"), frame,
               "--text-confidence-threshold", str(text_score)]
        if subtitle_only:
            cmd.append("--subtitle-only")
        t = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True, text=True)
        wall = (time.perf_counter() - t) * 1000.0
        if p.returncode != 0:
            raise RuntimeError(f"main.py 失败 rc={p.returncode}: {p.stdout[:300]} {p.stderr[:300]}")
        per_frame.append(wall)
        out = json.loads(p.stdout) if p.stdout.strip() else {}
        if "inference_ms" in out:
            inner.append(out["inference_ms"])
    # 所有子进程内容一致，ru_maxrss 的最大值即「一个 per-frame 进程」的峰值 RSS。
    peak_mb = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss / 1024.0
    return {
        "impl": "python",
        "mode": "oneshot",
        "frame": os.path.basename(frame),
        "per_invocation_ms": per_frame,   # 进程启动 + import + 建 session + 推理 + 退出
        "engine_only_ms": inner,          # 进程内自报的 engine(roi) 耗时
        "peak_rss_mb": peak_mb,           # 单个 per-frame 进程的峰值 RSS
        "reps": reps,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--frame", required=True)
    ap.add_argument("--reps", type=int, default=20)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--text-score", type=float, default=0.45)
    ap.add_argument("--subtitle-only", action="store_true", default=True)
    ap.add_argument("--oneshot", action="store_true")
    ap.add_argument("--arena-on", action="store_true")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    if a.oneshot:
        out = run_oneshot(a.frame, a.reps, a.text_score, a.subtitle_only)
    else:
        out = run_batch(a.frame, a.reps, a.threads, a.text_score, a.subtitle_only, a.arena_on)

    with open(a.out, "w") as f:
        json.dump(out, f, indent=2, ensure_ascii=False)
    print(json.dumps({k: v for k, v in out.items() if k not in ("per_frame_ms", "engine_only_ms", "per_invocation_ms")}, ensure_ascii=False))


if __name__ == "__main__":
    main()
