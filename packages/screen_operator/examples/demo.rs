//! `screen_operator` 演示：在屏幕绝对坐标上模拟人的操作。
//!
//! 用法：
//!   cargo run -p screen_operator --example demo -- move 100 100
//!   cargo run -p screen_operator --example demo -- click 100 100
//!   cargo run -p screen_operator --example demo -- double 100 100
//!   cargo run -p screen_operator --example demo -- drag 100 100 400 400
//!   cargo run -p screen_operator --example demo -- type "hello world"
//!   cargo run -p screen_operator --example demo -- key KEY_F5
//!
//! 前置：ydotool 已安装且 `ydotoold` 在运行
//! （`systemctl --user enable --now ydotool.service`）。

use glam::IVec2;
use screen_operator::{MouseButton, ScreenOperator};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        anyhow::bail!(
            "用法: demo <move|click|double|drag|type|key|combo> [坐标/文本]\n\
             \x20  move  x y           移动到绝对坐标\n\
             \x20  click x y          左键单击\n\
             \x20  right x y          右键单击\n\
             \x20  double x y         左键双击\n\
             \x20  drag x1 y1 x2 y2    拖拽\n\
             \x20  type  text         键入文本\n\
             \x20  key   NAME         按一次键（如 KEY_F5 / KEY_ENTER）\n\
             \x20  combo K1 K2 ...    组合键：依次按下各键再依次抬起\n\
             \x20                      例：combo KEY_LEFTCTRL KEY_S  = Ctrl+S"
        );
    }
    let op = &args[1];
    let op_args = &args[2..];
    let operator = ScreenOperator::new();

    match op.as_str() {
        "move" => {
            let pos = parse_xy(op_args)?;
            operator.ensure_move_to(pos)?;
            println!("已移动到 ({pos})");
        }
        "click" => {
            let pos = parse_xy(op_args)?;
            operator.click_left_at(pos)?;
            println!("已左键单击 ({pos})");
        }
        "right" => {
            let pos = parse_xy(op_args)?;
            operator.click_at(pos, MouseButton::Right)?;
            println!("已右键单击 ({pos})");
        }
        "double" => {
            let pos = parse_xy(op_args)?;
            operator.double_click_at(pos, MouseButton::Left)?;
            println!("已左键双击 ({pos})");
        }
        "drag" => {
            if op_args.len() < 4 {
                anyhow::bail!("drag 需要 4 个参数: x1 y1 x2 y2");
            }
            let from = IVec2::new(op_args[0].parse()?, op_args[1].parse()?);
            let to = IVec2::new(op_args[2].parse()?, op_args[3].parse()?);
            operator.drag(from, to, MouseButton::Left)?;
            println!("已从 {:?} 拖拽到 {:?}", from, to);
        }
        "type" => {
            let text = op_args.join(" ");
            operator.type_text(&text)?;
            println!("已键入: {text:?}");
        }
        "key" => {
            let name = op_args
                .first()
                .ok_or_else(|| anyhow::anyhow!("key 需要键名，如 KEY_F5"))?;
            operator.key(name)?;
            println!("已按键: {name}");
        }
        "combo" => {
            if op_args.is_empty() {
                anyhow::bail!("combo 需要至少一个键名，如 combo KEY_LEFTCTRL KEY_S");
            }
            let keys: Vec<&str> = op_args.iter().map(|s| s.as_str()).collect();
            operator.combo(&keys)?;
            println!("已发送组合键: {}", op_args.join("+"));
        }
        other => anyhow::bail!("未知操作: {other}"),
    }
    Ok(())
}

fn parse_xy(args: &[String]) -> anyhow::Result<IVec2> {
    if args.len() < 2 {
        anyhow::bail!("需要 2 个坐标参数: x y");
    }
    let xy = IVec2::new(args[0].parse()?, args[1].parse()?);
    Ok(xy)
}
