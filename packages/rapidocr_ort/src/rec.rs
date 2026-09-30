//! 识别分支的 CTC 贪婪解码。
//!
//! 对齐 subtitle-rust 的 `ctc_decode`：取每个时间步 argmax，折叠连续重复 +
//! 去 blank（索引 0）。关键修正：**遇到 blank 时把 prev 重置为 -1**，这样
//! 两个相同字符之间只要隔着一个 blank 就能都被保留（之前我的代码跨 blank
//! 保留 prev，会把 "oo" 的第二份丢掉）。

/// 对 rec 输出的 logits 做 CTC 贪婪解码。
///
/// `logits` 为扁平数据，`shape` 为 `[1, T, C]` 或 `[1, C, T]`（最后一维是
/// 字符类，倒数第二维是时间步）。返回 `(文本, 平均字符置信度)`。
pub fn ctc_greedy_decode(logits: &[f32], shape: &[usize], vocab: &[String]) -> (String, f32) {
    if shape.len() < 2 {
        return (String::new(), 0.0);
    }
    // 时间轴 = 倒数第二维，字符轴 = 最后一维。
    let timesteps = shape[shape.len() - 2];
    let num_classes = shape[shape.len() - 1];
    if timesteps * num_classes > logits.len() {
        return (String::new(), 0.0);
    }

    let mut chars = String::new();
    let mut confs: Vec<f32> = Vec::with_capacity(timesteps);
    let mut prev: i32 = -1; // -1 等价于 blank，可与索引 0 合并
    for t in 0..timesteps {
        let row = &logits[t * num_classes..(t + 1) * num_classes];
        let mut max_idx = 0usize;
        let mut max_val = row[0];
        for (i, &v) in row.iter().enumerate() {
            if v > max_val {
                max_val = v;
                max_idx = i;
            }
        }
        if max_idx == 0 {
            // blank：重置 prev，允许后续重复字符出现
            prev = -1;
            continue;
        }
        if (max_idx as i32) != prev {
            if max_idx < vocab.len() {
                chars.push_str(&vocab[max_idx]);
                confs.push(max_val);
            }
        }
        prev = max_idx as i32;
    }
    let avg = if confs.is_empty() {
        0.0
    } else {
        confs.iter().sum::<f32>() / confs.len() as f32
    };
    (chars, avg)
}
