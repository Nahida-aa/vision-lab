//! subtitle_ocr 基准的共享算法（灰盒测试与性能基准共用）。
//! 移植自 LocalDub 的 benchmark-ocr-video.ts / ocrMerge.ts / eval-ocr.ts，
//! 用于精确复刻 `--engine cpp` 的基准路径以便对比结果一致性。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 仓库根路径（CARGO_MANIFEST_DIR = .../tests/bench/subtitle_ocr，上溯 3 级）。
/// 供各 bin（bench / test / sf_ocr）共用，避免重复定义。
pub fn repo_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent() // .../tests/bench
        .unwrap()
        .parent() // .../tests
        .unwrap()
        .parent() // 仓库根
        .unwrap()
        .to_path_buf()
}

// ===========================================================================
// 帧结果与段
// ===========================================================================

/// 单帧 OCR 结果（对齐 LocalDub 的 FrameResult）。
/// bbox 为 (top, bottom)；cpp 单帧路径下不传 box，故为 None。
#[derive(Clone)]
pub struct FrameResult {
    pub text: String,
    pub timestamp: u64, // ms
    pub confidence: f64,
    pub bbox: Option<(f32, f32)>, // (top, bottom)，cpp 逐帧模式未提供
}

/// 合并后的段（对齐 LocalDub 的 Segment）。
#[derive(Clone)]
pub struct Segment {
    pub text: String,
    pub start: u64, // ms
    pub end: u64,   // ms
    pub confidence: Option<f64>,
    /// 横向值域 `[min_x, max_x]`（像素坐标），无字幕为 `[0,0]`。
    pub x_range: [f32; 2],
    /// 纵向值域 `[min_y, max_y]`（像素坐标），无字幕为 `[0,0]`。
    pub y_range: [f32; 2],
}

// ===========================================================================
// 编辑距离 / CER
// ===========================================================================

/// Levenshtein 编辑距离（对齐 ocrMerge.ts / eval-ocr.ts）。
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..=m {
        dp[i][0] = i;
    }
    for j in 0..=n {
        dp[0][j] = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                dp[i - 1][j].min(dp[i][j - 1]).min(dp[i - 1][j - 1]) + 1
            };
        }
    }
    dp[m][n]
}

/// CER = 编辑距离 / max(ref.len, 1)（对齐 eval-ocr.ts computeCER）。
pub fn compute_cer(reference: &str, hyp: &str) -> f64 {
    if reference.is_empty() && hyp.is_empty() {
        return 0.0;
    }
    levenshtein(reference, hyp) as f64 / (reference.chars().count().max(1)) as f64
}

/// 对齐 eval-ocr.ts normalizeForCER：
/// 1. 师父 → 师傅（同音字统一）
/// 2. 去所有空白
/// 3. 去中英文标点
/// 4. 数字（阿拉伯 + 中文数字）→ # 占位
pub fn normalize_for_cer(s: &str) -> String {
    let mut t = s.replace("师父", "师傅");
    // 去空白
    t = t.chars().filter(|c| !c.is_whitespace()).collect();
    // 去标点（覆盖 eval-ocr.ts 的字符集）
    let punct: &[char] = &[
        '。', '，', '！', '？', '、', '；', '：', '“', '”', '‘', '’', '「', '」', '【', '】', '《',
        '》', '（', '）', '.', ',', '!', '?', ';', ':', '\'', '"', '(', ')', '[', ']', '{', '}',
        '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{3000}', '\u{3001}', '\u{3002}',
        '\u{ff01}', '\u{ff0c}', '\u{ff1f}', '\u{ff1a}', '\u{ff1b}', '\u{2026}', '—', '～', '~',
        '-',
    ];
    t = t.chars().filter(|c| !punct.contains(c)).collect();
    // 数字 → #
    let digit_repl: &[char] = &[
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '零', '一', '二', '两', '三', '四', '五',
        '六', '七', '八', '九', '十', '百', '千', '万', '亿',
    ];
    let mut out = String::new();
    let mut prev_digit = false;
    for c in t.chars() {
        if digit_repl.contains(&c) {
            if !prev_digit {
                out.push('#');
            }
            prev_digit = true;
        } else {
            out.push(c);
            prev_digit = false;
        }
    }
    out
}

// ===========================================================================
// 时序对齐质量（偏移量 / IoU / 漏检虚检 / 配对后 CER）
// ===========================================================================
//
// 现有 CER 把所有 segment 拼成一整串比对，时间戳被完全丢弃——字幕整体早/晚
// 500ms、断句位置错位都不会反映在 CER 上。这里按时间区间配对后再统计，
// 用于衡量 merge_frames 的边界质量（抽帧 fps=2 时时间分辨率为 500ms）。

/// 带时间的文本条目（GT 与 hyp 通用）。
#[derive(Clone, Debug)]
pub struct TimedText {
    pub text: String,
    pub start: u64, // ms
    pub end: u64,   // ms
}

/// 两个区间是否有时间关联（含零时长段的容错）。
///
/// merge_frames 对「只在单帧出现」的字幕会产出 start==end 的零时长段
/// （实测 341 帧里有 4 条）。这是 LocalDub 原版行为，此处不修改产出，
/// 但配对时若按纯交集判定，零时长段与任何区间的交集恒为 0，会被误计为
/// 「虚检 + 漏检」各一次。故对退化区间改用「点落在对方区间内」判定。
fn related(a: &TimedText, b: &TimedText) -> bool {
    let a_deg = a.end <= a.start;
    let b_deg = b.end <= b.start;
    match (a_deg, b_deg) {
        // 都退化：时间点相同才算关联
        (true, true) => a.start == b.start,
        // a 退化为点：点落在 b 的闭区间内
        (true, false) => a.start >= b.start && a.start <= b.end,
        (false, true) => b.start >= a.start && b.start <= a.end,
        // 都是正常区间：按交集
        (false, false) => intersect_ms(a, b) > 0,
    }
}

/// 两个区间的交集时长（ms）。
fn intersect_ms(a: &TimedText, b: &TimedText) -> u64 {
    let lo = a.start.max(b.start);
    let hi = a.end.min(b.end);
    hi.saturating_sub(lo)
}

/// 时序 IoU = 交集 / 并集。
///
/// 零时长段的交集恒为 0，直接算会得到 IoU=0 而排在配对候选最末。
/// 为让它能配上正确的 GT，退化区间改用「距离衰减」代理分数：
/// 点落在 GT 区间内时给一个随 GT 时长递减的小正分（始终低于任何真实
/// 重叠配对），既能参与配对，又不会抢占正常区间的最佳匹配。
fn temporal_iou(a: &TimedText, b: &TimedText) -> f64 {
    let a_deg = a.end <= a.start;
    let b_deg = b.end <= b.start;
    if a_deg || b_deg {
        if !related(a, b) {
            return 0.0;
        }
        let dur = if a_deg {
            b.end.saturating_sub(b.start)
        } else {
            a.end.saturating_sub(a.start)
        };
        // 视作 1ms 的点与对方区间的 IoU，恒为极小正数
        return 1.0 / (dur.max(1) as f64 + 1.0);
    }
    let inter = intersect_ms(a, b) as f64;
    let union = (a.end.saturating_sub(a.start) + b.end.saturating_sub(b.start)) as f64 - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// 一对配对结果。
pub struct Pair {
    pub gt_idx: usize,
    pub hyp_idx: usize,
    pub iou: f64,
    pub start_delta: i64, // hyp.start - gt.start，正=偏晚
    pub end_delta: i64,   // hyp.end - gt.end
    pub cer: f64,         // 该条归一化后的 CER
}

pub struct AlignReport {
    pub pairs: Vec<Pair>,
    pub missed: usize,        // GT 有、无 hyp 与之重叠（漏检）
    pub spurious: usize,      // hyp 有、无 GT 与之重叠（虚检）
    /// hyp 中 start>=end 的退化段数。配对时已容错，但这是 merge_frames
    /// 的已知缺陷（单帧字幕），单独报出来避免被指标掩盖。
    pub zero_duration: usize,
    pub split: usize,         // 一条 GT 被多条 hyp 覆盖，多出的条数
    pub merged: usize,        // 一条 hyp 覆盖多条 GT，多出的条数
    pub iou_mean: f64,
    pub start_delta_mean: f64,
    pub start_delta_median: f64,
    pub start_delta_p95_abs: f64,
    pub end_delta_mean: f64,
    pub end_delta_median: f64,
    pub end_delta_p95_abs: f64,
    /// 按 GT 字符数加权的逐条 CER（配错行时会明显高于拼串 CER）
    pub paired_cer: f64,
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn median(v: &mut Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// 绝对值的 P95。
fn p95_abs(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut a: Vec<f64> = v.iter().map(|x| x.abs()).collect();
    a.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (((a.len() as f64) * 0.95).ceil() as usize).saturating_sub(1);
    a[idx.min(a.len() - 1)]
}

/// 按时间重叠最大配对 GT 与 hyp，统计偏移 / IoU / 漏虚检 / 逐条 CER。
///
/// 配对策略：对每条 GT 取 IoU 最大且 >0 的 hyp（贪心，按 IoU 降序占用，
/// 保证一对一）。未被占用的 GT 记为漏检，未被占用的 hyp 记为虚检。
/// split/merged 由「有重叠但未成为最佳配对」的关系数推出。
pub fn align_segments(gt: &[TimedText], hyp: &[TimedText]) -> AlignReport {
    // 收集所有有重叠的候选对，按 IoU 降序贪心配对
    let mut cands: Vec<(f64, usize, usize)> = Vec::new();
    for (gi, g) in gt.iter().enumerate() {
        for (hi, h) in hyp.iter().enumerate() {
            if related(g, h) {
                cands.push((temporal_iou(g, h), gi, hi));
            }
        }
    }
    cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut gt_used = vec![false; gt.len()];
    let mut hyp_used = vec![false; hyp.len()];
    let mut pairs: Vec<Pair> = Vec::new();
    for (iou, gi, hi) in &cands {
        if gt_used[*gi] || hyp_used[*hi] {
            continue;
        }
        gt_used[*gi] = true;
        hyp_used[*hi] = true;
        let g = &gt[*gi];
        let h = &hyp[*hi];
        pairs.push(Pair {
            gt_idx: *gi,
            hyp_idx: *hi,
            iou: *iou,
            start_delta: h.start as i64 - g.start as i64,
            end_delta: h.end as i64 - g.end as i64,
            cer: compute_cer(&normalize_for_cer(&g.text), &normalize_for_cer(&h.text)),
        });
    }
    pairs.sort_by_key(|p| p.gt_idx);

    let missed = gt_used.iter().filter(|u| !**u).count();
    let spurious = hyp_used.iter().filter(|u| !**u).count();

    // split：一条 GT 与多条 hyp 重叠，超出 1 的部分
    let mut split = 0usize;
    for g in gt.iter() {
        let n = hyp.iter().filter(|h| related(g, h)).count();
        if n > 1 {
            split += n - 1;
        }
    }
    // merged：一条 hyp 与多条 GT 重叠，超出 1 的部分
    let mut merged = 0usize;
    for h in hyp.iter() {
        let n = gt.iter().filter(|g| related(g, h)).count();
        if n > 1 {
            merged += n - 1;
        }
    }

    let ious: Vec<f64> = pairs.iter().map(|p| p.iou).collect();
    let sd: Vec<f64> = pairs.iter().map(|p| p.start_delta as f64).collect();
    let ed: Vec<f64> = pairs.iter().map(|p| p.end_delta as f64).collect();

    // 逐条 CER 按 GT 字符数加权；漏检的 GT 记为全错（CER=1）计入分母
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for p in &pairs {
        let w = normalize_for_cer(&gt[p.gt_idx].text).chars().count() as f64;
        num += p.cer * w;
        den += w;
    }
    for (gi, used) in gt_used.iter().enumerate() {
        if !*used {
            let w = normalize_for_cer(&gt[gi].text).chars().count() as f64;
            num += w; // CER=1
            den += w;
        }
    }

    let mut sd_m = sd.clone();
    let mut ed_m = ed.clone();
    AlignReport {
        missed,
        spurious,
        zero_duration: hyp.iter().filter(|h| h.end <= h.start).count(),
        split,
        merged,
        iou_mean: mean(&ious),
        start_delta_mean: mean(&sd),
        start_delta_median: median(&mut sd_m),
        start_delta_p95_abs: p95_abs(&sd),
        end_delta_mean: mean(&ed),
        end_delta_median: median(&mut ed_m),
        end_delta_p95_abs: p95_abs(&ed),
        paired_cer: if den > 0.0 { num / den } else { 0.0 },
        pairs,
    }
}

// ===========================================================================
// 帧合并（对齐 ocrMerge.ts mergeFrames，空 mergeFramesArgs）
// ===========================================================================

fn is_substring_of(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() || a.len() == b.len() {
        return false;
    }
    if a.len() < b.len() {
        b.contains(a)
    } else {
        a.contains(b)
    }
}

fn avg_confidence(confs: &[f64]) -> Option<f64> {
    if confs.is_empty() {
        None
    } else {
        Some(confs.iter().sum::<f64>() / confs.len() as f64)
    }
}

fn merge_confidence(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(x), Some(y)) => Some((x + y) / 2.0),
    }
}

fn normalize_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 把 `Option<(top, bottom)>` 纵向范围转成 `[min_y, max_y]`（无字幕 → `[0,0]`）。
fn bbox_to_range(bbox: Option<(f32, f32)>) -> [f32; 2] {
    match bbox {
        Some((mn, mx)) => [mn, mx],
        None => [0.0, 0.0],
    }
}

/// 合并相邻帧为字幕段（对齐 ocrMerge.ts mergeFrames，mergeFramesArgs={}）。
/// 注意：cpp 路径下帧不带 bbox（对齐 LocalDub ocrFrameCpp 丢弃 box 的行为），
/// 因此依赖 box_y 的 substring merge / A-B-C triplet 分支不会触发。
pub fn merge_frames(frames: &[FrameResult]) -> (String, Vec<Segment>) {
    let mut segments: Vec<Segment> = Vec::new();
    let mut current_text = String::new();
    let mut current_start: u64 = 0;
    let mut current_y_range: [f32; 2] = [0.0, 0.0];
    let mut gap_start: u64 = 0;
    let mut current_confidences: Vec<f64> = Vec::new();
    let mut current_end: u64 = 0;

    let dedup_levenshtein = 1usize;

    for f in frames {
        // A: 空帧 → 标记 gap
        if f.text.is_empty() {
            if !current_text.is_empty() && gap_start == 0 {
                gap_start = f.timestamp;
            }
            continue;
        }
        // B: gap 恢复检查
        if gap_start > 0 {
            let gap_ms = f.timestamp.saturating_sub(gap_start);
            if gap_ms <= 1500
                && (normalize_ws(&f.text) == normalize_ws(&current_text)
                    || is_substring_of(&f.text, &current_text)
                    || is_substring_of(&current_text, &f.text))
            {
                current_confidences.push(f.confidence);
                current_end = f.timestamp;
                gap_start = 0;
                continue;
            }
            // B2: flush
            segments.push(Segment {
                text: std::mem::take(&mut current_text),
                start: current_start,
                end: gap_start,
                x_range: [0.0, 0.0],
                y_range: current_y_range,
                confidence: avg_confidence(&current_confidences),
            });
            current_text.clear();
            current_start = 0;
            current_y_range = [0.0, 0.0];
            gap_start = 0;
            current_confidences.clear();
        }
        // C: text 比较
        if current_text.is_empty() || normalize_ws(&f.text) != normalize_ws(&current_text) {
            if !current_text.is_empty() {
                segments.push(Segment {
                    text: std::mem::take(&mut current_text),
                    start: current_start,
                    end: current_end,
                    x_range: [0.0, 0.0],
                    y_range: current_y_range,
                    confidence: avg_confidence(&current_confidences),
                });
            }
            current_text = f.text.clone();
            current_start = f.timestamp;
            current_end = f.timestamp;
            current_y_range = bbox_to_range(f.bbox);
            current_confidences = vec![f.confidence];
        } else {
            current_confidences.push(f.confidence);
            current_end = f.timestamp;
            // 并入：y 值域取并集。
            let r = bbox_to_range(f.bbox);
            current_y_range = [
                current_y_range[0].min(r[0]),
                current_y_range[1].max(r[1]),
            ];
        }
    }
    // D: flush 最后一段
    if !current_text.is_empty() {
        let last_ts = if gap_start > 0 { gap_start } else { current_end };
        segments.push(Segment {
            text: current_text,
            start: current_start,
            end: last_ts,
            x_range: [0.0, 0.0],
            y_range: current_y_range,
            confidence: avg_confidence(&current_confidences),
        });
    }

    // Pass 1 substring merge: 仅当 mergeSubstring 时执行；空 args → 跳过。

    // Pass 2 A-B-C triplet: 依赖 box_y overlap（cpp 无 box → 全跳过）。

    // Pass 3 overlapping dedup（纯文本 levenshtein ≤ dedupLevenshtein）
    dedup_overlap(&mut segments, dedup_levenshtein);

    // Pass 4 同 text 相邻合并（不依赖 box）
    let mut i = segments.len();
    while i > 1 {
        i -= 1;
        let prev = &segments[i - 1];
        let cur = &segments[i];
        if normalize_ws(&prev.text) != normalize_ws(&cur.text) {
            continue;
        }
        let gap = if cur.start > prev.end {
            cur.start - prev.end
        } else {
            // 重叠或负 gap
            if cur.start < prev.end {
                continue;
            }
            0
        };
        if gap > 2000 {
            continue;
        }
        // 合并 cur 进 prev
        let merged_conf = merge_confidence(prev.confidence, cur.confidence);
        segments[i - 1].end = cur.end;
        segments[i - 1].confidence = merged_conf;
        segments.remove(i);
    }

    let text = segments
        .iter()
        .map(|s| s.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    (text, segments)
}

fn dedup_overlap(segments: &mut Vec<Segment>, dedup_levenshtein: usize) {
    let touch_gap_ms = 500u64;
    let mut i = 0;
    while i < segments.len() {
        let mut j = i + 1;
        while j < segments.len() {
            let (a, b) = (&segments[i], &segments[j]);
            let gap = (a.start.max(b.start) as i64 - a.end.min(b.end) as i64).max(0) as u64;
            let overlap = a.start < b.end && b.start < a.end;
            let touching = gap <= touch_gap_ms;
            if (overlap || touching) && levenshtein(&a.text, &b.text) <= dedup_levenshtein {
                let new_text = if a.text.chars().count() >= b.text.chars().count() {
                    a.text.clone()
                } else {
                    b.text.clone()
                };
                let merged_conf = merge_confidence(a.confidence, b.confidence);
                let new_start = a.start.min(b.start);
                let new_end = a.end.max(b.end);
                let new_x = [
                    a.x_range[0].min(b.x_range[0]),
                    a.x_range[1].max(b.x_range[1]),
                ];
                let new_y = [
                    a.y_range[0].min(b.y_range[0]),
                    a.y_range[1].max(b.y_range[1]),
                ];
                segments[i].text = new_text;
                segments[i].start = new_start;
                segments[i].end = new_end;
                segments[i].x_range = new_x;
                segments[i].y_range = new_y;
                segments[i].confidence = merged_conf;
                segments.remove(j);
                j -= 1;
            }
            j += 1;
        }
        i += 1;
    }
}

// ===========================================================================
// 抽帧（对齐 benchmark-ocr-video.ts extractFrames）
// ===========================================================================

/// 用 ffprobe 读视频时长（秒）与源帧率。
pub fn probe_video(video: &Path) -> (f64, f64) {
    let dur = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
            video.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe 失败");
    let duration = String::from_utf8_lossy(&dur.stdout).trim().parse::<f64>().unwrap_or(0.0);

    let fr = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate",
            "-of",
            "csv=p=0",
            video.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe 帧率失败");
    let fr_str = String::from_utf8_lossy(&fr.stdout).trim().to_string();
    let parts: Vec<&str> = fr_str.split('/').collect();
    let src_fps = if parts.len() == 2 {
        let num = parts[0].parse::<f64>().unwrap_or(30.0);
        let den = parts[1].parse::<f64>().unwrap_or(1.0);
        if den > 0.0 {
            num / den
        } else {
            30.0
        }
    } else {
        30.0
    };
    (duration, src_fps)
}

/// 抽帧到 out_dir/frame_%05d.jpg，返回 (时长秒, step, 源帧率)。
pub fn extract_frames(video: &Path, out_dir: &Path, fps: f64) -> (f64, u64, f64) {
    std::fs::create_dir_all(out_dir).unwrap();
    let (duration, src_fps) = probe_video(video);
    let step = (src_fps / fps).round().max(1.0) as u64;
    let out_pattern = out_dir.join("frame_%05d.jpg");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            video.to_str().unwrap(),
            "-vf",
            &format!("select='not(mod(n,{}))'", step),
            // ffmpeg 8 移除了 -vsync（deprecated 自 5.x），等价新旗标为 -fps_mode vfr。
            "-fps_mode",
            "vfr",
            "-qscale:v",
            "2",
            out_pattern.to_str().unwrap(),
        ])
        .status()
        .expect("ffmpeg 抽帧失败");
    assert!(status.success(), "ffmpeg 抽帧退出非 0");
    (duration, step, src_fps)
}

/// 列出抽帧目录里的 .jpg 文件（按名排序，对齐 listFrameFiles）。
pub fn list_frame_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map(|x| x.eq_ignore_ascii_case("jpg"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

// ============================================================================
// 共享的 CER / summary 生成（供 sf_ocr / bench 复用）。
// ============================================================================

/// CER 结果（raw + norm + 字符数）。
pub struct CerResult {
    pub raw: f64,
    pub norm: f64,
    pub hyp_chars: usize,
    pub ref_chars: usize,
}

/// 读 ocr.json / GT 的 `result.segments` → 去空白 join 文本（用于 CER）。
pub fn load_segments_text(path: &Path) -> Vec<String> {
    let content = std::fs::read_to_string(path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    v.get("result")
        .and_then(|r| r.get("segments"))
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
                .map(|t| t.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// 读 ocr.json / GT 的 `result.segments` → 带时间轴的 TimedText 列表（用于对齐）。
pub fn load_timed_segments(path: &Path) -> Vec<TimedText> {
    let content = std::fs::read_to_string(path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    v.get("result")
        .and_then(|r| r.get("segments"))
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    Some(TimedText {
                        text: s.get("text").and_then(|t| t.as_str())?.trim().to_string(),
                        start: s.get("start").and_then(|t| t.as_u64())?,
                        end: s.get("end").and_then(|t| t.as_u64())?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 对齐 eval-ocr.ts main()：读 hyp/GT 的 result.segments，join 文本，算 raw + norm CER。
pub fn eval_cer(hyp_path: &Path, gt_path: &Path) -> CerResult {
    let gt_segs = load_segments_text(gt_path);
    let hyp_segs = load_segments_text(hyp_path);
    let gt_full: String = gt_segs.concat();
    let hyp_full: String = hyp_segs.concat();

    let raw = compute_cer(&strip_ws(&gt_full), &strip_ws(&hyp_full));
    let norm = compute_cer(&normalize_for_cer(&gt_full), &normalize_for_cer(&hyp_full));

    CerResult {
        raw,
        norm,
        hyp_chars: strip_ws(&hyp_full).chars().count(),
        ref_chars: strip_ws(&gt_full).chars().count(),
    }
}
