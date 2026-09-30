//! 键盘键名 → Linux 输入子系统 keycode 映射。
//!
//! **重要**：ydotool 的 `key` 子命令只认**数字 keycode**（`strtol` 解析），直接传
//! `KEY_ENTER` 这类名字会被当成 0（KEY_RESERVED）静默失效。所以本模块内置这张表，
//! 让 `key("KEY_S")` / `combo(&["KEY_LEFTCTRL","KEY_S"])` 这种直观写法能正确翻译成
//! 数字码再透传给 ydotool。

use anyhow::{Context, Result};
use phf::phf_map;

/// 把键名/数字码翻译成 Linux keycode（数字）。
///
/// 接受两种写法：
/// - `KEY_*` 键名（大小写不敏感），如 `"KEY_ENTER"`、`"key_a"`；
/// - 纯数字字符串（十进制或 `0x` 十六进制），如 `"31"`、`"0x1F"` 直接当 keycode。
pub fn keycode_of(name: &str) -> Result<u16> {
    // 纯数字（十进制或 0x 十六进制）直接当 keycode。
    if let Some(hex) = name.strip_prefix("0x").or_else(|| name.strip_prefix("0X")) {
        return u16::from_str_radix(hex, 16).with_context(|| format!("非法十六进制键码: {name}"));
    }
    if name.bytes().all(|b| b.is_ascii_digit()) {
        return name
            .parse::<u16>()
            .with_context(|| format!("非法十进制键码: {name}"));
    }
    KEYCODES
        .get(name.to_ascii_uppercase().as_str())
        .copied()
        .with_context(|| {
            format!(
                "未知键名: {name}（ydotool 只认数字 keycode；可用 KEY_* 名或数字，见 KEYCODES 表）"
            )
        })
}

/// 常用键名 → Linux keycode 映射。键名统一大写（查表时忽略大小写）。
///
/// 数据来源：`linux/input-event-codes.h`（KEY_* 宏值）。仅列常用键，覆盖
/// 字母/数字/修饰键/功能键/方向键/编辑键；特殊键按需补充。
pub static KEYCODES: phf::Map<&'static str, u16> = phf_map! {
    // 修饰键
    "KEY_LEFTCTRL" => 29, "KEY_RIGHTCTRL" => 97, "KEY_LEFTSHIFT" => 42,
    "KEY_RIGHTSHIFT" => 54, "KEY_LEFTALT" => 56, "KEY_RIGHTALT" => 100,
    "KEY_LEFTMETA" => 125, "KEY_RIGHTMETA" => 126,
    // 字母
    "KEY_A" => 30, "KEY_B" => 48, "KEY_C" => 46, "KEY_D" => 32, "KEY_E" => 18,
    "KEY_F" => 33, "KEY_G" => 34, "KEY_H" => 35, "KEY_I" => 23, "KEY_J" => 36,
    "KEY_K" => 37, "KEY_L" => 38, "KEY_M" => 50, "KEY_N" => 49, "KEY_O" => 24,
    "KEY_P" => 25, "KEY_Q" => 16, "KEY_R" => 19, "KEY_S" => 31, "KEY_T" => 20,
    "KEY_U" => 22, "KEY_V" => 47, "KEY_W" => 17, "KEY_X" => 45, "KEY_Y" => 21,
    "KEY_Z" => 44,
    // 数字（主键盘）
    "KEY_0" => 11, "KEY_1" => 2, "KEY_2" => 3, "KEY_3" => 4, "KEY_4" => 5,
    "KEY_5" => 6, "KEY_6" => 7, "KEY_7" => 8, "KEY_8" => 9, "KEY_9" => 10,
    // 功能键
    "KEY_F1" => 59, "KEY_F2" => 60, "KEY_F3" => 61, "KEY_F4" => 62, "KEY_F5" => 63,
    "KEY_F6" => 64, "KEY_F7" => 65, "KEY_F8" => 66, "KEY_F9" => 67, "KEY_F10" => 68,
    "KEY_F11" => 87, "KEY_F12" => 88,
    // 编辑 / 导航
    "KEY_ENTER" => 28, "KEY_ESC" => 1, "KEY_TAB" => 15, "KEY_SPACE" => 57,
    "KEY_BACKSPACE" => 14, "KEY_DELETE" => 111, "KEY_INSERT" => 110,
    "KEY_HOME" => 102, "KEY_END" => 107, "KEY_PAGEUP" => 104, "KEY_PAGEDOWN" => 109,
    "KEY_LEFT" => 105, "KEY_RIGHT" => 106, "KEY_UP" => 103, "KEY_DOWN" => 108,
    "KEY_CAPSLOCK" => 58, "KEY_NUMLOCK" => 69, "KEY_SCROLLLOCK" => 70,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_translation() {
        // KEY_* 名字 → 数字 keycode（与 input-event-codes.h 一致）。
        assert_eq!(keycode_of("KEY_ENTER").unwrap(), 28);
        assert_eq!(keycode_of("KEY_LEFTCTRL").unwrap(), 29);
        assert_eq!(keycode_of("KEY_S").unwrap(), 31);
        assert_eq!(keycode_of("key_a").unwrap(), 30); // 大小写不敏感
        // 纯数字直接当 keycode。
        assert_eq!(keycode_of("31").unwrap(), 31);
        assert_eq!(keycode_of("0x1F").unwrap(), 31);
        // 未知名字报错。
        assert!(keycode_of("KEY_NOPE").is_err());
    }
}
