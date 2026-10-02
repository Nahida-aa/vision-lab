#!/usr/bin/env node
// OpenCV 开发环境一键准备（Windows 为主；Linux 走系统包 / pkg-config，无需本脚本）。
//
// 做的事：下载 OpenCV 官方 prebuilt（sha256 pin 校验）→ 解压到约定位置 →
// 打印（或经 --apply 写入）opengl crate 探测所需的三个环境变量。版本与路径
// 对齐 release-windows.yml（CI 构建即发布构建的 OpenCV 环境），使
// 开发者环境 == CI 环境 == 发布环境，基准结果与 CI / golden 可比。
//
// 社区惯例说明：刻意**不在 build.rs 里做下载**——build script 做网络 IO 会废掉
// `cargo build --offline`、cargo clean 后重下、失败报错难读；检测留在
// packages/rapidocr_ort/build.rs，下载交给本脚本。
//
// 用法：
//   node scripts/setup_opencv.mjs                 # 下载 + 解压 + 打印 env
//   node scripts/setup_opencv.mjs --apply         # 同上，并 setx 持久化（Windows）
//   node scripts/setup_opencv.mjs --from <file>   # 用本地归档（离线 / 已下载过，仍校验 sha256）
//   node scripts/setup_opencv.mjs --base-url <u>  # 换下载源（GitHub 直连慢时指向镜像）
//   node scripts/setup_opencv.mjs --dest <dir>    # 自定安装位置（默认 C:\opencv5，与 CI 一致）
//   node scripts/setup_opencv.mjs --force         # 已就绪也重新下载解压
//   node scripts/setup_opencv.mjs --keep-archive  # 成功后保留归档（默认删除省 260MB）
//
// 需要 Node 18+（global fetch）。7-Zip 需已安装（官方自解压包用 7z 解出）。

import { createHash } from 'node:crypto';
import { createWriteStream, existsSync, mkdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

// ---- 固定 pin（版本升级 = 改这三行 + 核对解压布局） ------------------------

const VERSION = '5.0.0';
const FILE = `opencv-${VERSION}-windows.exe`;
const DEFAULT_BASE_URL = `https://github.com/opencv/opencv/releases/download/${VERSION}`;
// 官方 prebuilt 的 sha256（对 opencv-5.0.0-windows.exe 全文件计算）。
const SHA256 = '9c6c1fcea58acdf06edba13148b2246e00c2658143fa51e61ecd370db8c39f63';
// 与 release-windows.yml 的 env 三连完全一致（include / link / libs）。
const LINK_LIBS = 'opencv_world500';

// ---- 参数 -------------------------------------------------------------------

function parseArgs(argv) {
  const o = {
    dest: process.platform === 'win32' ? 'C:\\opencv5' : path.join(tmpdir(), 'opencv5'),
    baseUrl: process.env.OPENCV_SETUP_BASE_URL || DEFAULT_BASE_URL,
    from: null,
    force: false,
    apply: false,
    keepArchive: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--dest') o.dest = argv[++i];
    else if (a === '--base-url') o.baseUrl = argv[++i];
    else if (a === '--from') o.from = argv[++i];
    else if (a === '--force') o.force = true;
    else if (a === '--apply') o.apply = true;
    else if (a === '--keep-archive') o.keepArchive = true;
    else if (a === '--help' || a === '-h') { usage(); process.exit(0); }
    else { console.error(`未知参数: ${a}`); usage(); process.exit(2); }
  }
  return o;
}

function usage() {
  console.log('用法: node scripts/setup_opencv.mjs [--dest <dir>] [--base-url <u>] [--from <file>] [--force] [--apply] [--keep-archive]');
}

const args = parseArgs(process.argv.slice(2));

// ---- 工具 -------------------------------------------------------------------

const envValues = () => ({
  OPENCV_INCLUDE_PATHS: `${args.dest}/opencv/build/include`,
  OPENCV_LINK_PATHS: `${args.dest}/opencv/build/x64/vc16/lib`,
  OPENCV_LINK_LIBS: LINK_LIBS,
});

const versionHeader = () => path.join(args.dest, 'opencv', 'build', 'include', 'opencv2', 'core', 'version.hpp');

function printEnv() {
  const v = envValues();
  console.log('\n配置 opencv crate 探测环境（与 release-windows.yml 一致）：');
  if (process.platform === 'win32') {
    for (const [k, val] of Object.entries(v)) console.log(`  setx ${k} "${val}"`);
    console.log('（或直接 node scripts/setup_opencv.mjs --apply 由本脚本执行 setx；需新开终端生效）');
  } else {
    for (const [k, val] of Object.entries(v)) console.log(`  export ${k}="${val}"`);
    console.log('（Linux 一般无需本脚本：pkg-config 能找到系统 OpenCV 即可）');
  }
}

function log(msg) { console.log(msg); }

async function downloadToFile(url, file) {
  log(`下载 ${url}`);
  const resp = await fetch(url);
  if (!resp.ok || !resp.body) {
    console.error(`下载失败: HTTP ${resp.status}`);
    console.error('GitHub 直连慢/不通时，用 --base-url 指向镜像（需提供同名文件），或 --from 指定本地归档。');
    process.exit(1);
  }
  const total = Number(resp.headers.get('content-length') || 0);
  const hash = createHash('sha256');
  const ws = createWriteStream(file);
  let done = 0, lastPct = -10;
  for await (const chunk of resp.body) {
    hash.update(chunk);
    ws.write(Buffer.from(chunk));
    done += chunk.length;
    if (total) {
      const pct = Math.floor((done / total) * 100);
      if (pct >= lastPct + 10) {
        lastPct = pct;
        process.stdout.write(`  ${pct}%  (${(done / 1e6).toFixed(0)} MB)\r\n`);
      }
    }
  }
  await new Promise((res, rej) => { ws.end(res); ws.on('error', rej); });
  const sha = hash.digest('hex');
  if (sha !== SHA256) {
    rmSync(file, { force: true });
    console.error(`sha256 校验失败:\n  期望 ${SHA256}\n  实际 ${sha}\n归档已删除（下载源可疑或文件被更换，勿强行绕过）。`);
    process.exit(1);
  }
  log(`  sha256 校验通过 (${sha.slice(0, 12)}…)`);
}

function find7z() {
  const candidates = process.platform === 'win32'
    ? ['7z', 'C:\\Program Files\\7-Zip\\7z.exe']
    : ['7z', '7za'];
  for (const c of candidates) {
    try { execFileSync(c, ['i'], { stdio: 'ignore' }); return c; } catch { /* 下一个 */ }
  }
  console.error('未找到 7-Zip。官方自解压包需 7z 解出：');
  console.error('  Windows: winget install 7zip.7zip   （或装到默认路径自动被找到）');
  console.error('  Linux:   sudo pacman -S p7zip / apt install p7zip-full');
  process.exit(1);
}

// ---- 主流程 -------------------------------------------------------------------

mkdirSync(args.dest, { recursive: true });

if (!args.force && existsSync(versionHeader())) {
  log(`OpenCV ${VERSION} 已就绪: ${versionHeader()}`);
  log('（--force 重新下载解压）');
  printEnv();
  process.exit(0);
}

// 1. 取归档（本地 or 下载），均校验 sha256。
const archive = args.from ?? path.join(args.dest, FILE);
if (args.from) {
  if (!existsSync(args.from)) { console.error(`本地归档不存在: ${args.from}`); process.exit(1); }
  const hash = createHash('sha256');
  hash.update(readFileSync(args.from));
  const sha = hash.digest('hex');
  if (sha !== SHA256) {
    console.error(`sha256 校验失败:\n  期望 ${SHA256}\n  实际 ${sha}`);
    process.exit(1);
  }
  log(`本地归档 sha256 校验通过 (${sha.slice(0, 12)}…)`);
} else {
  await downloadToFile(`${args.baseUrl}/${FILE}`, archive);
}

// 2. 解压（自解压包本质是 7z 归档，解出 <dest>/opencv/...）。
const sevenZip = find7z();
log(`解压到 ${args.dest} ...`);
execFileSync(sevenZip, ['x', archive, `-o${args.dest}`, '-y'], { stdio: ['ignore', 'ignore', 'inherit'] });

// 3. 校验解压布局 + 主版本。
if (!existsSync(versionHeader())) {
  console.error(`解压后未找到 ${versionHeader()}（布局可能变化），检查 ${args.dest} 下内容。`);
  process.exit(1);
}
const header = readFileSync(versionHeader(), 'utf8');
const major = header.split('\n')
  .map((l) => l.trim().replace(/^#define CV_VERSION_MAJOR\s*/, ''))
  .find((v) => /^\d+$/.test(v));
if (major !== '5') {
  console.error(`版本头主版本为 ${major}，期望 5（预编译包与 pin 不符？）`);
  process.exit(1);
}
log(`OpenCV ${VERSION} 就绪: ${versionHeader()}`);

// 4. 清理归档（默认）。
if (!args.keepArchive && !args.from) {
  rmSync(archive, { force: true });
  log('已删除归档（--keep-archive 保留）');
}

printEnv();

// 5. --apply：Windows setx 持久化（仅新开终端生效）。
if (args.apply) {
  if (process.platform !== 'win32') {
    console.error('--apply 仅支持 Windows（Linux 用 export 或直接依赖 pkg-config）。');
    process.exit(1);
  }
  for (const [k, val] of Object.entries(envValues())) {
    execFileSync('setx', [k, val], { stdio: 'inherit' });
  }
  log('已写入用户级环境变量，**新开终端**后生效。');
}
