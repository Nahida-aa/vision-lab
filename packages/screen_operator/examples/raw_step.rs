//! 原语探针：直接调用 `ScreenOperator::move_rel`（相对移一步，**不闭环**），
//! 用 KWin `cursor_pos` 读移动前后的逻辑坐标，验证「移动一次」注入原语本身是否生效。
//!
//! 与 `move_probe`（闭环 `ensure_move_to`）的区别：本例**故意绕过闭环**，只发一条相对
//! 移动指令，再事后读数。用于确认 ydotool 注入通道本身 OK、以及单次 `move_rel` 的
//! 实际落点偏移（ydotool 相对移动落点不稳定，单次不一定精确等于 `delta`）。
//!
//! 用法：
//! ```text
//! cargo run -p screen_operator --example raw_step -- 100 100
//! ```
//! 参数 `DX DY` 为相对当前光标的**逻辑像素**增量。

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
    if args.len() < 3 {
        anyhow::bail!("用法: raw_step <逻辑DX> <逻辑DY>");
    }
    let dx: i32 = args[1].parse().context("逻辑DX 非法")?;
    let dy: i32 = args[2].parse().context("逻辑DY 非法")?;
    let delta = IVec2::new(dx, dy);

    let fg = KdeForegrounder::new("testing_08");
    let before = fg
        .cursor_pos()
        .context("读移动前光标失败（确认 testing_08 在前台 / KWin 可读）")?;

    // 直接调用「移动一次」原语（不闭环、不读回确认）。
    let op = ScreenOperator::new();
    op.move_rel(delta)
        .context("move_rel 注入失败（确认 ydotoold 在运行）")?;

    // 等光标真正落盘后再读。
    std::thread::sleep(std::time::Duration::from_millis(110));
    let after = fg.cursor_pos().context("读移动后光标失败")?;

    let actual = after - before;
    println!(
        "[raw_step] 指令增量=({delta}), 实际偏移=({actual}), 误差=({}, {})",
        actual.x - delta.x,
        actual.y - delta.y
    );
    Ok(())
}
