//! 移动探针：把光标闭环移动到指定**物理**坐标，打印每步（RUST_LOG=screen_operator=debug）。
//!
//! 用途：隔离验证「看 / 操作分离」里**操作侧的相对移动闭环**是否收敛——不依赖 OCR /
//! 录屏，直接给物理绝对坐标，靠 `KdeForegrounder::cursor_pos`（逻辑坐标，经本机
//! 修复后稳定）做闭环。用于排查闭环是否过冲 / 永不收敛。
//!
//! 用法：
//! ```text
//! RUST_LOG=screen_operator=debug cargo run -p screen_operator --example move_probe -- 0 562
//! RUST_LOG=screen_operator=debug cargo run -p screen_operator --example move_probe -- --center
//! ```
//! 坐标语义：**KWin 逻辑坐标**（与 `cursor_pos` / `screen_logical_size` 同套，本机
//! 1800×1125）。`ScreenOperator` 的移动/点击入口统一收逻辑坐标，`KdeForegrounder::
//! cursor_pos` 读的也是逻辑坐标，闭环全程逻辑、无需 scale 换算。
//!
//! 本 example 完全属于 `screen_operator`（操作侧），不依赖 `ocr_agent` 业务层：
//! 闭环所需的「读光标」由同 crate 的 `KdeForegrounder` 提供。

use anyhow::{Context, Result};
use glam::IVec2;
use screen_operator::{KdeForegrounder, ScreenOperator};
use tracing_subscriber;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().collect();

    // 可选 --step-cap N：覆盖 ScreenOperator 单步上限（默认 200）。仅用于对比/调试。
    let step_cap = args
        .iter()
        .position(|a| a == "--step-cap")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(200);

    // 可选 --tolerance N：覆盖 ScreenOperator 到达容差（默认 2）。仅用于对比/调试。
    let tolerance = args
        .iter()
        .position(|a| a == "--tolerance")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(2);

    let fg = KdeForegrounder::new("testing_08");

    // 目标逻辑坐标：--center 时由屏幕逻辑尺寸动态算（换分辨率也正确）；
    // 否则接收两个逻辑坐标参数。
    let a_pos = if args.iter().any(|a| a == "--center") {
        let (sw, sh) = fg.screen_logical_size()?;
        // 逻辑中心 = (宽/2 取整, 高/2 取整)。
        IVec2::new(sw / 2, sh / 2)
    } else if args.len() >= 3 {
        // 坐标语义：KWin 逻辑坐标（与 cursor_pos / screen_logical_size 同套，本机 1800×1125）。
        let ax: i32 = args[1].parse().context("逻辑x 非法")?;
        let ay: i32 = args[2].parse().context("逻辑y 非法")?;
        IVec2::new(ax, ay)
    } else {
        anyhow::bail!("用法: move_probe <逻辑x> <逻辑y>  |  move_probe --center");
    };

    println!(
        "[move_probe] 目标逻辑=({}), 屏幕逻辑尺寸={:?}",
        a_pos,
        fg.screen_logical_size()?
    );

    // with_foregrounder 让 ScreenOperator 自己用 fg.cursor_pos() 跑闭环，无需外部闭包。
    // 入口坐标是逻辑坐标，与 KWin 读数同套，不需要 scale 换算。
    let op = ScreenOperator::new()
        .with_foregrounder(fg)
        .with_step_cap(step_cap)
        .with_tolerance(tolerance);
    op.ensure_move_to(a_pos).context("移动失败")?;

    // 收尾读一次确认落点（重新构造，fg 已移入 ScreenOperator）。
    if let Ok(p_pos) = KdeForegrounder::new("testing_08").cursor_pos() {
        println!(
            "[move_probe] 结束: KWin读逻辑=({p_pos}), 目标逻辑=({a_pos}), 偏差=({})",
            p_pos - a_pos,
        );
    }
    Ok(())
}
