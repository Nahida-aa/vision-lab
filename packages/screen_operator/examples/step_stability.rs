//! 原语稳定性探针：不同距离 × 多次，测 `move_rel`(相对移一步,不闭环)的落点误差。
//!
//! 每轮：
//!   1. `ensure_move_to(START)` 闭环回到起点（稳住每轮起点，避免误差累积叠加）；
//!   2. 读 `before = cursor_pos()`（真实起点，隔离 `ensure_move_to` 收尾 ≤2 的偏差）；
//!   3. `move_rel((dist, 0))` 在 x 方向移一步（**不闭环、不读回确认**）；
//!   4. 读 `after = cursor_pos()`，误差 = `after - before - (dist, 0)`（纯 `move_rel` 增量误差）。
//! 对每个距离循环 `--rounds` 次，把每轮原始记录（距离 / 轮次 / before / after / 误差）
//! 收集起来，按 `--format csv|json` 写到 `--out <文件>`（默认 csv 写到
//! `crates/screen_operator/docs/step_stability.csv`），方便后续读取分析；同时 stdout
//! 仍打印每距离的汇总（误差序列 / 跨度 / mean）。
//!
//! 用法：
//! ```text
//! cargo run -p screen_operator --example step_stability
//! cargo run -p screen_operator --example step_stability --rounds 8 --dists 960,970,980 --format json --out docs/step_stability_960_980.json
//! ```

use anyhow::{Context, Result};
use glam::IVec2;
use screen_operator::{KdeForegrounder, ScreenOperator};
use tracing_subscriber;

/// 起点：左边缘中点（逻辑坐标）。从 x=0 出发测大距离不越界（逻辑宽 1800）。
const START: IVec2 = IVec2::new(0, 562);

/// 待测距离集合（x 方向增量，逻辑像素）。
const DISTANCES: &[i32] = &[
    1, 2, 3, 4, 5, 7, 10, 14, 20, 30, 45, 60, 80, 100, 200, 350, 500, 600, 700, 750, 800, 850, 900,
    950, 960, 970, 980, 1000, 1200,
];

/// 单轮原始记录。
struct Record {
    dist: i32,
    round: usize,
    before: IVec2,
    after: IVec2,
    err: IVec2,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().collect();
    let rounds = args
        .iter()
        .position(|a| a == "--rounds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(8);

    // 支持 --dists 1,2,3 指定部分距离（分段跑，跑过的记下来不重跑）。
    let dists: Vec<i32> = args
        .iter()
        .position(|a| a == "--dists")
        .and_then(|i| args.get(i + 1))
        .map(|s| {
            s.split(',')
                .filter_map(|x| x.trim().parse::<i32>().ok())
                .collect()
        })
        .unwrap_or_else(|| DISTANCES.to_vec());

    // 输出格式与路径。
    let format = args
        .iter()
        .position(|a| a == "--format")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
        .unwrap_or("csv");
    let out_path = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "crates/screen_operator/docs/step_stability.csv".to_string());

    let fg = KdeForegrounder::new("testing_08");
    let op = ScreenOperator::new().with_foregrounder(fg.clone());

    let (sw, sh) = fg.screen_logical_size()?;
    println!(
        "[step_stability] 屏幕逻辑尺寸=({sw}, {sh}), 起点={START}, 距离数={}, 每距离轮数={rounds}",
        dists.len()
    );

    let mut records: Vec<Record> = Vec::new();

    for &dist in &dists {
        let mut x_errs: Vec<i32> = Vec::with_capacity(rounds);
        let mut y_errs: Vec<i32> = Vec::with_capacity(rounds);
        for r in 0..rounds {
            // 1. 闭环回起点（稳住每轮起点）。
            op.ensure_move_to(START)?;
            std::thread::sleep(std::time::Duration::from_millis(110));
            // 2. 读真实起点（隔离 ensure_move_to 收尾偏差）。
            let before = fg.cursor_pos()?;

            // 3. 原语移一步（不闭环）。
            op.move_rel(IVec2::new(dist, 0))?;
            std::thread::sleep(std::time::Duration::from_millis(110));
            // 4. 读落点。
            let after = fg.cursor_pos()?;

            // 纯 move_rel 增量误差。
            let err = after - before - IVec2::new(dist, 0);
            x_errs.push(err.x);
            y_errs.push(err.y);
            records.push(Record {
                dist,
                round: r,
                before,
                after,
                err,
            });
        }
        let (xmin, xmax) = (
            x_errs.iter().min().copied().unwrap(),
            x_errs.iter().max().copied().unwrap(),
        );
        let xmean = x_errs.iter().sum::<i32>() / x_errs.len() as i32;
        let (ymin, ymax) = (
            y_errs.iter().min().copied().unwrap(),
            y_errs.iter().max().copied().unwrap(),
        );
        println!(
            "[step_stability] 距离={dist:4}  x误差={:?}  x跨度={} xmean={}  y误差=[{},{}]",
            x_errs,
            xmax - xmin,
            xmean,
            ymin,
            ymax
        );
    }

    // 写文件（csv / json）。
    let out = match format {
        "json" => to_json(&records),
        _ => to_csv(&records),
    };
    std::fs::write(&out_path, out).with_context(|| format!("写结果文件失败: {out_path}"))?;
    println!(
        "[step_stability] 已写 {format} 结果 -> {out_path}（共 {} 行）",
        records.len()
    );

    Ok(())
}

/// 写 CSV：表头 + 每轮一行。
fn to_csv(rs: &[Record]) -> String {
    let mut s = String::from("dist,round,before_x,before_y,after_x,after_y,err_x,err_y\n");
    for r in rs {
        s.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            r.dist, r.round, r.before.x, r.before.y, r.after.x, r.after.y, r.err.x, r.err.y
        ));
    }
    s
}

/// 写 JSON：数组 of 对象（手写，不引入额外依赖）。
fn to_json(rs: &[Record]) -> String {
    let mut s = String::from("[\n");
    for (i, r) in rs.iter().enumerate() {
        s.push_str(&format!(
            "  {{\"dist\":{},\"round\":{},\"before\":[{},{}],\"after\":[{},{}],\"err\":[{},{}]}}{}\n",
            r.dist,
            r.round,
            r.before.x,
            r.before.y,
            r.after.x,
            r.after.y,
            r.err.x,
            r.err.y,
            if i + 1 < rs.len() { "," } else { "" }
        ));
    }
    s.push(']');
    s
}
