#!/usr/bin/env python3
"""子进程资源监控：峰值/平均 RSS + 墙钟 + 线程数。

用法：
    python3 monitor.py <out.json> -- <cmd...>

口径：
- 峰值 RSS 取 `os.wait4` 的 ru_maxrss（内核记账的精确峰值，KB）。
- 平均 RSS 由外部按 20ms 周期采样 `/proc/<pid>/status` 的 VmRSS 得到，
  从子进程出现开始采样到退出；首采样前的那段（解释器启动）不计入均值。
- 峰值 RSS 与平均 RSS 都包含进程启动、模型加载等一次性部分——这正是
  「跑一个进程要占多少内存」的口径。纯推理期的增量另由进程内采样补足。
"""
import json
import os
import subprocess
import sys
import time

STATUS = "/proc/{pid}/status"


def read_status(pid):
    vals = {}
    try:
        with open(STATUS.format(pid=pid), "r") as f:
            for line in f:
                if ":" in line:
                    k, v = line.split(":", 1)
                    vals[k.strip()] = v.strip()
    except (FileNotFoundError, ProcessLookupError):
        return None
    return vals


def kb(field):
    # "12345 kB" -> 12345
    try:
        return int(field.split()[0])
    except Exception:
        return 0


def main():
    out_path = sys.argv[1]
    sep = sys.argv.index("--")
    cmd = sys.argv[sep + 1:]

    t0 = time.monotonic()
    proc = subprocess.Popen(cmd)
    samples = []
    threads = []

    while proc.poll() is None:
        st = read_status(proc.pid)
        if st:
            samples.append(kb(st.get("VmRSS", "0 kB")))
            threads.append(int(st.get("Threads", "0") or 0))
        time.sleep(0.02)

    # Popen 已回收子进程，returncode 由它记录；峰值用 getrusage(RUSAGE_CHILDREN)
    # （本进程只 spawn 这一个子进程，故就是该子进程的精确峰值）。
    import resource

    ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    wall_ms = (time.monotonic() - t0) * 1000.0

    peak_kb = max([max(samples) if samples else 0, ru.ru_maxrss])
    mean_kb = sum(samples) / len(samples) if samples else 0

    out = {
        "cmd": cmd,
        "exit_code": proc.returncode,
        "wall_ms": wall_ms,
        "peak_rss_mb": peak_kb / 1024.0,
        "mean_rss_mb": mean_kb / 1024.0,
        "samples": len(samples),
        "threads_max": max(threads) if threads else 0,
        "user_s": ru.ru_utime,
        "sys_s": ru.ru_stime,
    }
    with open(out_path, "w") as f:
        json.dump(out, f, indent=2, ensure_ascii=False)
    print(json.dumps(out, ensure_ascii=False))


if __name__ == "__main__":
    main()
