#!/usr/bin/env node
// 批量去掉文件名前缀（默认 `frame_`），便于对接 subtitle_ocr 的 `--dir` 时间命名约定。
//
// subtitle_ocr 的 `list_frames` 要求文件名本身即时间数值（`ms` 或 `ms_ms`，可前置 0），
// 不允许 `frame_` 这类语义前缀，否则会被当作 bad-name 跳过/报错。本脚本把目录下所有
// 以指定前缀开头的文件改名、剥掉前缀：
//
//     frame_0000030.jpg  →  0000030.jpg
//
// 通用、可复用：其他文件夹若有类似 `shot_` / `clip_` 等前缀，传 --prefix 即可。
//
// 用法：
//     node scripts/rename_strip_prefix.mjs <dir> [--prefix frame_] [--dry-run]
//
//   - <dir>：必填，目标目录（支持相对仓库根或绝对路径）。
//   - --prefix：要剥掉的前缀，默认 frame_。
//   - --dry-run：只打印将要执行的改名，不真的改。
//   - 目标文件名已存在时跳过该文件并报 collision，避免覆盖丢数据。
//
// 零依赖（node 内置 fs / path）。

import { readdirSync, renameSync, existsSync } from "node:fs";
import { join } from "node:path";

function parseArgs(argv) {
  const positional = [];
  const opts = { prefix: "frame_", dryRun: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--prefix") {
      opts.prefix = argv[++i] ?? "";
    } else if (a === "--dry-run") {
      opts.dryRun = true;
    } else if (a.startsWith("--prefix=")) {
      opts.prefix = a.slice("--prefix=".length);
    } else if (a.startsWith("-") && a !== "-") {
      console.error(`未知参数: ${a}`);
      process.exit(1);
    } else {
      positional.push(a);
    }
  }
  return { dir: positional[0], opts };
}

function stripPrefixInDir(directory, prefix, dryRun) {
  if (!existsSync(directory)) {
    console.error(`错误：目录不存在: ${directory}`);
    process.exit(1);
  }
  let renamed = 0;
  let skipped = 0;
  // 排序后处理，输出稳定可复现。
  for (const name of readdirSync(directory).sort()) {
    if (!name.startsWith(prefix)) continue;
    const newName = name.slice(prefix.length);
    const src = join(directory, name);
    const dst = join(directory, newName);
    if (existsSync(dst)) {
      console.log(`SKIP collision: ${name} -> ${newName}（目标已存在，跳过以免覆盖）`);
      skipped++;
      continue;
    }
    if (dryRun) {
      console.log(`would rename: ${name} -> ${newName}`);
      renamed++;
      continue;
    }
    renameSync(src, dst);
    console.log(`renamed: ${name} -> ${newName}`);
    renamed++;
  }
  return { renamed, skipped };
}

function main() {
  const { dir, opts } = parseArgs(process.argv.slice(2));
  if (!dir) {
    console.error("用法: node scripts/rename_strip_prefix.mjs <dir> [--prefix frame_] [--dry-run]");
    process.exit(1);
  }
  const { renamed, skipped } = stripPrefixInDir(dir, opts.prefix, opts.dryRun);
  const mode = opts.dryRun ? "dry-run" : "actual";
  console.log(`\n[${mode}] renamed=${renamed} skipped=${skipped} (prefix=${JSON.stringify(opts.prefix)})`);
}

main();
