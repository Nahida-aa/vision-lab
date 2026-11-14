#!/usr/bin/env python3
"""汇总 run.sh 的原始结果 → 每实现 avg/P50/P95 + 峰值/平均 RSS。

口径：
- per-frame ms = 进程内逐帧计时（读图解码 + det + 后处理 + cls + rec + 聚合），
  不含进程启动 / 模型加载 / 抽帧。
- cpp 的 per-frame = imageLoadMs + detMs + postMs + recMs（stderr 逐帧记录），
  同样不含 modelLoadMs（只加载一次）。
- RSS 来自 monitor.py 的外部采样（峰值 = wait4 ru_maxrss，均值 = /proc VmRSS 采样均值）。
"""
import glob
import json
import os
import re
import statistics
import sys


def pct(sorted_vals, p):
    if not sorted_vals:
        return 0.0
    k = max(0, min(len(sorted_vals) - 1, int(round(p / 100.0 * (len(sorted_vals) - 1)))))
    return sorted_vals[k]


def stats(vals):
    s = sorted(vals)
    return {
        "n": len(s),
        "avg": statistics.mean(s),
        "p50": statistics.median(s),
        "p95": pct(s, 95),
        "min": s[0],
        "max": s[-1],
    }


CPP_RE = re.compile(
    r"detMs=(?P<det>[\d.]+) postMs=(?P<post>[\d.]+) recMs=(?P<rec>[\d.]+) "
    r"totalMs=(?P<total>[\d.]+) \(load: charList=(?P<cl>[\d.]+) image=(?P<img>[\d.]+) "
    r"model=(?P<model>[\d.]+)\)"
)


def load_cpp(path):
    per, det, post, rec, img, model = [], [], [], [], [], None
    for line in open(path, encoding="utf-8", errors="replace"):
        m = CPP_RE.search(line)
        if not m:
            continue
        d, p, r, i = (float(m[k]) for k in ("det", "post", "rec", "img"))
        per.append(d + p + r + i)
        det.append(d)
        post.append(p)
        rec.append(r)
        img.append(i)
        model = float(m["model"])
    return {"per": per, "det": det, "post": post, "rec": rec, "img": img, "model_load_ms": model}


def main():
    res = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "results")
    runs = sorted(glob.glob(os.path.join(res, "*-r*.json")))
    runs = [r for r in runs if not r.endswith(".mem.json")]

    grouped = {}
    for path in runs:
        name = os.path.basename(path)
        key = re.sub(r"-r\d+\.json$", "", name)
        d = json.load(open(path))
        if "per_frame_ms" in d:
            grouped.setdefault(key, []).extend(d["per_frame_ms"])
        elif "per_invocation_ms" in d:
            grouped.setdefault(key, []).extend(d["per_invocation_ms"])

    # cpp：从 stderr 逐帧取
    cpp = {"per": [], "det": [], "post": [], "rec": [], "img": [], "model_load_ms": None}
    for p in sorted(glob.glob(os.path.join(res, "cpp-v3-r*.stderr"))):
        r = load_cpp(p)
        for k in ("per", "det", "post", "rec", "img"):
            cpp[k].extend(r[k])
        cpp["model_load_ms"] = r["model_load_ms"]
    if cpp["per"]:
        grouped["cpp-v3(ORT)"] = cpp["per"]

    # RSS：每次 run 的峰值取 max，均值取各 run 平均再平均
    def mem(key):
        peaks, means, threads = [], [], []
        for p in sorted(glob.glob(os.path.join(res, f"{key}-r*.mem.json"))):
            d = json.load(open(p))
            peaks.append(d["peak_rss_mb"])
            means.append(d["mean_rss_mb"])
            threads.append(d.get("threads_max", 0))
        # python one-shot 的峰值写在其自身结果里
        if not peaks:
            for p in sorted(glob.glob(os.path.join(res, f"{key}-r*.json"))):
                d = json.load(open(p))
                if "peak_rss_mb" in d:
                    peaks.append(d["peak_rss_mb"])
        if not peaks:
            return None
        return {
            "peak": max(peaks),
            "peak_runs": peaks,
            "mean": statistics.mean(means) if means else None,
            "threads_max": max(threads) if threads else None,
        }

    print("## 逐帧耗时（ms）\n")
    print("| 实现 | 样本 n | avg | P50 | P95 | min | max |")
    print("| --- | ---: | ---: | ---: | ---: | ---: | ---: |")
    order = ["rust-v4", "rust-v3", "cpp-v3(ORT)", "py-v4", "py-v4-oneshot",
             "py-v4-arenaon", "rust-v4-autothreads", "py-v4-autothreads"]
    for key in order + [k for k in grouped if k not in order]:
        if key not in grouped:
            continue
        s = stats(grouped[key])
        print(
            f"| {key} | {s['n']} | {s['avg']:.1f} | {s['p50']:.1f} | {s['p95']:.1f} "
            f"| {s['min']:.1f} | {s['max']:.1f} |"
        )

    if cpp["det"]:
        print("\n## cpp 分相（ms，逐帧均值）\n")
        print("| 读图 | det | det 后处理 | rec | 模型加载(一次) |")
        print("| ---: | ---: | ---: | ---: | ---: |")
        print(
            f"| {statistics.mean(cpp['img']):.1f} | {statistics.mean(cpp['det']):.1f} "
            f"| {statistics.mean(cpp['post']):.1f} | {statistics.mean(cpp['rec']):.1f} "
            f"| {cpp['model_load_ms']:.0f} |"
        )

    print("\n## 内存（MB）\n")
    print("| 实现 | 峰值 RSS | 各轮峰值 | 平均 RSS | 线程峰值 |")
    print("| --- | ---: | --- | ---: | ---: |")
    for key in ["rust-v4", "rust-v3", "cpp-v3", "py-v4", "py-v4-oneshot",
                "py-v4-arenaon", "rust-v4-autothreads", "py-v4-autothreads"]:
        m = mem(key)
        if not m:
            continue
        peaks = ", ".join(f"{p:.0f}" for p in m["peak_runs"])
        mean = f"{m['mean']:.1f}" if m["mean"] is not None else "—"
        th = m["threads_max"] if m["threads_max"] is not None else "—"
        print(f"| {key} | {m['peak']:.1f} | {peaks} | {mean} | {th} |")

    # 轮间波动
    print("\n## 轮间波动（stdout 内 per-run 均值）\n")
    for key in order:
        per_run = []
        for path in sorted(glob.glob(os.path.join(res, f"{key}-r[0-9]*.json"))):  # noqa
            if path.endswith(".mem.json"):
                continue
            d = json.load(open(path))
            vals = d.get("per_frame_ms") or d.get("per_invocation_ms")
            if vals:
                per_run.append(statistics.mean(vals))
        if len(per_run) >= 2:
            spread = (max(per_run) - min(per_run)) / min(per_run) * 100
            runs_txt = ", ".join(f"{v:.1f}" for v in per_run)
            print(f"- {key}: {runs_txt}  → 波动 {spread:.1f}%")


if __name__ == "__main__":
    main()
