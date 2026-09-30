#!/usr/bin/env python3
"""A/B 对比脚本：读 Rust/C++ 的 ab_dump 指纹 + raw 数组，定位首个像素级差异。

用法:
  ab_compare.py <rust_fingerprint.txt> <cpp_fingerprint.txt> <rust_prefix> <cpp_prefix> <w>

rust_fingerprint.txt / cpp_fingerprint.txt: ab_dump 的 stdout（stage=... 行）。
rust_prefix / cpp_prefix: ab_dump 的 <out_prefix>，用于定位 *.raw 文件。
w: 图像宽（用于像素坐标换算）。

输出:
  1) 各阶段指纹对比表（total/top26/mid331/hash）
  2) 首个差异阶段
  3) 对首个差异阶段，精确 diff raw 数组 → 首个差异像素坐标 + 数量
"""

import sys
import os
import re


def parse_fingerprint(path):
    """解析 ab_dump stdout 的 stage= 行。返回 {stage: (total, top26, mid331, hash)}。"""
    stages = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line.startswith("stage="):
                continue
            # 跳过 get_im_ff 的 lb_a/le_a 行（无数字指纹）。
            if "lb_a=" in line:
                continue
            m = re.match(
                r"stage=(\S+) total=(\d+) top26=(\d+) mid331=(\d+) hash=(0x[0-9a-f]+|[0-9a-f]+)", line
            )
            if m:
                stages[m.group(1)] = (int(m.group(2)), int(m.group(3)), int(m.group(4)), m.group(5).lstrip("0x"))
    return stages


def diff_raw(a_path, b_path, w):
    """精确 diff 两个 raw 数组，返回 (首个差异索引, 差异像素数)。"""
    a = open(a_path, "rb").read()
    b = open(b_path, "rb").read()
    if len(a) != len(b):
        return None, f"长度不同 {len(a)} vs {len(b)}"
    diffs = []
    for i in range(len(a)):
        if a[i] != b[i]:
            diffs.append(i)
            if len(diffs) >= 1000:
                break
    if not diffs:
        return None, 0
    first = diffs[0]
    x = first % w
    y = first // w
    return (y, x, len(diffs)), f"首差异像素 idx={first} (y={y},x={x}) 差异数(截断1000)={len(diffs)}"


def main():
    if len(sys.argv) < 6:
        print(__doc__)
        sys.exit(1)
    rust_fp, cpp_fp, rust_pref, cpp_pref, w = (
        sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
    )

    r = parse_fingerprint(rust_fp)
    c = parse_fingerprint(cpp_fp)

    stages = ["y", "u", "v", "ff", "sf", "ne0", "he", "ne", "sf_step1", "tf", "sf_filtered"]
    print(f"{'stage':<14} {'R total':>8} {'C total':>8} {'R top26':>8} {'C top26':>8} "
          f"{'R mid331':>8} {'C mid331':>8}   hash R==C")
    first_diff = None
    for s in stages:
        if s in r and s in c:
            rt, r26, r331, rh = r[s]
            ct, c26, c331, ch = c[s]
            same = "OK " if rh == ch else "DIFF"
            print(f"{s:<14} {rt:>8} {ct:>8} {r26:>8} {c26:>8} {r331:>8} {c331:>8}   {same}")
            if first_diff is None and rh != ch:
                first_diff = s
        else:
            print(f"{s:<14}   (缺失: Rust={'有' if s in r else '无'} C++={'有' if s in c else '无'})")

    if first_diff is None:
        print("\n✅ 所有阶段指纹一致（此输入无差异）")
        return

    print(f"\n首个差异阶段: {first_diff}")
    a_path = os.path.join(os.path.dirname(rust_pref) or ".", f"{rust_pref}.{first_diff}.raw")
    b_path = os.path.join(os.path.dirname(cpp_pref) or ".", f"{cpp_pref}.{first_diff}.raw")
    # 实际 raw 文件路径是 {prefix}.{stage}.raw，prefix 可能带目录。
    a_path = f"{rust_pref}.{first_diff}.raw"
    b_path = f"{cpp_pref}.{first_diff}.raw"
    if os.path.exists(a_path) and os.path.exists(b_path):
        info, detail = diff_raw(a_path, b_path, w)
        print(f"  {detail}")
        if info:
            print(f"  首差异: y={info[0]} x={info[1]}")


if __name__ == "__main__":
    main()
